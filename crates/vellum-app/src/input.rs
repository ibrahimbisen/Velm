//! Pointer, wheel and gesture handling: what the hands do, and what the camera does
//! about it.
//!
//! # Bindings are Miro's, deliberately
//!
//! `docs/features/README.md` §9 is explicit that muscle memory has to transfer, so
//! the bindings here are Miro's rather than a fresh design:
//!
//! | Gesture | Does |
//! |---|---|
//! | Left press on an item | **Select it**, and arm a move |
//! | Left drag from an item | **Move the selection** |
//! | Left drag on empty canvas | **Marquee select** — never pan |
//! | Double click on an item | Select it and open its text |
//! | Space + drag, middle drag, right drag, `H` then drag | Pan |
//! | Two-finger trackpad scroll | Pan, content tracking the fingers 1:1 |
//! | Mouse wheel | **Zoom about the pointer** — see [`InputConfig::wheel_zooms`] |
//! | Pinch, or ⌘/Ctrl + scroll | Zoom about the pointer |
//! | Shift + wheel | Pan horizontally |
//!
//! An earlier build panned on left-drag. That is the single most disorienting thing
//! a canvas app can get wrong — every selection attempt moves the board instead —
//! and it is why this module exists as its own testable state machine rather than as
//! a `match` inside the event loop.
//!
//! # One coordinate space, and it is physical pixels
//!
//! Every screen-space value that reaches [`Camera`] is in **physical** pixels:
//! winit's `CursorMoved` is physical, its macOS `PixelDelta` is physical (winit
//! multiplies AppKit's point deltas by the scale factor before emitting them), and
//! [`Camera::viewport`] is configured from the surface, which is physical. A mixture
//! would put the zoom anchor at half or double the cursor's real position, and the
//! board would slide out from under the pointer a little more on every notch.
//! [`Input::scale_factor`] exists **only** to keep rates — how fast a wheel notch
//! zooms — the same on a Retina display as on a 1× one; it never touches a position.
//!
//! # Why the zoom mapping is exponential
//!
//! `zoom *= exp(k · pixels)`. Composition is then addition in the exponent, so
//! zooming in by *n* pixels and back out by *n* returns to the starting scale
//! exactly, and a notch feels the same at 5% as at 500%. `(1.0 + delta)`, which is
//! the obvious thing to write for a pinch, has neither property: it is asymmetric,
//! and a large delta can drive the factor to zero or negative.
//!
//! # Inertia
//!
//! macOS emits its own momentum stream after a two-finger scroll — those arrive as
//! ordinary `MouseWheel` events with a momentum phase — so applying inertia to
//! scrolling *there* would double it. Inertia is therefore per-source, and both
//! defaults follow from the *device* rather than the platform:
//!
//! - **Drag inertia is off.** A button-drag pan is a mouse gesture, and a mouse has
//!   no momentum to model — a canvas that keeps sliding after the button is up reads
//!   as the app ignoring you.
//! - **Scroll inertia is off on macOS**, where the window server sends its own
//!   momentum, and on elsewhere, where nothing does.
//!
//! See [`InputConfig::drag_inertia`] and [`InputConfig::scroll_inertia`].

use std::time::{Duration, Instant};

use vellum_scene::{Camera, ScreenPoint};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::CursorIcon;

/// One wheel notch, in logical pixels. Wheels report lines and trackpads report
/// pixels; 40 is the figure browsers settled on, and it puts a notch in the same
/// range as the pixel deltas a trackpad produces.
pub const PIXELS_PER_WHEEL_LINE: f64 = 40.0;

/// Zoom exponent per logical pixel of ⌘/Ctrl-scroll. At the default sensitivity a
/// 40 px notch is a 22% step, which is close enough to Miro's that a user coming
/// from it does not overshoot.
///
/// This is the **continuous** rate, and it applies to a trackpad or a ⌘-held pixel
/// scroll only. A mouse wheel takes [`ZOOM_PER_WHEEL_NOTCH`] instead.
pub const ZOOM_RATE_PER_PIXEL: f64 = 0.005;

/// What one notch of the **mouse wheel** multiplies the zoom by. *"when zooming in with
/// the mouse it should go in 10 percent increments"*.
///
/// Geometric rather than additive, and that is the whole of the decision. Ten
/// *percentage points* a notch is unusable at both ends of the range: a board fitted at
/// 4% would jump to 14% on the first notch — two and a half times bigger — while at 400%
/// a notch would be a rounding error. Multiplying by 1.1 is the same 10% wherever you
/// are, which is what makes a wheel predictable, and it matches `⌘+`/`⌘−`, which have
/// always stepped geometrically (`actions::ZOOM_STEP`, ×1.15).
///
/// A wheel is the one input that reports **discrete notches** — that is what
/// `MouseScrollDelta::LineDelta` means — so it is the only one that can step at all. A
/// trackpad reports a dense stream of pixels and stays continuous; quantising that would
/// make a smooth two-finger zoom stutter.
pub const ZOOM_PER_WHEEL_NOTCH: f64 = 1.1;

/// Zoom exponent per unit of trackpad magnification.
///
/// macOS reports a pinch as a fraction of the current scale per event, so the naive
/// mapping is `1 + delta`. Feeding it through the same exponential the wheel uses
/// costs nothing, makes pinching in and back out exact, and — because the events
/// arrive in a dense stream — needs damping below 1.0 or a two-finger flick crosses
/// the whole zoom range. 0.9 was chosen against the trackpad, not derived.
pub const PINCH_ZOOM_RATE: f64 = 0.9;

/// Time constant of the velocity estimate that feeds inertia. Short enough to follow
/// a flick, long enough that one stuttering frame does not define the throw.
const VELOCITY_TAU: f64 = 0.05;

/// Time constant of the inertial glide. A throw travels roughly `velocity × TAU`
/// before stopping.
const INERTIA_TAU: f64 = 0.16;

/// Speed, in physical px/s, below which a glide is over. Above zero so the camera
/// actually comes to rest instead of asymptotically creeping.
const INERTIA_CUTOFF: f64 = 24.0;

/// A pointer that moved less than this between press and release was a click, not a
/// marquee and not a move. In physical pixels, so it is about half a fingertip's
/// wobble on a Retina display.
///
/// One number for both the marquee and the move threshold, deliberately.
/// `docs/06-mouse-controls.md` §4 asks for "~3px"; five *physical* pixels is 2.5
/// points on the Retina display this is developed against, which is that figure. Two
/// separate constants would mean a drag that had begun to move an item could still
/// end as a click, which is the sort of near-miss that makes a canvas feel unreliable.
const CLICK_SLOP: f64 = 5.0;

/// Longest gap between two left clicks in the same place that still reads as a
/// double click. Matches the platform default closely enough that nobody has to
/// learn a new rhythm; winit reports no double-click event of its own, so this is
/// the only place it can be decided.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// How far apart two clicks may land and still be one double click, in physical
/// pixels. The same wobble allowance a single click gets.
const DOUBLE_CLICK_SLOP: f64 = CLICK_SLOP;

/// Longest gap between two motion samples that still counts as one continuous
/// gesture. Beyond it the hand has effectively stopped, and dividing by the gap is
/// the honest estimate rather than a floor that inflates a slow drift into a flick.
const MAX_MOTION_GAP: f64 = 0.5;

/// How soon after a scroll sequence ends a new one must start to be the *platform's*
/// momentum rather than the user's next gesture. macOS emits the two back to back,
/// within a frame; a human cannot lift and re-place two fingers that fast.
const MOMENTUM_HANDOVER: Duration = Duration::from_millis(80);

/// How the input maps onto the camera. Every field is something a user could
/// reasonably want to change, which is why they are here rather than as constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputConfig {
    /// Multiplies scroll and drag panning. 1.0 tracks the fingers exactly.
    pub pan_sensitivity: f64,
    /// Multiplies the zoom exponent, for both wheel and pinch.
    pub zoom_sensitivity: f64,
    /// Flips both scroll axes. For a user whose system "natural scrolling" setting
    /// disagrees with the platform default — the deltas arrive already flipped, and
    /// nothing but the person at the trackpad knows which way they want it.
    pub invert_pan: bool,
    /// Flips scroll- and pinch-zoom direction.
    pub invert_zoom: bool,
    /// Glide after a drag-pan is released.
    ///
    /// **Off by default**, because a button-drag pan is a *mouse* gesture — middle,
    /// right, or space+left — and a mouse has no physical momentum to model. A
    /// canvas that keeps sliding after the button is up reads as the app ignoring
    /// you. Inertia is a trackpad idiom, where the fingers leave the surface still
    /// moving; see [`Self::scroll_inertia`], which is where it belongs.
    ///
    /// Kept configurable rather than deleted: it is genuinely wanted once a drag can
    /// originate from a trackpad, which is deferred work (`docs/06-mouse-controls.md`).
    pub drag_inertia: bool,
    /// Whether a **mouse wheel** notch zooms rather than pans.
    ///
    /// On by default. A wheel has one axis and coarse notches, so panning with it is
    /// slow and can only move one direction at a time — which is exactly the
    /// complaint that prompted this. Zooming is what CAD and diagram tools bind it
    /// to, and `Shift` still pans horizontally when that is what is wanted.
    ///
    /// This deliberately does **not** change trackpad behaviour: a `PixelDelta`
    /// still pans, because two fingers moving on a surface should move the surface.
    /// The two devices are told apart by `LineDelta` vs `PixelDelta`, which is the
    /// only reliable signal winit gives.
    pub wheel_zooms: bool,
    /// Glide after a two-finger scroll stops.
    ///
    /// Off on macOS, where the window server sends its own momentum events and ours
    /// would compound with them. On everywhere else there is no such stream, so a
    /// trackpad pan stops dead without this.
    pub scroll_inertia: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            pan_sensitivity: 1.0,
            zoom_sensitivity: 1.0,
            invert_pan: false,
            invert_zoom: false,
            drag_inertia: false,
            wheel_zooms: true,
            scroll_inertia: !cfg!(target_os = "macos"),
        }
    }
}

/// Which tool the pointer is holding, in Miro's sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    /// `V` — select, marquee, drag items.
    #[default]
    Select,
    /// `H` — the hand: left-drag pans.
    Hand,
    /// Any tool whose click *creates* something — sticky, text, frame, shape, pen,
    /// connector, image.
    ///
    /// One member for all of them because this module only has to know that a left
    /// press must not select and must not marquee; *what* gets made is
    /// `crate::actions`' business. Before this existed, arming the sticky tool and
    /// dragging out a note — which is what Miro does — swept a selection rectangle
    /// instead, created nothing, and left the tool armed for the next stray click.
    Place,
}

/// What a pointer gesture is currently doing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Gesture {
    Idle,
    /// Dragging the canvas. `last` is the previous pointer position, so each move is
    /// a self-contained delta and the camera never has to be rewound.
    Pan { button: MouseButton, last: ScreenPoint },
    /// A left press whose meaning the application has not settled yet.
    ///
    /// Whether a drag from here moves an item or sweeps a rectangle depends on what
    /// is under the pointer, and this module deliberately does not know about the
    /// scene. So the press is reported as [`Intent::Press`] and the answer comes back
    /// through [`Input::resolve_press`], synchronously, before the next event.
    Pending { origin: ScreenPoint, additive: bool },
    /// Sweeping out a selection rectangle.
    Marquee { origin: ScreenPoint, current: ScreenPoint, additive: bool },
    /// Dragging the selection. `moved` records whether the threshold was ever
    /// crossed, so a press-and-release that wobbled is not an undo step.
    Move { origin: ScreenPoint, current: ScreenPoint, moved: bool },
    /// A placement tool sweeping out the new item's box.
    Place { origin: ScreenPoint, current: ScreenPoint },
}

/// What the application has to do about a gesture, beyond moving the camera.
///
/// The camera is mutated in place because every consumer wants that; selection is
/// returned because only the application knows what is selectable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Intent {
    /// Nothing but the camera moved.
    None,
    /// The left button went down on the canvas. The application picks whatever is
    /// under `at` — or clears the selection when there is nothing — and **must**
    /// answer with [`Input::resolve_press`], which is what decides whether the drag
    /// that may follow moves the selection or sweeps a rectangle.
    ///
    /// Picking happens on *press* rather than on release because that is the only
    /// order in which an item can be grabbed and dragged in one gesture.
    /// `additive` is shift- or ⌘-click: extend rather than replace.
    Press { at: ScreenPoint, additive: bool, double: bool },
    /// The selection is being dragged. Both points come from the same press, so the
    /// offset is absolute and cannot accumulate rounding over a long drag.
    Move { from: ScreenPoint, to: ScreenPoint },
    /// The drag ended. `moved` is false when the pointer never crossed the threshold,
    /// in which case there is nothing to commit and nothing to undo.
    MoveEnd { moved: bool },
    /// A marquee finished. Select everything it touches.
    Marquee { from: ScreenPoint, to: ScreenPoint, additive: bool },
    /// A placement tool was released. `at` is the press and `to` the release, which
    /// are the same point for a click and opposite corners of the new item's box for
    /// a drag.
    Place { at: ScreenPoint, to: ScreenPoint },
    /// One pointer sample during a placing drag. Emitted continuously so a freehand
    /// tool can accumulate its path; tools that only need the final rectangle ignore
    /// it. Reporting rather than buffering keeps `input` free of tool knowledge.
    PlaceSample { at: ScreenPoint },
    /// The right button came up without the pointer having moved.
    ///
    /// On *release*, not on press, because a right-drag pans — see [`Input::pans_with`].
    /// The application resolves what is under `at` and opens the menu for it, exactly
    /// as it resolves a left press: this module knows nothing about the scene.
    ContextMenu { at: ScreenPoint },
}

/// The pointer, the modifiers, and the gesture in flight.
#[derive(Debug)]
pub struct Input {
    config: InputConfig,
    modifiers: ModifiersState,
    /// Physical pixels. Starts at the viewport centre rather than `(0, 0)` so that
    /// a scroll-to-zoom before the pointer has ever moved anchors somewhere sane
    /// instead of on the top-left corner.
    cursor: ScreenPoint,
    cursor_known: bool,
    space_held: bool,
    tool: Tool,
    gesture: Gesture,
    /// How far the pointer has travelled since it went down, to tell a click from a
    /// marquee.
    press_travel: f64,
    velocity: (f64, f64),
    /// Set while a glide is running, cleared when it decays below the cutoff.
    gliding: bool,
    /// Whether the scroll sequence in flight is the platform's own momentum. If it
    /// is, the platform is already doing the gliding and ours would compound it.
    scroll_momentum: bool,
    /// When the last scroll sequence ended, which is how the next one is recognised
    /// as momentum rather than as a new gesture.
    scroll_ended_at: Option<Instant>,
    last_motion: Option<Instant>,
    /// When and where the last left click landed, so the next one can be recognised
    /// as the second half of a double click. winit reports no such event itself.
    last_click: Option<(Instant, ScreenPoint)>,
    /// How many presses in the current double-click run — 1 for a fresh click, 2 for a
    /// double, 3 for a triple. Read by [`Input::click_run`].
    clicks: u32,
    /// Physical pixels per logical pixel. Rates only — never positions.
    scale_factor: f64,
}

impl Default for Input {
    fn default() -> Self {
        Self::new(InputConfig::default())
    }
}

impl Input {
    pub fn new(config: InputConfig) -> Self {
        Self {
            config,
            modifiers: ModifiersState::empty(),
            cursor: ScreenPoint::new(0.0, 0.0),
            cursor_known: false,
            space_held: false,
            tool: Tool::Select,
            gesture: Gesture::Idle,
            press_travel: 0.0,
            velocity: (0.0, 0.0),
            gliding: false,
            scroll_momentum: false,
            scroll_ended_at: None,
            last_motion: None,
            last_click: None,
            clicks: 0,
            scale_factor: 1.0,
        }
    }

    pub fn config(&self) -> &InputConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut InputConfig {
        &mut self.config
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
    }

    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        if scale_factor.is_finite() && scale_factor > 0.0 {
            self.scale_factor = scale_factor;
        }
    }

    pub fn set_modifiers(&mut self, modifiers: ModifiersState) {
        self.modifiers = modifiers;
    }

    pub fn modifiers(&self) -> ModifiersState {
        self.modifiers
    }

    /// The pointer, in physical pixels. Falls back to the viewport centre until the
    /// pointer has been seen, so a gesture that arrives first still has an anchor.
    pub fn cursor(&self, camera: &Camera) -> ScreenPoint {
        if self.cursor_known {
            self.cursor
        } else {
            let viewport = camera.viewport();
            ScreenPoint::new(viewport.width / 2.0, viewport.height / 2.0)
        }
    }

    /// The marquee in flight, as two opposite corners in physical pixels.
    pub fn marquee(&self) -> Option<(ScreenPoint, ScreenPoint)> {
        match self.gesture {
            Gesture::Marquee { origin, current, .. } => Some((origin, current)),
            _ => None,
        }
    }

    /// The box a placement tool is sweeping out, as two opposite corners in physical
    /// pixels. `None` unless a create tool has the button down.
    pub fn placement(&self) -> Option<(ScreenPoint, ScreenPoint)> {
        match self.gesture {
            Gesture::Place { origin, current } => Some((origin, current)),
            _ => None,
        }
    }

    /// Whether the canvas is being dragged, so the caller can show a grabbing cursor.
    pub fn is_panning(&self) -> bool {
        matches!(self.gesture, Gesture::Pan { .. })
    }

    /// Whether a left gesture that touches the document is in flight — a move, a
    /// marquee, a placement, or a press not yet resolved. The chrome must not steal
    /// the release that ends any of them.
    pub fn is_gesturing(&self) -> bool {
        !matches!(self.gesture, Gesture::Idle | Gesture::Pan { .. })
    }

    /// Whether the selection is being dragged right now.
    pub fn is_moving(&self) -> bool {
        matches!(self.gesture, Gesture::Move { moved: true, .. })
    }

    /// Answers [`Intent::Press`]: `true` when the press landed on something the
    /// application is willing to move.
    ///
    /// Called synchronously from the press handler. A caller that forgets falls back
    /// to a marquee on the first motion, which is the safe half of the choice —
    /// sweeping a rectangle loses nothing, and silently moving an item the user did
    /// not grab loses work.
    pub fn resolve_press(&mut self, over_item: bool) {
        if let Gesture::Pending { origin, additive } = self.gesture {
            self.gesture = if over_item {
                Gesture::Move { origin, current: origin, moved: false }
            } else {
                Gesture::Marquee { origin, current: origin, additive }
            };
        }
    }

    /// Turns the press that is being resolved into a **placement** sweep.
    ///
    /// [`Self::resolve_press`]'s third answer, and it exists because a press can begin a
    /// placement without the *tool* being a placing one. A drag from one of Miro's four
    /// connector ports is the case: the palette says Select, the press arrives as
    /// [`Gesture::Pending`], and only the application knows a port was under it.
    ///
    /// # Why `set_tool(Tool::Place)` is not enough on its own
    ///
    /// It is what the application does next, and it decides what *future* presses mean —
    /// the gesture already in flight is untouched. So the press resolved to `Move` or
    /// `Marquee`, [`Self::placement`] answered `None` for the whole drag, and the live
    /// preview drew nothing until the button came up. Measured: *"mid-drag: armed true,
    /// previewing false"*, which is feedback 7, 23 and 34's shape a fourth time — state
    /// accumulated where the painter cannot see it — arriving through the one layer that is
    /// deliberately ignorant of the scene.
    ///
    /// Only ever converts a `Pending` press, so it cannot hijack a move or a marquee that
    /// is already under way.
    pub fn begin_placement(&mut self) {
        if let Gesture::Pending { origin, .. } = self.gesture {
            self.gesture = Gesture::Place { origin, current: origin };
        }
    }

    /// Abandons whatever left gesture is in flight, for Escape. Returns whether there
    /// was one, so the caller knows whether Escape has already been spent.
    pub fn cancel_gesture(&mut self) -> bool {
        if self.is_gesturing() {
            self.gesture = Gesture::Idle;
            true
        } else {
            false
        }
    }

    /// Whether a pan gesture *would* start on the next left press — space held or
    /// the hand tool chosen. Drives the cursor shape.
    pub fn pan_armed(&self) -> bool {
        self.space_held || self.tool == Tool::Hand
    }

    /// The pointer shape the current state calls for.
    ///
    /// The cursor is the only channel that tells someone what a click is *about* to
    /// do before they commit to it — that space is held, that they are over
    /// something draggable, that a drag will draw a marquee rather than pan. Without
    /// it every gesture is a guess, which is a large part of why the first build felt
    /// unpredictable even where its behaviour was correct.
    ///
    /// `over_item` is supplied by the caller because hit-testing needs the scene,
    /// which this module deliberately does not know about.
    /// The pointer's shape.
    ///
    /// `handle` is which resize or rotate grip the pointer is on, or is dragging — the
    /// app supplies it for the same reason it supplies `over_item`: this module
    /// deliberately knows nothing about the scene (trap 5), and "is there a handle here"
    /// is a question only the scene can answer.
    ///
    /// Without it every grip reported `Move`, because a resize *is* a `Gesture::Move`
    /// with a `DragMode::Resize` inside it and nothing here could see the difference.
    /// Reported as *"when i am trying to hold and drag to resize the cursor icon still
    /// shows a moving icon instead of that specific direction's expand collapse icon"*.
    pub fn cursor_icon(&self, handle: Option<crate::handle::Handle>) -> CursorIcon {
        // A grip outranks everything except a pan, which is a whole-canvas gesture and
        // says so. Checked before the gesture arms below because a resize in flight is a
        // `Gesture::Move` and would otherwise answer `Move` all the way through the drag.
        if let Some(handle) = handle
            && !matches!(self.gesture, Gesture::Pan { .. })
        {
            return handle_cursor(handle);
        }
        match self.gesture {
            // Mid-gesture the shape reports what is happening, never what could.
            Gesture::Pan { .. } => CursorIcon::Grabbing,
            Gesture::Marquee { .. } => CursorIcon::Crosshair,
            Gesture::Move { .. } => CursorIcon::Move,
            Gesture::Place { .. } => CursorIcon::Crosshair,
            _ if self.pan_armed() => CursorIcon::Grab,
            _ => match self.tool {
                Tool::Hand => CursorIcon::Grab,
                // Honest about the click landing at the intersection, which is what
                // `docs/06-mouse-controls.md` §4 asks a placement tool to say.
                Tool::Place => CursorIcon::Crosshair,
                // **The plain arrow over an item, not a move cursor.**
                //
                // Hovering used to answer `Move` — the four-way arrow — for anything
                // under the pointer, so the cursor changed constantly while reading a
                // board and said "you are moving this" about something nobody had
                // touched. *"why when i am over anything it shows the move cursor icon"*.
                // Figma, Sketch and Miro all keep the arrow on hover and change only once
                // a drag is under way, which the `Gesture::Move` arm above already does.
                //
                // The cursor now only ever reports what *is* happening — a drag, a pan, a
                // marquee, a grip — never what could.
                Tool::Select => CursorIcon::Default,
            },
        }
    }

    /// Records a key that changes what the pointer does. Returns whether it was one.
    ///
    /// Space is a *held* modifier in Miro rather than a toggle, so it is tracked here
    /// with the other modifiers instead of in the application's shortcut table.
    pub fn key(&mut self, key: &Key, state: ElementState) -> bool {
        let pressed = state == ElementState::Pressed;
        match key.as_ref() {
            Key::Named(NamedKey::Space) => {
                self.space_held = pressed;
                // Releasing space mid-drag must not strand the gesture: the button
                // is still down, but the canvas is no longer what it grabs.
                if !pressed
                    && self.tool != Tool::Hand
                    && matches!(self.gesture, Gesture::Pan { button: MouseButton::Left, .. })
                {
                    self.gesture = Gesture::Idle;
                }
                true
            }
            // Both tool keys defer to a held command key: ⌘V is paste and ⌘H hides
            // the app, and neither should silently swap the pointer's tool.
            Key::Character("h" | "H") if pressed && !self.command_held() => {
                self.tool = Tool::Hand;
                true
            }
            Key::Character("v" | "V") if pressed && !self.command_held() => {
                self.tool = Tool::Select;
                true
            }
            _ => false,
        }
    }

    /// Records where the pointer is without acting on the move.
    ///
    /// For the frames where the **chrome** owns the pointer. `crate::app` only calls
    /// [`Self::cursor_moved`] when egui has not consumed the event, which is right for
    /// panning and marquees — a drag must not continue under a panel — but it also meant
    /// [`Self::cursor`] silently stopped updating the moment the mouse crossed onto a
    /// toolbar, a tab strip or the properties panel. Anything that later asks "where is
    /// the pointer?" then got the last position on bare canvas, which is why a `⌘V` with
    /// the mouse resting over the chrome pasted somewhere the user had been rather than
    /// where they were.
    ///
    /// Deliberately does **not** touch the gesture state, the camera or the double-click
    /// timer: this is the position, and only the position.
    pub fn note_cursor(&mut self, position: ScreenPoint) {
        self.cursor = position;
        self.cursor_known = true;
    }

    /// The pointer moved. Pans, extends a marquee, or does nothing.
    pub fn cursor_moved(&mut self, camera: &mut Camera, position: ScreenPoint) -> Intent {
        self.cursor_moved_at(camera, position, Instant::now())
    }

    fn cursor_moved_at(
        &mut self,
        camera: &mut Camera,
        position: ScreenPoint,
        now: Instant,
    ) -> Intent {
        let previous = self.cursor;
        self.cursor = position;
        let first_sighting = !self.cursor_known;
        self.cursor_known = true;

        match self.gesture {
            // A pointer that entered the window mid-drag has no meaningful previous
            // position; treating the jump as a delta would fling the board.
            Gesture::Pan { .. } if first_sighting => Intent::None,
            Gesture::Pan { button, .. } => {
                let dx = position.x - previous.x;
                let dy = position.y - previous.y;
                self.press_travel += dx.hypot(dy);
                self.gesture = Gesture::Pan { button, last: position };
                self.pan(camera, dx, dy, now);
                Intent::None
            }
            Gesture::Marquee { origin, additive, .. } => {
                self.press_travel += (position.x - previous.x).hypot(position.y - previous.y);
                self.gesture = Gesture::Marquee { origin, current: position, additive };
                Intent::None
            }
            // The application never answered the press. Sweeping a rectangle is the
            // safe half of the choice; see `resolve_press`.
            Gesture::Pending { origin, additive } => {
                self.press_travel += (position.x - previous.x).hypot(position.y - previous.y);
                self.gesture = Gesture::Marquee { origin, current: position, additive };
                Intent::None
            }
            Gesture::Move { origin, moved, .. } => {
                self.press_travel += (position.x - previous.x).hypot(position.y - previous.y);
                // The threshold is on total travel from the press, not on this one
                // sample: a slow drag arrives as a hundred sub-pixel moves.
                let moved = moved || self.press_travel > CLICK_SLOP;
                self.gesture = Gesture::Move { origin, current: position, moved };
                if moved { Intent::Move { from: origin, to: position } } else { Intent::None }
            }
            Gesture::Place { origin, .. } => {
                self.press_travel += (position.x - previous.x).hypot(position.y - previous.y);
                self.gesture = Gesture::Place { origin, current: position };
                // A sticky only needs the rectangle a drag swept, which `Place`
                // carries. A pen needs the *path*, and it is gone by the time the
                // button comes up — so every sample is reported as it happens and
                // the tool decides whether it cares.
                Intent::PlaceSample { at: position }
            }
            Gesture::Idle => Intent::None,
        }
    }

    /// A mouse button went down or up.
    pub fn mouse_input(
        &mut self,
        camera: &mut Camera,
        button: MouseButton,
        state: ElementState,
    ) -> Intent {
        self.mouse_input_at(camera, button, state, Instant::now())
    }

    fn mouse_input_at(
        &mut self,
        camera: &mut Camera,
        button: MouseButton,
        state: ElementState,
        now: Instant,
    ) -> Intent {
        let at = self.cursor(camera);
        match (state, button) {
            (ElementState::Pressed, _) => {
                // Any press cancels a glide. Grabbing a moving board and having it
                // keep sliding is the classic momentum bug.
                self.stop_glide();
                // The gesture's clock starts here, not at its first motion. Without
                // it the first move of a drag has no interval to divide by, and a
                // slow 3 px nudge after a long pause reads as a flick.
                self.last_motion = Some(now);
                self.press_travel = 0.0;
                if self.pans_with(button) {
                    self.gesture = Gesture::Pan { button, last: at };
                    return Intent::None;
                }
                if button != MouseButton::Left {
                    self.gesture = Gesture::Idle;
                    return Intent::None;
                }
                if self.tool == Tool::Place {
                    self.gesture = Gesture::Place { origin: at, current: at };
                    // The press is the stroke's first point. Reporting it here rather
                    // than waiting for the first motion is what makes a pen start
                    // where the nib was put down; without it the path begins a few
                    // pixels in, and a quick flick that ends after one motion sample
                    // is a single point, which is below the floor for a mark and is
                    // dropped entirely.
                    return Intent::PlaceSample { at };
                }

                // **A run, not a flag.** This used to clear `last_click` on the second press,
                // which made a *third* click a fresh single and put triple-click out of reach —
                // the user's *"if i click one more time it should select everything"*. Counting
                // the run instead costs one field and leaves `Intent::Press`'s shape alone, so
                // every existing caller and every existing test is untouched: `double` still
                // means exactly "the second of a run", and a third press still reports `false`.
                self.clicks = if self.is_second_click(at, now) { self.clicks + 1 } else { 1 };
                let double = self.clicks == 2;
                self.last_click = Some((now, at));
                self.gesture = Gesture::Pending { origin: at, additive: self.additive_held() };
                Intent::Press { at, additive: self.additive_held(), double }
            }
            (ElementState::Released, _) => {
                let finished = std::mem::replace(&mut self.gesture, Gesture::Idle);
                match finished {
                    Gesture::Pan { button: held, .. } if held == button => {
                        if self.config.drag_inertia {
                            self.start_glide();
                        }
                        // A right button that panned nowhere was a right *click*.
                        // `opens_context_menu` had been written for exactly this and
                        // had no caller — the menu it exists for did not exist, so the
                        // right button did nothing at all on the canvas.
                        if self.opens_context_menu(button, self.press_travel) {
                            Intent::ContextMenu { at }
                        } else {
                            Intent::None
                        }
                    }
                    // A marquee that never moved is a click, and the click already
                    // happened on press. Sweeping a rectangle of a few pixels and
                    // selecting whatever it grazes is not what the user meant, and it
                    // is what makes clicking feel unreliable.
                    Gesture::Marquee { origin, current, additive }
                        if button == MouseButton::Left =>
                    {
                        if self.press_travel <= CLICK_SLOP {
                            Intent::None
                        } else {
                            Intent::Marquee { from: origin, to: current, additive }
                        }
                    }
                    Gesture::Move { moved, .. } if button == MouseButton::Left => {
                        Intent::MoveEnd { moved }
                    }
                    Gesture::Place { origin, current } if button == MouseButton::Left => {
                        Intent::Place { at: origin, to: current }
                    }
                    // The press was never resolved and never moved: nothing to do
                    // beyond what the press itself already did.
                    Gesture::Pending { .. } if button == MouseButton::Left => Intent::None,
                    other => {
                        // Some other button came up; the gesture in flight continues.
                        self.gesture = other;
                        Intent::None
                    }
                }
            }
        }
    }

    /// The pointer left the window. A drag that resumed on re-entry would arrive
    /// with a viewport-sized delta.
    pub fn cursor_left(&mut self) {
        if let Gesture::Pan { .. } = self.gesture {
            self.gesture = Gesture::Idle;
        }
        self.cursor_known = false;
    }

    /// A wheel notch or a trackpad scroll.
    pub fn wheel(&mut self, camera: &mut Camera, delta: MouseScrollDelta, phase: TouchPhase) {
        self.wheel_at(camera, delta, phase, Instant::now());
    }

    fn wheel_at(
        &mut self,
        camera: &mut Camera,
        delta: MouseScrollDelta,
        phase: TouchPhase,
        now: Instant,
    ) {
        let (dx, dy) = scroll_pixels(delta, self.scale_factor);
        // A wheel reports discrete notches, a trackpad reports pixels. That is the
        // only reliable signal of which device is in the user's hand, and the two
        // want opposite defaults: a wheel zooms, two fingers pan.
        let from_wheel = matches!(delta, MouseScrollDelta::LineDelta(..));

        match phase {
            TouchPhase::Started => {
                // A sequence that begins within a frame or two of the last one
                // ending is the window server taking over, not the user starting
                // again. Either way whatever we were gliding is superseded — by the
                // platform's momentum, or by a fresh gesture.
                self.scroll_momentum = self
                    .scroll_ended_at
                    .is_some_and(|ended| now.saturating_duration_since(ended) < MOMENTUM_HANDOVER);
                self.stop_glide();
            }
            TouchPhase::Ended => {
                self.scroll_ended_at = Some(now);
                if self.config.scroll_inertia && !self.scroll_momentum {
                    self.start_glide();
                }
                return;
            }
            TouchPhase::Moved | TouchPhase::Cancelled => {}
        }

        // Shift is the escape hatch on a wheel that only has one axis: it pans
        // horizontally, which is what every other canvas app does, and it takes
        // priority so there is always a way to scroll sideways with a mouse.
        let shift_pans = from_wheel && self.modifiers.shift_key();

        if !shift_pans && (self.zoom_modifier_held() || (from_wheel && self.config.wheel_zooms)) {
            let sign = if self.config.invert_zoom { -1.0 } else { 1.0 };
            // A wheel steps, everything else glides. See [`ZOOM_PER_WHEEL_NOTCH`] — the
            // notch count is taken from the event rather than reconstructed from `dy`,
            // which has been through `PIXELS_PER_WHEEL_LINE` and the scale factor and so
            // would put a Retina display on a different step size to a 1× one.
            let factor = if let MouseScrollDelta::LineDelta(_, notches) = delta {
                let steps = sign * f64::from(notches) * self.config.zoom_sensitivity;
                ZOOM_PER_WHEEL_NOTCH.powf(steps)
            } else {
                let logical = dy / self.scale_factor;
                (sign * logical * ZOOM_RATE_PER_PIXEL * self.config.zoom_sensitivity).exp()
            };
            camera.zoom_by(factor, self.cursor(camera));
            return;
        }

        let (dx, dy) = if self.modifiers.shift_key() && dx == 0.0 { (dy, 0.0) } else { (dx, dy) };
        self.pan(camera, dx, dy, now);
    }

    /// Trackpad magnification. `delta` is a fraction of the current scale per event.
    pub fn pinch(&mut self, camera: &mut Camera, delta: f64) {
        if !delta.is_finite() {
            return;
        }
        let sign = if self.config.invert_zoom { -1.0 } else { 1.0 };
        let exponent = sign * delta * PINCH_ZOOM_RATE * self.config.zoom_sensitivity;
        camera.zoom_by(exponent.exp(), self.cursor(camera));
    }

    /// Advances any inertial glide. Call once per frame; returns whether the camera
    /// moved, so a caller that only redraws on change can use it.
    pub fn tick(&mut self, camera: &mut Camera, dt: Duration) -> bool {
        if !self.gliding {
            return false;
        }
        let dt = dt.as_secs_f64().clamp(0.0, 0.1);
        if dt <= 0.0 {
            return false;
        }

        camera.pan_by_screen_delta(self.velocity.0 * dt, self.velocity.1 * dt);

        // Exponential decay, evaluated exactly rather than as a per-frame constant,
        // so a glide covers the same distance at 60 fps and at 120.
        let decay = (-dt / INERTIA_TAU).exp();
        self.velocity = (self.velocity.0 * decay, self.velocity.1 * decay);
        if self.velocity.0.hypot(self.velocity.1) < INERTIA_CUTOFF {
            self.stop_glide();
        }
        true
    }

    /// Whether a scroll should zoom rather than pan.
    ///
    /// Control and the platform command key both, because macOS synthesises
    /// Ctrl+scroll for some pointing devices while ⌘+scroll is the convention users
    /// bring from every other canvas app.
    fn zoom_modifier_held(&self) -> bool {
        self.modifiers.control_key() || self.modifiers.super_key()
    }

    fn command_held(&self) -> bool {
        self.modifiers.super_key() || self.modifiers.control_key()
    }

    fn additive_held(&self) -> bool {
        self.modifiers.shift_key() || self.command_held()
    }

    /// How many clicks the press being handled is part of.
    ///
    /// `Intent::Press` carries only `double`, because that is all any caller needed until a
    /// text session wanted *triple* to mean "select everything" — the convention every text
    /// field has. Widening the intent would have touched every match on it and every test that
    /// builds one, to serve one caller; this answers the same question for the caller that
    /// asks. Valid immediately after a press and reset by the next one outside the window.
    pub const fn click_run(&self) -> u32 {
        self.clicks
    }

    /// Whether a left press at `at` is the second half of a double click.
    fn is_second_click(&self, at: ScreenPoint, now: Instant) -> bool {
        self.last_click.is_some_and(|(then, where_)| {
            now.saturating_duration_since(then) <= DOUBLE_CLICK_WINDOW
                && (at.x - where_.x).hypot(at.y - where_.y) <= DOUBLE_CLICK_SLOP
        })
    }

    /// Whether pressing `button` grabs the canvas rather than selecting.
    ///
    /// Right-drag pans as well as middle-drag. Plenty of mice have no usable middle
    /// button — a scroll wheel that clicks is stiff, and tilt wheels fire it by
    /// accident — so a right-drag alternative costs nothing and is what most
    /// diagram tools offer. The context menu therefore opens on *release without
    /// motion*, not on press; see [`Self::release`].
    fn pans_with(&self, button: MouseButton) -> bool {
        match button {
            MouseButton::Middle | MouseButton::Right => true,
            MouseButton::Left => self.pan_armed(),
            _ => false,
        }
    }

    /// Whether a released button should open a context menu: right button, and only
    /// if the pointer stayed put, so a right-drag pan does not end in a menu.
    pub fn opens_context_menu(&self, button: MouseButton, travelled: f64) -> bool {
        button == MouseButton::Right && travelled <= CLICK_SLOP
    }

    /// Moves the content by a screen delta and folds it into the velocity estimate.
    fn pan(&mut self, camera: &mut Camera, dx: f64, dy: f64, now: Instant) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        let sign = if self.config.invert_pan { -1.0 } else { 1.0 };
        let (dx, dy) = (
            dx * sign * self.config.pan_sensitivity,
            dy * sign * self.config.pan_sensitivity,
        );
        camera.pan_by_screen_delta(dx, dy);

        let elapsed = self
            .last_motion
            .map(|then| now.saturating_duration_since(then).as_secs_f64())
            // The first sample of a gesture has no interval; assume one frame at
            // 120 Hz rather than dividing by zero.
            .unwrap_or(1.0 / 120.0)
            // The floor guards the division; the ceiling is generous on purpose. A
            // tighter one would divide a slow 3 px drift by a tenth of a second and
            // report a flick, so letting go of a stationary drag would throw the
            // board across the canvas.
            .clamp(1e-4, MAX_MOTION_GAP);
        self.last_motion = Some(now);

        let instant = (dx / elapsed, dy / elapsed);
        let blend = 1.0 - (-elapsed / VELOCITY_TAU).exp();
        self.velocity = (
            self.velocity.0 + (instant.0 - self.velocity.0) * blend,
            self.velocity.1 + (instant.1 - self.velocity.1) * blend,
        );
    }

    fn start_glide(&mut self) {
        // A gesture that ended after the hand had already stopped moving has a stale
        // velocity; releasing then must not throw the board.
        let stale = self
            .last_motion
            .is_none_or(|then| then.elapsed() > Duration::from_millis(80));
        if stale || self.velocity.0.hypot(self.velocity.1) < INERTIA_CUTOFF {
            self.stop_glide();
            return;
        }
        self.gliding = true;
    }

    fn stop_glide(&mut self) {
        self.gliding = false;
        self.velocity = (0.0, 0.0);
        self.last_motion = None;
    }
}

/// Converts a scroll delta into physical pixels.
///
/// A `PixelDelta` is already physical — winit multiplies AppKit's point deltas by
/// the scale factor before it emits them — while a `LineDelta` is a notch count with
/// no relationship to the display, so it is expanded in *logical* pixels and then
/// scaled. Getting that asymmetry wrong makes a mouse wheel scroll half as far as a
/// trackpad on a Retina display.
fn scroll_pixels(delta: MouseScrollDelta, scale_factor: f64) -> (f64, f64) {
    match delta {
        MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
        MouseScrollDelta::LineDelta(x, y) => (
            f64::from(x) * PIXELS_PER_WHEEL_LINE * scale_factor,
            f64::from(y) * PIXELS_PER_WHEEL_LINE * scale_factor,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_scene::{ScreenSize, WorldPoint};
    use winit::dpi::PhysicalPosition;

    fn camera() -> Camera {
        Camera::new(ScreenSize::new(1600.0, 900.0))
    }

    fn input() -> Input {
        let mut input = Input::default();
        input.set_scale_factor(2.0);
        input
    }

    fn press(input: &mut Input, camera: &mut Camera, button: MouseButton) -> Intent {
        input.mouse_input(camera, button, ElementState::Pressed)
    }

    fn press_at(
        input: &mut Input,
        camera: &mut Camera,
        button: MouseButton,
        now: Instant,
    ) -> Intent {
        input.mouse_input_at(camera, button, ElementState::Pressed, now)
    }

    fn release(input: &mut Input, camera: &mut Camera, button: MouseButton) -> Intent {
        input.mouse_input(camera, button, ElementState::Released)
    }

    fn move_to(input: &mut Input, camera: &mut Camera, x: f64, y: f64) -> Intent {
        input.cursor_moved(camera, ScreenPoint::new(x, y))
    }

    fn pixels(x: f64, y: f64) -> MouseScrollDelta {
        MouseScrollDelta::PixelDelta(PhysicalPosition::new(x, y))
    }

    // ----- the reported fault: left-drag must not pan ----------------------

    /// The user's complaint, as a test. A plain left drag sweeps a marquee and the
    /// camera does not move by so much as a pixel.
    #[test]
    fn a_plain_left_drag_selects_and_never_pans() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 100.0, 100.0);
        let before = camera;

        press(&mut input, &mut camera, MouseButton::Left);
        move_to(&mut input, &mut camera, 400.0, 300.0);
        assert_eq!(camera, before, "left-drag moved the camera");
        assert_eq!(
            input.marquee(),
            Some((ScreenPoint::new(100.0, 100.0), ScreenPoint::new(400.0, 300.0)))
        );

        let intent = release(&mut input, &mut camera, MouseButton::Left);
        assert_eq!(
            intent,
            Intent::Marquee {
                from: ScreenPoint::new(100.0, 100.0),
                to: ScreenPoint::new(400.0, 300.0),
                additive: false,
            }
        );
        assert_eq!(camera, before);
        assert!(input.marquee().is_none(), "the marquee outlived the gesture");
    }

    /// Picking is on **press**, because that is the only order in which an item can
    /// be grabbed and dragged in one gesture. A wobble of a couple of pixels must not
    /// then turn the click into a marquee that grazes its neighbours.
    #[test]
    fn a_left_click_picks_on_press_and_a_wobble_does_not_sweep() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 640.0, 480.0);

        assert_eq!(
            press(&mut input, &mut camera, MouseButton::Left),
            Intent::Press { at: ScreenPoint::new(640.0, 480.0), additive: false, double: false }
        );
        input.resolve_press(false);
        move_to(&mut input, &mut camera, 642.0, 481.0);

        assert_eq!(release(&mut input, &mut camera, MouseButton::Left), Intent::None);
    }

    #[test]
    fn shift_click_extends_the_selection() {
        let (mut input, mut camera) = (input(), camera());
        input.set_modifiers(ModifiersState::SHIFT);
        move_to(&mut input, &mut camera, 10.0, 10.0);

        assert_eq!(
            press(&mut input, &mut camera, MouseButton::Left),
            Intent::Press { at: ScreenPoint::new(10.0, 10.0), additive: true, double: false }
        );
    }

    // ----- dragging an item -----------------------------------------------

    /// The headline fault of the first build: a press on an item swept a marquee out
    /// from under it and nothing on the board could be moved with the mouse.
    #[test]
    fn a_drag_from_an_item_moves_it_and_never_marquees() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 200.0, 200.0);
        let before = camera;

        press(&mut input, &mut camera, MouseButton::Left);
        input.resolve_press(true);

        // Under the threshold: nothing has moved yet.
        assert_eq!(move_to(&mut input, &mut camera, 203.0, 200.0), Intent::None);
        assert!(input.marquee().is_none(), "a drag from an item swept a rectangle");

        assert_eq!(
            move_to(&mut input, &mut camera, 260.0, 240.0),
            Intent::Move {
                from: ScreenPoint::new(200.0, 200.0),
                to: ScreenPoint::new(260.0, 240.0),
            }
        );
        assert!(input.is_moving());
        assert_eq!(camera, before, "moving an item moved the camera");

        assert_eq!(
            release(&mut input, &mut camera, MouseButton::Left),
            Intent::MoveEnd { moved: true }
        );
        assert!(!input.is_moving());
    }

    /// The offset is measured from the press every time, so a thousand small samples
    /// cannot accumulate into a drift.
    #[test]
    fn a_move_reports_the_offset_from_the_press_not_from_the_last_sample() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Left);
        input.resolve_press(true);

        for step in 1..=100 {
            let intent = move_to(&mut input, &mut camera, f64::from(step), 0.0);
            if step > 5 {
                assert_eq!(
                    intent,
                    Intent::Move {
                        from: ScreenPoint::new(0.0, 0.0),
                        to: ScreenPoint::new(f64::from(step), 0.0),
                    }
                );
            }
        }
    }

    /// A press on an item that never travels is a selection, not an undo step.
    #[test]
    fn pressing_an_item_without_moving_commits_nothing() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 50.0, 50.0);
        press(&mut input, &mut camera, MouseButton::Left);
        input.resolve_press(true);
        move_to(&mut input, &mut camera, 52.0, 51.0);

        assert_eq!(
            release(&mut input, &mut camera, MouseButton::Left),
            Intent::MoveEnd { moved: false }
        );
    }

    /// An application that never answers the press must not end up with an item stuck
    /// to the cursor. Falling back to a marquee loses nothing.
    #[test]
    fn an_unresolved_press_falls_back_to_a_marquee() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Left);
        move_to(&mut input, &mut camera, 100.0, 100.0);

        assert!(input.marquee().is_some());
        assert!(matches!(
            release(&mut input, &mut camera, MouseButton::Left),
            Intent::Marquee { .. }
        ));
    }

    #[test]
    fn escape_abandons_a_drag_and_reports_whether_there_was_one() {
        let (mut input, mut camera) = (input(), camera());
        assert!(!input.cancel_gesture(), "nothing was in flight");

        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Left);
        input.resolve_press(true);
        move_to(&mut input, &mut camera, 90.0, 0.0);

        assert!(input.cancel_gesture());
        assert!(!input.is_moving());
        assert_eq!(release(&mut input, &mut camera, MouseButton::Left), Intent::None);
    }

    // ----- placement tools -------------------------------------------------

    /// Miro sizes a sticky by dragging it out. Before this, that swept a selection
    /// rectangle, created nothing, and left the tool armed for the next stray click.
    #[test]
    fn a_placement_tool_drags_out_a_box_rather_than_a_marquee() {
        let (mut input, mut camera) = (input(), camera());
        input.set_tool(Tool::Place);
        move_to(&mut input, &mut camera, 100.0, 100.0);

        // The press is the first sample, not nothing: a pen has to start where the nib
        // was put down. A tool that only wants the rectangle ignores it and reads
        // `placement()` instead.
        assert_eq!(
            press(&mut input, &mut camera, MouseButton::Left),
            Intent::PlaceSample { at: ScreenPoint::new(100.0, 100.0) }
        );
        move_to(&mut input, &mut camera, 400.0, 300.0);
        assert!(input.marquee().is_none(), "a create tool swept a selection rectangle");
        assert_eq!(
            input.placement(),
            Some((ScreenPoint::new(100.0, 100.0), ScreenPoint::new(400.0, 300.0)))
        );

        assert_eq!(
            release(&mut input, &mut camera, MouseButton::Left),
            Intent::Place {
                at: ScreenPoint::new(100.0, 100.0),
                to: ScreenPoint::new(400.0, 300.0),
            }
        );
    }

    #[test]
    fn a_placement_tool_click_places_at_one_point() {
        let (mut input, mut camera) = (input(), camera());
        input.set_tool(Tool::Place);
        move_to(&mut input, &mut camera, 640.0, 360.0);
        press(&mut input, &mut camera, MouseButton::Left);

        let at = ScreenPoint::new(640.0, 360.0);
        assert_eq!(
            release(&mut input, &mut camera, MouseButton::Left),
            Intent::Place { at, to: at }
        );
    }

    /// A placement tool still has to be able to pan, or arming the sticky tool traps
    /// the user on whatever part of the board they were looking at.
    #[test]
    fn a_placement_tool_still_pans_with_space_and_the_middle_button() {
        let (mut input, mut camera) = (input(), camera());
        input.set_tool(Tool::Place);
        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Middle);
        move_to(&mut input, &mut camera, 40.0, 0.0);
        assert_eq!(camera.center().x, -40.0);
        assert!(input.placement().is_none());
    }

    // ----- double click ----------------------------------------------------

    #[test]
    fn two_quick_clicks_in_the_same_place_are_a_double_click() {
        let (mut input, mut camera) = (input(), camera());
        let start = Instant::now();
        input.cursor_moved_at(&mut camera, ScreenPoint::new(300.0, 300.0), start);

        let first = press_at(&mut input, &mut camera, MouseButton::Left, start);
        assert!(matches!(first, Intent::Press { double: false, .. }));
        input.resolve_press(true);
        release(&mut input, &mut camera, MouseButton::Left);

        let second = press_at(
            &mut input,
            &mut camera,
            MouseButton::Left,
            start + Duration::from_millis(180),
        );
        assert!(matches!(second, Intent::Press { double: true, .. }), "{second:?}");
        input.resolve_press(true);
        release(&mut input, &mut camera, MouseButton::Left);

        // Three clicks are not two double clicks: the pair is consumed.
        let third = press_at(
            &mut input,
            &mut camera,
            MouseButton::Left,
            start + Duration::from_millis(340),
        );
        assert!(matches!(third, Intent::Press { double: false, .. }), "{third:?}");
    }

    #[test]
    fn two_slow_or_distant_clicks_are_two_single_clicks() {
        for (delay, at) in [
            (Duration::from_millis(900), ScreenPoint::new(300.0, 300.0)),
            (Duration::from_millis(100), ScreenPoint::new(400.0, 300.0)),
        ] {
            let (mut input, mut camera) = (input(), camera());
            let start = Instant::now();
            input.cursor_moved_at(&mut camera, ScreenPoint::new(300.0, 300.0), start);
            press_at(&mut input, &mut camera, MouseButton::Left, start);
            input.resolve_press(true);
            release(&mut input, &mut camera, MouseButton::Left);

            input.cursor_moved_at(&mut camera, at, start + delay);
            let second = press_at(&mut input, &mut camera, MouseButton::Left, start + delay);
            assert!(matches!(second, Intent::Press { double: false, .. }), "{second:?}");
        }
    }

    // ----- the three ways to pan ------------------------------------------

    #[test]
    fn space_drag_pans_and_releasing_space_ends_it() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 200.0, 200.0);
        input.key(&Key::Named(NamedKey::Space), ElementState::Pressed);
        assert!(input.pan_armed());

        press(&mut input, &mut camera, MouseButton::Left);
        assert!(input.is_panning());
        move_to(&mut input, &mut camera, 300.0, 260.0);
        assert_eq!(camera.center(), WorldPoint::new(-100.0, -60.0));
        assert!(input.marquee().is_none(), "space-drag started a marquee");

        input.key(&Key::Named(NamedKey::Space), ElementState::Released);
        let after_release = camera;
        move_to(&mut input, &mut camera, 900.0, 900.0);
        assert_eq!(camera, after_release, "the pan continued after space came up");
    }

    #[test]
    fn middle_drag_pans_without_any_modifier() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Middle);
        move_to(&mut input, &mut camera, 40.0, -25.0);

        assert_eq!(camera.center(), WorldPoint::new(-40.0, 25.0));
        assert!(input.marquee().is_none());
    }

    #[test]
    fn the_hand_tool_makes_left_drag_pan() {
        let (mut input, mut camera) = (input(), camera());
        input.key(&Key::Character("h".into()), ElementState::Pressed);
        assert_eq!(input.tool(), Tool::Hand);

        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Left);
        move_to(&mut input, &mut camera, 100.0, 0.0);
        assert_eq!(camera.center(), WorldPoint::new(-100.0, 0.0));

        input.key(&Key::Character("v".into()), ElementState::Pressed);
        assert_eq!(input.tool(), Tool::Select);
    }

    /// `⌘V` is paste and `⌘H` hides the app. Neither is a tool change, and a tool
    /// that changed under the user's hand while they pasted would be baffling.
    #[test]
    fn a_command_chord_is_never_a_tool_shortcut() {
        let mut input = input();
        input.set_modifiers(ModifiersState::SUPER);

        input.set_tool(Tool::Hand);
        assert!(!input.key(&Key::Character("v".into()), ElementState::Pressed));
        assert_eq!(input.tool(), Tool::Hand);

        input.set_tool(Tool::Select);
        assert!(!input.key(&Key::Character("h".into()), ElementState::Pressed));
        assert_eq!(input.tool(), Tool::Select);
    }

    // ----- scrolling tracks the fingers -----------------------------------

    /// The second reported fault. A two-finger scroll of *n* physical pixels moves
    /// the content *n* physical pixels, in the same direction, at every zoom level —
    /// which is what "1:1 with the fingers" means once the camera has a scale.
    #[test]
    fn scrolling_moves_the_content_with_the_fingers_at_any_zoom() {
        for zoom in [0.05, 0.5, 1.0, 3.0, 32.0] {
            let (mut input, mut camera) = (input(), camera());
            camera.set_zoom_about(zoom, ScreenPoint::new(800.0, 450.0));
            let anchored = camera.screen_to_world(ScreenPoint::new(400.0, 300.0));

            input.wheel(&mut camera, pixels(60.0, -45.0), TouchPhase::Moved);

            let now = camera.world_to_screen(anchored);
            assert!((now.x - 460.0).abs() < 1e-9, "zoom {zoom}: x moved to {}", now.x);
            assert!((now.y - 255.0).abs() < 1e-9, "zoom {zoom}: y moved to {}", now.y);
        }
    }

    #[test]
    fn inverting_the_pan_flips_both_axes() {
        let (mut input, mut camera) = (input(), camera());
        input.config_mut().invert_pan = true;
        input.wheel(&mut camera, pixels(30.0, 20.0), TouchPhase::Moved);
        assert_eq!(camera.center(), WorldPoint::new(30.0, 20.0));
    }

    #[test]
    fn pan_sensitivity_scales_the_distance_travelled() {
        let (mut input, mut camera) = (input(), camera());
        input.config_mut().pan_sensitivity = 2.5;
        input.wheel(&mut camera, pixels(0.0, 100.0), TouchPhase::Moved);
        assert_eq!(camera.center().y, -250.0);
    }

    /// A wheel notch is a notch on any display; a trackpad's pixels are already
    /// physical. Expanding both in physical pixels would make the mouse crawl on a
    /// Retina screen.
    #[test]
    fn a_wheel_line_is_scaled_by_the_display_and_a_pixel_delta_is_not() {
        assert_eq!(scroll_pixels(pixels(10.0, -20.0), 2.0), (10.0, -20.0));
        assert_eq!(
            scroll_pixels(MouseScrollDelta::LineDelta(0.0, 1.0), 2.0),
            (0.0, PIXELS_PER_WHEEL_LINE * 2.0)
        );
    }

    #[test]
    fn shift_scroll_pans_sideways_on_a_one_axis_wheel() {
        let (mut input, mut camera) = (input(), camera());
        input.set_modifiers(ModifiersState::SHIFT);
        input.wheel(&mut camera, MouseScrollDelta::LineDelta(0.0, 1.0), TouchPhase::Moved);

        assert_eq!(camera.center().y, 0.0, "a shift-scroll moved vertically");
        assert!(camera.center().x < 0.0, "a shift-scroll did not move horizontally");
    }

    // ----- zoom stays under the pointer -----------------------------------

    /// The headline requirement: over a long gesture the world point under the
    /// cursor must not move at all. 1000 steps, at the far corner of the reference
    /// board, driven through the real event path rather than through `Camera`.
    #[test]
    fn zoom_never_drifts_from_the_cursor_over_a_thousand_steps() {
        for anchor in [
            ScreenPoint::new(0.0, 0.0),
            ScreenPoint::new(1599.0, 899.0),
            ScreenPoint::new(37.0, 811.0),
            ScreenPoint::new(800.0, 450.0),
        ] {
            let (mut input, mut camera) = (input(), camera());
            input.set_modifiers(ModifiersState::SUPER);
            camera.set_center(WorldPoint::new(41_282.89, 17_515.36));
            input.cursor_moved(&mut camera, anchor);

            let pinned = camera.screen_to_world(anchor);
            for step in 0..1000 {
                // Alternating, uneven notches: a symmetric sequence could hide a
                // drift that cancels, and the clamps at both ends of the zoom range
                // are reached and left again.
                let notch = if step % 3 == 0 { -37.0 } else { 21.0 };
                input.wheel(&mut camera, pixels(0.0, notch), TouchPhase::Moved);

                let now = camera.screen_to_world(anchor);
                assert!(
                    (now.x - pinned.x).abs() < 1e-6 && (now.y - pinned.y).abs() < 1e-6,
                    "step {step} at {anchor:?}: anchor moved from {pinned:?} to {now:?}"
                );
            }
        }
    }

    /// Zooming in by *n* and back out by *n* returns to exactly the starting scale.
    /// This is the property the exponential mapping exists for, and the one
    /// `1.0 + delta` does not have.
    #[test]
    fn scroll_zoom_is_exactly_symmetric() {
        let (mut input, mut camera) = (input(), camera());
        input.set_modifiers(ModifiersState::CONTROL);
        input.cursor_moved(&mut camera, ScreenPoint::new(300.0, 200.0));
        let start = camera.zoom();

        for _ in 0..40 {
            input.wheel(&mut camera, pixels(0.0, 24.0), TouchPhase::Moved);
        }
        for _ in 0..40 {
            input.wheel(&mut camera, pixels(0.0, -24.0), TouchPhase::Moved);
        }
        assert!((camera.zoom() - start).abs() < 1e-9, "ended at {}", camera.zoom());
    }

    /// Same property for the pinch, which used to be `1.0 + delta` and therefore
    /// asymmetric: forty pinches in and forty out left the board 0.4% smaller.
    #[test]
    fn pinch_zoom_is_exactly_symmetric_and_stays_on_the_cursor() {
        let (mut input, mut camera) = (input(), camera());
        input.cursor_moved(&mut camera, ScreenPoint::new(1200.0, 700.0));
        let anchor = ScreenPoint::new(1200.0, 700.0);
        let pinned = camera.screen_to_world(anchor);
        let start = camera.zoom();

        for _ in 0..200 {
            input.pinch(&mut camera, 0.02);
        }
        for _ in 0..200 {
            input.pinch(&mut camera, -0.02);
        }

        assert!((camera.zoom() - start).abs() < 1e-9, "ended at {}", camera.zoom());
        let now = camera.screen_to_world(anchor);
        assert!((now.x - pinned.x).abs() < 1e-6 && (now.y - pinned.y).abs() < 1e-6);
    }

    /// A single flick of the trackpad must not cross the whole zoom range. The old
    /// `1.0 + delta` mapping made one event a scale change of `delta`; the damped
    /// exponential keeps a plausible event under a factor of two.
    #[test]
    fn one_pinch_event_is_a_bounded_step() {
        let (mut input, mut camera) = (input(), camera());
        let start = camera.zoom();
        input.pinch(&mut camera, 0.3);
        let ratio = camera.zoom() / start;
        assert!((1.0..2.0).contains(&ratio), "one pinch event scaled by {ratio}");
    }

    #[test]
    fn nonsense_pinch_deltas_are_ignored() {
        let (mut input, mut camera) = (input(), camera());
        let before = camera;
        input.pinch(&mut camera, f64::NAN);
        input.pinch(&mut camera, f64::INFINITY);
        assert_eq!(camera, before);
    }

    #[test]
    fn zoom_sensitivity_and_inversion_apply_to_both_wheel_and_pinch() {
        /// One ctrl-scroll notch through a freshly configured input.
        fn notch(configure: impl FnOnce(&mut Input)) -> f64 {
            let mut input = input();
            let mut camera = camera();
            input.set_modifiers(ModifiersState::CONTROL);
            configure(&mut input);
            input.wheel(&mut camera, pixels(0.0, 40.0), TouchPhase::Moved);
            camera.zoom()
        }

        let baseline = notch(|_| {});
        assert!(baseline > 1.0, "a positive notch should zoom in, got {baseline}");

        let doubled = notch(|input| input.config_mut().zoom_sensitivity = 2.0);
        assert!(
            (doubled - baseline * baseline).abs() < 1e-9,
            "double sensitivity should square the step: {doubled} vs {}",
            baseline * baseline
        );

        let inverted = notch(|input| input.config_mut().invert_zoom = true);
        assert!((inverted - 1.0 / baseline).abs() < 1e-9);

        let mut input = input();
        let mut camera = camera();
        input.config_mut().invert_zoom = true;
        let before = camera.zoom();
        input.pinch(&mut camera, 0.1);
        assert!(camera.zoom() < before, "inverted pinch still zoomed in");
    }

    /// The rate is defined per *logical* pixel, so the same physical gesture zooms
    /// by the same amount on a Retina display as on a 1× one.
    #[test]
    fn zoom_rate_is_independent_of_the_display_scale() {
        let step = |scale: f64| {
            let mut input = Input::default();
            input.set_scale_factor(scale);
            input.set_modifiers(ModifiersState::CONTROL);
            let mut camera = camera();
            // The same physical gesture: winit reports twice the pixels at 2×.
            input.wheel(&mut camera, pixels(0.0, 30.0 * scale), TouchPhase::Moved);
            camera.zoom()
        };
        assert!((step(1.0) - step(2.0)).abs() < 1e-12, "{} vs {}", step(1.0), step(2.0));
    }

    /// A gesture that arrives before the pointer has ever been seen must anchor
    /// somewhere defensible. The window corner is not it.
    #[test]
    fn zoom_before_the_pointer_is_known_anchors_on_the_viewport_centre() {
        let (mut input, mut camera) = (input(), camera());
        input.set_modifiers(ModifiersState::CONTROL);
        let centre = camera.screen_to_world(ScreenPoint::new(800.0, 450.0));

        input.wheel(&mut camera, pixels(0.0, 200.0), TouchPhase::Moved);

        let now = camera.screen_to_world(ScreenPoint::new(800.0, 450.0));
        assert!((now.x - centre.x).abs() < 1e-9 && (now.y - centre.y).abs() < 1e-9);
    }

    // ----- inertia --------------------------------------------------------

    #[test]
    fn a_flick_glides_on_after_the_button_comes_up_and_then_stops() {
        let (mut input, mut camera) = (input(), camera());
        // Opt in: drag inertia is off by default because a mouse has no momentum.
        // The mechanism still has to work, for trackpad drags later.
        input.config_mut().drag_inertia = true;
        let start = Instant::now();
        input.cursor_moved_at(&mut camera, ScreenPoint::new(0.0, 0.0), start);
        press_at(&mut input, &mut camera, MouseButton::Middle, start);

        // 100 px every 8 ms for six frames: 12,500 px/s.
        for step in 1..=6 {
            let at = start + Duration::from_millis(8 * step);
            input.cursor_moved_at(&mut camera, ScreenPoint::new(100.0 * step as f64, 0.0), at);
        }
        let released = camera.center().x;
        release(&mut input, &mut camera, MouseButton::Middle);

        let mut frames = 0;
        while input.tick(&mut camera, Duration::from_millis(8)) {
            frames += 1;
            assert!(frames < 500, "the glide never stopped");
        }

        assert!(frames > 5, "the glide lasted only {frames} frames");
        assert!(
            camera.center().x < released,
            "the glide did not continue the drag's direction"
        );
        assert!(!input.tick(&mut camera, Duration::from_millis(8)));
    }

    /// Grabbing a gliding board must stop it dead. Anything else feels like the
    /// canvas is fighting the hand.
    #[test]
    fn pressing_a_button_cancels_a_glide() {
        let (mut input, mut camera) = (input(), camera());
        input.config_mut().drag_inertia = true;
        let start = Instant::now();
        input.cursor_moved_at(&mut camera, ScreenPoint::new(0.0, 0.0), start);
        press_at(&mut input, &mut camera, MouseButton::Middle, start);
        for step in 1..=6 {
            let at = start + Duration::from_millis(8 * step);
            input.cursor_moved_at(&mut camera, ScreenPoint::new(120.0 * step as f64, 0.0), at);
        }
        release(&mut input, &mut camera, MouseButton::Middle);
        assert!(input.tick(&mut camera, Duration::from_millis(8)));

        press(&mut input, &mut camera, MouseButton::Left);
        let held = camera;
        assert!(!input.tick(&mut camera, Duration::from_millis(8)));
        assert_eq!(camera, held);
    }

    /// Letting go after the hand has already stopped must not throw the board.
    #[test]
    fn releasing_a_stationary_drag_does_not_glide() {
        let (mut input, mut camera) = (input(), camera());
        let start = Instant::now();
        input.cursor_moved_at(&mut camera, ScreenPoint::new(0.0, 0.0), start);
        press_at(&mut input, &mut camera, MouseButton::Middle, start);
        input.cursor_moved_at(&mut camera, ScreenPoint::new(3.0, 0.0), start + Duration::from_millis(200));
        release(&mut input, &mut camera, MouseButton::Middle);

        assert!(!input.tick(&mut camera, Duration::from_millis(8)));
    }

    #[test]
    fn inertia_can_be_switched_off() {
        let (mut input, mut camera) = (input(), camera());
        input.config_mut().drag_inertia = false;
        let start = Instant::now();
        input.cursor_moved_at(&mut camera, ScreenPoint::new(0.0, 0.0), start);
        press_at(&mut input, &mut camera, MouseButton::Middle, start);
        for step in 1..=6 {
            let at = start + Duration::from_millis(8 * step);
            input.cursor_moved_at(&mut camera, ScreenPoint::new(150.0 * step as f64, 0.0), at);
        }
        release(&mut input, &mut camera, MouseButton::Middle);
        assert!(!input.tick(&mut camera, Duration::from_millis(8)));
    }

    /// The exact event sequence macOS produces for one flicked two-finger scroll:
    /// the earlier phase, then a *second* sequence that is the window server's
    /// momentum. Gliding on top of that momentum doubles every throw.
    #[test]
    fn platform_momentum_suppresses_our_own_glide() {
        let (mut input, mut camera) = (input(), camera());
        input.config_mut().scroll_inertia = true;
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);

        input.wheel_at(&mut camera, pixels(0.0, 0.0), TouchPhase::Started, at(0));
        for step in 1..=6 {
            input.wheel_at(&mut camera, pixels(0.0, 100.0), TouchPhase::Moved, at(8 * step));
        }
        input.wheel_at(&mut camera, pixels(0.0, 100.0), TouchPhase::Ended, at(56));

        // The window server takes over within a frame. Our glide has to give way.
        input.wheel_at(&mut camera, pixels(0.0, 80.0), TouchPhase::Started, at(64));
        assert!(
            !input.tick(&mut camera, Duration::from_millis(8)),
            "our glide survived the platform taking over"
        );
        for step in 1..=6 {
            input.wheel_at(&mut camera, pixels(0.0, 40.0), TouchPhase::Moved, at(64 + 8 * step));
        }
        input.wheel_at(&mut camera, pixels(0.0, 10.0), TouchPhase::Ended, at(120));

        assert!(
            !input.tick(&mut camera, Duration::from_millis(8)),
            "glided on after the platform's own momentum finished"
        );
    }

    /// The other half of the same rule: on a platform that sends no momentum, the
    /// user's own gesture ending *does* glide, or a trackpad pan stops dead.
    #[test]
    fn a_scroll_flick_glides_where_the_platform_sends_no_momentum() {
        let (mut input, mut camera) = (input(), camera());
        input.config_mut().scroll_inertia = true;
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);

        input.wheel_at(&mut camera, pixels(0.0, 0.0), TouchPhase::Started, at(0));
        for step in 1..=6 {
            input.wheel_at(&mut camera, pixels(0.0, 100.0), TouchPhase::Moved, at(8 * step));
        }
        input.wheel_at(&mut camera, pixels(0.0, 100.0), TouchPhase::Ended, at(56));

        assert!(input.tick(&mut camera, Duration::from_millis(8)), "the flick stopped dead");
    }

    // ----- gesture hygiene -------------------------------------------------

    #[test]
    fn a_drag_that_leaves_the_window_does_not_resume_with_a_jump() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 100.0, 100.0);
        press(&mut input, &mut camera, MouseButton::Middle);
        move_to(&mut input, &mut camera, 120.0, 100.0);
        let left_at = camera;

        input.cursor_left();
        move_to(&mut input, &mut camera, 1500.0, 800.0);
        assert_eq!(camera, left_at, "re-entering the window flung the board");
    }

    #[test]
    fn a_right_button_release_does_not_end_a_middle_drag() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Middle);
        release(&mut input, &mut camera, MouseButton::Right);
        assert!(input.is_panning());

        move_to(&mut input, &mut camera, 50.0, 0.0);
        assert_eq!(camera.center().x, -50.0);
    }

    #[test]
    fn the_default_configuration_matches_the_platform_momentum_story() {
        let config = InputConfig::default();
        assert!(
            !config.drag_inertia,
            "a button-drag is a mouse gesture, and a mouse has no momentum to model — \
             a canvas that keeps sliding after the button is up reads as ignoring you"
        );
        assert_eq!(
            config.scroll_inertia,
            !cfg!(target_os = "macos"),
            "macOS sends its own momentum; everywhere else sends none"
        );
        assert_eq!(config.pan_sensitivity, 1.0, "the default must track the fingers exactly");
    }

    /// A wheel notch zooms; two fingers on a trackpad still pan. The devices are
    /// told apart by `LineDelta` vs `PixelDelta`, and they want opposite defaults.
    #[test]
    fn a_wheel_zooms_but_a_trackpad_pans() {
        {
            let (mut i, mut cam) = (input(), camera());
            let at = ScreenPoint::new(100.0, 100.0);
            move_to(&mut i, &mut cam, at.x, at.y);
            let before = cam.zoom();
            let under_cursor = cam.screen_to_world(at);

            i.wheel(&mut cam, MouseScrollDelta::LineDelta(0.0, 1.0), TouchPhase::Moved);

            assert!(cam.zoom() > before, "a wheel notch should zoom in");
            // The centre necessarily moves when zooming about an off-centre point;
            // the invariant that matters is that the point under the pointer does not.
            let after = cam.screen_to_world(at);
            assert!(
                (after.x - under_cursor.x).abs() < 1e-9 && (after.y - under_cursor.y).abs() < 1e-9,
                "the world point under the cursor drifted: {under_cursor:?} -> {after:?}"
            );
        }
        {
            let (mut i, mut cam) = (input(), camera());
            move_to(&mut i, &mut cam, 100.0, 100.0);
            let before = cam.zoom();
            i.wheel(&mut cam, pixels(0.0, 40.0), TouchPhase::Moved);
            assert_eq!(cam.zoom(), before, "two fingers must not zoom");
            assert_ne!(cam.center(), WorldPoint::new(0.0, 0.0), "they pan");
        }
    }

    /// A wheel has one axis, so Shift has to remain a way to move sideways.
    #[test]
    fn shift_wheel_still_pans_horizontally() {
        let (mut input, mut camera) = (input(), camera());
        move_to(&mut input, &mut camera, 100.0, 100.0);
        let before = camera.zoom();
        input.set_modifiers(ModifiersState::SHIFT);
        input.wheel(&mut camera, MouseScrollDelta::LineDelta(0.0, 1.0), TouchPhase::Moved);

        assert_eq!(camera.zoom(), before, "shift+wheel must pan, not zoom");
        assert_ne!(camera.center().x, 0.0, "and it must move the x axis");
        assert_eq!(camera.center().y, 0.0);
    }

    /// The wheel binding is a preference, so it has to be switchable.
    #[test]
    fn wheel_zoom_can_be_switched_off() {
        let (mut input, mut camera) = (input(), camera());
        input.config_mut().wheel_zooms = false;
        move_to(&mut input, &mut camera, 100.0, 100.0);
        let before = camera.zoom();
        input.wheel(&mut camera, MouseScrollDelta::LineDelta(0.0, 1.0), TouchPhase::Moved);
        assert_eq!(camera.zoom(), before);
        assert_ne!(camera.center().y, 0.0, "it falls back to panning");
    }

    /// Right-drag pans, so that a mouse with a stiff or absent middle button is not
    /// left without a pan gesture at all.
    #[test]
    fn the_right_button_pans_like_the_middle_one() {
        for button in [MouseButton::Middle, MouseButton::Right] {
            let (mut input, mut camera) = (input(), camera());
            // Seat the pointer first: a press uses the last known position as the
            // drag origin, so without this the first move has nothing to subtract.
            move_to(&mut input, &mut camera, 0.0, 0.0);
            press(&mut input, &mut camera, button);
            assert!(input.is_panning(), "{button:?} should grab the canvas");
            move_to(&mut input, &mut camera, 40.0, -25.0);
            assert_eq!(
                camera.center(),
                WorldPoint::new(-40.0, 25.0),
                "{button:?} did not pan"
            );
            release(&mut input, &mut camera, button);
            assert!(!input.is_panning());
            assert!(input.marquee().is_none(), "{button:?} must never marquee");
        }
    }

    /// A right-drag that panned must not also open a context menu on release, or
    /// every pan ends with a menu in the way.
    #[test]
    fn a_right_drag_pans_without_opening_a_menu() {
        let input = input();
        assert!(
            input.opens_context_menu(MouseButton::Right, 0.0),
            "a right click that did not move is a menu"
        );
        assert!(
            !input.opens_context_menu(MouseButton::Right, CLICK_SLOP + 1.0),
            "a right drag is a pan, not a menu"
        );
        assert!(!input.opens_context_menu(MouseButton::Left, 0.0));
    }

    /// The cursor is the only signal of what a click will do *before* it is made.
    #[test]
    fn the_cursor_reports_what_the_next_click_will_do() {
        let (mut input, mut camera) = (input(), camera());
        // **The plain arrow, hovering, whatever is under the pointer.** This used to take
        // an `over_item` flag and answer `Move` for anything hittable, so the cursor
        // changed constantly while reading a board and claimed something was being moved
        // that nobody had touched. The shape now reports what *is* happening, never what
        // could — see `Input::cursor_icon`.
        assert_eq!(input.cursor_icon(None), CursorIcon::Default);

        input.key(&Key::Named(NamedKey::Space), ElementState::Pressed);
        assert_eq!(input.cursor_icon(None), CursorIcon::Grab, "space arms a pan");

        press(&mut input, &mut camera, MouseButton::Middle);
        assert_eq!(input.cursor_icon(None), CursorIcon::Grabbing, "mid-pan");
        release(&mut input, &mut camera, MouseButton::Middle);

        input.key(&Key::Named(NamedKey::Space), ElementState::Released);
        move_to(&mut input, &mut camera, 0.0, 0.0);
        press(&mut input, &mut camera, MouseButton::Left);
        input.resolve_press(false);
        move_to(&mut input, &mut camera, 60.0, 60.0);
        assert_eq!(input.cursor_icon(None), CursorIcon::Crosshair, "marquee in flight");

        // `docs/06-mouse-controls.md` §4: a create tool says so before the click.
        {
            let mut armed = Input::default();
            armed.set_scale_factor(2.0);
            armed.set_tool(Tool::Place);
            assert_eq!(armed.cursor_icon(None), CursorIcon::Crosshair, "a create tool is armed");
            assert_eq!(armed.cursor_icon(None), CursorIcon::Crosshair, "…over an item too");
        }
        {
            let mut dragging = Input::default();
            dragging.set_scale_factor(2.0);
            let mut cam = Camera::new(ScreenSize::new(1600.0, 900.0));
            move_to(&mut dragging, &mut cam, 0.0, 0.0);
            press(&mut dragging, &mut cam, MouseButton::Left);
            dragging.resolve_press(true);
            move_to(&mut dragging, &mut cam, 60.0, 60.0);
            assert_eq!(dragging.cursor_icon(None), CursorIcon::Move, "a move in flight");
        }
    }
}

/// The double-headed arrow that names which way a grip resizes.
///
/// The diagonals are the pair people notice: `NwSe` runs ↖↘ and `NeSw` runs ↗↙, so a
/// top-left grip and a top-right grip get opposite arrows. Getting those two the same
/// way round is the difference between a cursor that reads as a resize and one that
/// reads as decoration.
///
/// Rotate gets `Grab` rather than an arrow. There is no rotate cursor in the platform
/// set, and every alternative — a crosshair, a pointer — says something untrue; `Grab`
/// at least says "this is a thing you take hold of", which is what it is.
const fn handle_cursor(handle: crate::handle::Handle) -> CursorIcon {
    use crate::handle::Handle;
    match handle {
        Handle::TopLeft | Handle::BottomRight => CursorIcon::NwseResize,
        Handle::TopRight | Handle::BottomLeft => CursorIcon::NeswResize,
        Handle::Top | Handle::Bottom => CursorIcon::NsResize,
        Handle::Left | Handle::Right => CursorIcon::EwResize,
        Handle::Rotate => CursorIcon::Grab,
    }
}

#[cfg(test)]
mod handle_cursor_tests {
    use super::*;
    use crate::handle::Handle;

    /// A grip names its own direction, and the two diagonals are opposites.
    ///
    /// The diagonals are the pair worth pinning: getting `NwSe` and `NeSw` the same way
    /// round is the difference between a cursor that reads as a resize and one that reads
    /// as decoration, and nothing on screen would look obviously wrong if they were
    /// swapped — it would just feel off.
    #[test]
    fn every_grip_names_its_direction() {
        assert_eq!(handle_cursor(Handle::TopLeft), CursorIcon::NwseResize);
        assert_eq!(handle_cursor(Handle::BottomRight), CursorIcon::NwseResize);
        assert_eq!(handle_cursor(Handle::TopRight), CursorIcon::NeswResize);
        assert_eq!(handle_cursor(Handle::BottomLeft), CursorIcon::NeswResize);
        assert_ne!(
            handle_cursor(Handle::TopLeft),
            handle_cursor(Handle::TopRight),
            "the two diagonals must not share an arrow"
        );
        assert_eq!(handle_cursor(Handle::Top), CursorIcon::NsResize);
        assert_eq!(handle_cursor(Handle::Left), CursorIcon::EwResize);
        assert_eq!(handle_cursor(Handle::Rotate), CursorIcon::Grab);
    }

    /// A resize is a `Gesture::Move` underneath, so the handle has to outrank the gesture
    /// or the arrow reverts to `Move` the moment the drag starts. That is the whole bug.
    #[test]
    fn a_grip_outranks_the_move_gesture_it_is_made_of() {
        let mut input = Input::default();
        let mut camera = Camera::new(vellum_scene::ScreenSize::new(1600.0, 900.0));
        input.cursor_moved(&mut camera, ScreenPoint::new(100.0, 100.0));
        input.mouse_input(&mut camera, MouseButton::Left, ElementState::Pressed);
        // A left press is resolved by the *application* — trap 5 — so the gesture is not
        // a move until this is answered. Without it the test would be about a marquee.
        input.resolve_press(true);

        assert_eq!(
            input.cursor_icon(None),
            CursorIcon::Move,
            "no grip: an ordinary move in flight still says Move"
        );
        assert_eq!(
            input.cursor_icon(Some(Handle::BottomRight)),
            CursorIcon::NwseResize,
            "a grip in flight must not report Move"
        );
    }

    /// A pan is a whole-canvas gesture and outranks a grip the pointer happens to be over.
    #[test]
    fn a_pan_still_wins() {
        let mut input = Input::default();
        let mut camera = Camera::new(vellum_scene::ScreenSize::new(1600.0, 900.0));
        input.cursor_moved(&mut camera, ScreenPoint::new(100.0, 100.0));
        input.mouse_input(&mut camera, MouseButton::Middle, ElementState::Pressed);
        assert_eq!(input.cursor_icon(Some(Handle::Top)), CursorIcon::Grabbing);
    }
}
