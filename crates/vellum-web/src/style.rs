//! What a selection looks like, and the edits that change it — the browser's half of
//! `vellum_app::inspect`.
//!
//! Two directions, exactly as that module has them.
//!
//! **Out**: [`summarise`] folds a selection into a [`Summary`], which is serialised to JSON
//! and read by the DOM panel to decide *which controls to draw*. **In**: [`apply_style`] and
//! [`apply_transform`] take one control's answer and write it to the document.
//!
//! # The controls are derived from what the item has, never from what it is
//!
//! `vellum_ui::context_bar`'s rule, and it is the whole design. An ink stroke gets a stroke
//! colour and a width and no fill — not because a `match` on "ink" says so, but because ink
//! *has* a stroke and *has no* fill. A shape gets a fill **and** an outline, which is why it
//! also gets an opacity slider and a sticky does not: a sticky *is* its colour, so the alpha
//! in its own swatch says everything there is to say, while a shape paints an interior and
//! an outline and its words, and no single swatch can fade all three.
//!
//! Deriving it this way is what stops a new item kind arriving with a blank panel. It is
//! also the fix for a real defect: `apply_style`'s fill arm once matched `Sticky` and
//! `Frame` and dropped every one of the 41 shape forms into `_ => {}`, so changing a
//! rectangle's colour did nothing, reported nothing, and left the swatch saying *no fill*.
//!
//! # Three states, not two, and the panel needs all of them
//!
//! A property is **absent** (nothing selected carries it — draw no control), **mixed** (they
//! disagree — draw the control showing a placeholder), or it has a value. Collapsing absent
//! and mixed into one `null` would leave the panel unable to tell *"these items have no
//! fill"* from *"these items have different fills"*, and those two want opposite chrome. See
//! [`Prop`] for how they are spelled on the wire, and for the fourth reading — **unset** —
//! which is what the properties where "the document says nothing" is itself an answer need.
//!
//! # Nothing here knows about wasm
//!
//! No `wasm_bindgen`, no `web-sys`, no `js-sys`: this module is a board in and JSON out, and
//! the `#[wasm_bindgen]` exports that call it live in `lib.rs`. That is deliberate — see the
//! testing note at the foot of this comment.
//!
//! # ⚠ The undo group is the sharpest edge in the file
//!
//! Loro's undo manager has no depth count and nothing but `group_end` ever closes a group. So
//! a `?` escaping between `Board::begin_undo_group` and `Board::end_undo_group` leaves one
//! open **for the rest of the session**, and every later grouped operation — every style
//! edit, every move, every reorder — then fails with *"There is already an active undo
//! group"*. `vellum-app` was bitten by this three separate times and fixed it once, at a
//! chokepoint. There is no `Editor::edit` in this crate, so [`grouped`] is that chokepoint:
//! it opens the group, runs the closure, and then closes the group and reprojects
//! **unconditionally**, before it is allowed to look at whether the closure failed. No `?`
//! appears between those two calls.
//!
//! Note what [`grouped`] deliberately does *not* do: if `begin_undo_group` itself fails, it
//! returns without calling `end_undo_group`. A failure there means somebody else's group is
//! already open, and closing it would commit their unfinished gesture as part of ours.
//!
//! # ⚠ Reproject after every change
//!
//! A [`Projection`] is derived state: the scene the painter draws, the R-tree the hit-test
//! queries, and the bounds a camera fit is measured against. Writing to the board without
//! rebuilding it leaves all three describing a board that no longer exists — drawn stale
//! *and* clicked stale, and the second is the worse half, because a click landing where an
//! item is not reads as a completely different bug. [`grouped`] rebuilds on every path
//! including the error one, for the reason `Editor::edit` does: a closure that failed part
//! way has usually already changed something.
//!
//! # ⚠ A locked item is an anchor, not a participant
//!
//! Locked items are not restyled, not moved, not resized and not reordered. They still
//! **count** towards the selection's anchor, so typing an X with a pinned background frame in
//! the selection moves the movable things onto the frame rather than measuring from a box
//! that pretends the frame is not there. That is the rule align and distribute already
//! follow, and every one of those commands wrote straight through a lock once.
//!
//! The single exception is [`StyleEdit::Locked`] itself, which is applied to locked items on
//! purpose: it is the only way back out. Refusing it would make locking a one-way door — a
//! bug that shipped here once and needed a user report to find, because every test of the
//! padlock set it and none of them cleared it.
//!
//! # Everything is in world units
//!
//! Positions, sizes and stroke widths crossing this boundary are board coordinates. Nothing
//! here multiplies by the camera's zoom; a panel that reported screen pixels would give two
//! different widths for one item at two zoom levels, and typing either back would resize it.
//!
//! # ⚠ `panic = "abort"`, so there is no recoverable mistake here
//!
//! The release profile aborts on panic, and on wasm an abort takes the tab down with no
//! message and no stack. So there is no `unwrap`, no `expect`, no slice index and no
//! arithmetic that can trap anywhere in this file. Numbers arriving from the DOM are checked
//! for finiteness before they reach a placement — `parseFloat("")` is `NaN`, a panel will
//! send one eventually, and a `NaN` in a placement poisons the R-tree, `content_bounds` and
//! therefore every later fit: a board that cannot be looked at again, from one empty field.
//!
//! # Testing
//!
//! `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so **nothing in this file can have a
//! runnable test** — `cargo test` builds for the host, where the crate compiles to nothing at
//! all.
//!
//! That is an argument for moving the pure half out rather than a gap to accept. Everything
//! above the `Summary → document` banner is pure: [`summarise`], [`fold`], [`fill_of`],
//! [`stroke_of`], [`stroke_home`], [`text_home`], [`Colour`]'s parsing and the "is the fill
//! the whole item" rule all take a document and answer data. `vellum-project` is where that
//! belongs — it is where `look.rs`, `runs.rs`, `frame.rs` and `card.rs` went for exactly this
//! reason, its `card` module's own doc comment names the reason in one line, it already
//! depends on `vellum-doc`, `vellum-scene` and `vellum-render`, and it is compiled for the
//! host. The two `apply_*` functions can follow later; they are the half `vellum-app` would
//! also like to share, at which point they belong beside `inspect.rs`'s versions or in place
//! of them.

use serde::{Deserialize, Serialize};

use vellum_doc::{Align, Board, Color, ItemId as DocId, Item, ItemKind, Placement};
use vellum_project::project::{Projected, Projection};
use vellum_project::theme::Theme;
use vellum_render::Rgba;
use vellum_scene::ItemId as SceneId;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A colour, as the DOM speaks it.
///
/// `hex` is **RGB only** — six digits — and the alpha rides beside it as a byte. That is
/// exactly what an `<input type="color">` and a range slider are, so the panel never has to
/// compose or split a string, and it is why an eight-digit `#rrggbbaa` is **refused** on the
/// way in rather than merged: two places to say alpha is two places that can disagree about
/// it, and silently picking a winner is how a picker ends up reporting a colour nobody chose.
///
/// Named `Colour` rather than `Color` so it cannot be mistaken for [`vellum_doc::Color`] at a
/// glance — one is a wire value that may be malformed, the other is a document value that
/// cannot be. The **JSON** keys keep the document's American spelling (`text_color`), because
/// they mirror `Style`'s own field names, and a wire format that renames the thing it carries
/// is a translation nobody can check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colour {
    /// `#rrggbb`. Three-digit shorthand is accepted inbound and never produced outbound.
    pub hex: String,
    /// Straight alpha, 0–255. Opaque when the panel omits it.
    #[serde(default = "opaque")]
    pub alpha: u8,
}

const fn opaque() -> u8 {
    u8::MAX
}

impl Colour {
    fn from_doc(color: Color) -> Self {
        Self { hex: format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b), alpha: color.a }
    }

    /// Parses the wire form, naming what is wrong rather than falling back to black.
    ///
    /// A colour that silently becomes black is an item the user has to undo without knowing
    /// why; a refusal is a sentence the panel can put in front of them.
    fn to_doc(&self) -> Result<Color, String> {
        let digits = self.hex.strip_prefix('#').unwrap_or(self.hex.as_str());
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("\"{}\" is not a colour", self.hex));
        }
        // `chars()` rather than byte slicing. Every character here is ASCII by the check
        // above, but a byte index into a `&str` is a panic waiting for the day it is not,
        // and a panic in this crate aborts the tab.
        let nibbles: Vec<u32> = digits.chars().filter_map(|c| c.to_digit(16)).collect();
        let nibble = |i: usize| -> u32 { nibbles.get(i).copied().unwrap_or(0) };
        let pair = |hi: usize, lo: usize| -> u8 { ((nibble(hi) << 4) | nibble(lo)) as u8 };
        match nibbles.len() {
            // `#abc` is `#aabbcc` — each digit doubled, which is the CSS rule.
            3 => {
                let doubled = |i: usize| -> u8 { ((nibble(i) << 4) | nibble(i)) as u8 };
                Ok(Color::rgba(doubled(0), doubled(1), doubled(2), self.alpha))
            }
            6 => Ok(Color::rgba(pair(0, 1), pair(2, 3), pair(4, 5), self.alpha)),
            8 => Err(format!(
                "\"{}\" carries its own alpha; send six digits and the `alpha` field",
                self.hex
            )),
            other => Err(format!("\"{}\" has {other} hex digits, not 3 or 6", self.hex)),
        }
    }
}

/// One property of a selection, with the four readings a panel has to tell apart.
///
/// **Absent is the fourth**, and it is spelled by the key being missing from the JSON rather
/// than by a variant here: a control is drawn only when its key is present, so `'fill' in
/// summary` is the whole test and there is no way to draw a swatch for something that has no
/// fill.
///
/// | JSON | meaning |
/// |---|---|
/// | key absent | nothing selected carries this property — draw no control |
/// | `{"state":"uniform","value":…}` | everything that carries it agrees |
/// | `{"state":"unset"}` | they agree, and the document says nothing — see below |
/// | `{"state":"mixed"}` | they disagree — draw the control with a placeholder |
///
/// **Unset is an answer, not an absence.** For a shape's or a frame's fill it means *the
/// board's default*, which `Style::fill` distinguishes from a transparent colour on purpose.
/// For a font size it means *auto-fit* — Miro's `fs: 0`, which is what every sticky on the
/// reference board carries. A panel that rendered unset as "no control" would hide the only
/// control that can turn auto-fit off.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Prop<T> {
    Uniform { value: T },
    Unset,
    Mixed,
}

/// A read-only box in world units: minimum corner and extent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Box4 {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A control the panel may offer that the document cannot honour, with the reason.
///
/// Reported rather than silently dropped, and reported *with a sentence*, because the house
/// style is a disabled control whose tooltip names what is missing. A control that looks live
/// and does nothing costs the user their time before it costs them their trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unsupported {
    /// The wire name of the [`StyleEdit`] variant, so the panel can key off it directly.
    pub control: &'static str,
    pub why: &'static str,
}

/// Why font weight cannot be written. The same sentence `vellum_app::inspect` gives, because
/// it is the same fact: Miro carries weight inside the rich-text spans and
/// `vellum_doc::Style` has no field for it.
const NO_WEIGHT: &str =
    "Font weight lives in the text's own spans, which the document does not carry as an \
     item-level property yet";

/// What is said when a gesture is refused by a lock rather than by a fault.
const ALL_LOCKED: &str = "Everything selected is locked.";

/// What the chrome needs to draw the right controls.
///
/// Serialised to JSON for the DOM. Every property field is `Option<Prop<T>>` with
/// `skip_serializing_if`, so **absent means the key is not there** — see [`Prop`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    /// How many items are selected.
    pub count: usize,
    /// The distinct [`ItemKind::tag`] values present, in the order they were first met.
    ///
    /// Tags rather than a composed headline: *"3 sticky notes"* is copy, copy belongs in the
    /// page, and pluralising English in Rust so a panel that also has to translate the word
    /// *objects* can print it is work that produces a worse result in both places.
    pub kinds: Vec<&'static str>,
    /// How many of the selected items are locked.
    ///
    /// A count and not a flag: the padlock shows *Lock* or *Unlock* from whether **all** of
    /// them are locked, and an edit reporting fewer changes than [`Self::editable_count`] can
    /// then say how many were left alone.
    pub locked_count: usize,
    /// `count - locked_count` — what a style edit or a transform will actually touch.
    ///
    /// Offered so the panel can warn *before* the press rather than explain after it.
    pub editable_count: usize,
    /// Whether width and height may be typed.
    ///
    /// One unlocked item only, matching the desktop panel. For several items the two fields
    /// describe a bounding box, and there is no single honest reading of *"make this box 400
    /// wide"* that does not silently choose between stretching the box and resizing each
    /// member of it.
    pub size_editable: bool,
    /// The selection's true world bounding box, rotation and scale applied.
    ///
    /// Read-only, and **not** the same numbers as [`Self::x`] and [`Self::y`] — this is the
    /// minimum-corner box taken from the projection's own rects, which is what a *frame the
    /// selection* or *how big is this* readout wants. Absent when nothing is selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Box4>,

    // --- paint ---------------------------------------------------------------
    /// A sticky's own colour, or a shape's or a frame's `Style::fill`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<Prop<Colour>>,
    /// The outline: ink's and a connector's own colour, or a shape's `Style::stroke`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke: Option<Prop<Colour>>,
    /// Outline width in world units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<Prop<f64>>,
    /// Whole-item opacity, 0–1.
    ///
    /// **Present only when the fill is not the whole item**, which is the sticky rule: a note
    /// *is* its colour and its own swatch already carries the alpha, so a second control
    /// beside it would be two ways to say one thing that disagree the moment either is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<Prop<f64>>,

    // --- words ---------------------------------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_color: Option<Prop<Colour>>,
    /// World units. `unset` is auto-fit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<Prop<f64>>,
    /// `"left"`, `"center"` or `"right"` — [`Align::tag`], so the wire and the document
    /// cannot come to spell it differently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<Prop<String>>,

    // --- place ---------------------------------------------------------------
    /// The item's own centre, in world units, matching [`Placement`].
    ///
    /// Reported as the centre and **accepted** as the centre by [`Transform::Position`], so
    /// typing back the number that is shown moves nothing. The desktop panel makes the same
    /// choice, and self-consistency is the only property of a coordinate field that matters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<Prop<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<Prop<f64>>,
    /// Width after `Placement::scale` — what the item measures on the board.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<Prop<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<Prop<f64>>,
    /// Degrees clockwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<Prop<f64>>,

    // --- order ---------------------------------------------------------------
    /// The padlock's own state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<Prop<bool>>,
    /// Whether the four reordering verbs can do anything — false with nothing selected, and
    /// false when everything selected is locked.
    pub z_order: bool,
    /// Controls the panel may draw greyed, with the reason to put in the tooltip.
    pub unsupported: Vec<Unsupported>,
}

impl Summary {
    /// An empty selection: no controls, no box, nothing enabled.
    fn empty() -> Self {
        Self {
            count: 0,
            kinds: Vec::new(),
            locked_count: 0,
            editable_count: 0,
            size_editable: false,
            bounds: None,
            fill: None,
            stroke: None,
            stroke_width: None,
            opacity: None,
            text_color: None,
            font_size: None,
            align: None,
            x: None,
            y: None,
            w: None,
            h: None,
            rotation: None,
            locked: None,
            z_order: false,
            unsupported: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

/// One styling change, as the chrome sends it.
///
/// Externally tagged, which is serde's default and the shape with the fewest surprises for a
/// hand-written JSON producer: `{"fill":{"hex":"#ff0000","alpha":255}}`, `{"fill":null}` for
/// *no fill*, `{"locked":true}`.
///
/// `Fill(None)` and `Stroke(None)` are the *clear* verbs — the panel's ✕ — rather than a
/// second variant each, and what clearing means is decided per item by where that item keeps
/// the property. See [`apply_one`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StyleEdit {
    /// A sticky's own colour, or a shape's or a frame's interior.
    Fill(Option<Colour>),
    /// The outline, wherever this kind keeps it.
    Stroke(Option<Colour>),
    /// World units.
    StrokeWidth(f64),
    /// 0–1, clamped.
    Opacity(f64),
    #[serde(rename = "text_color")]
    TextColour(Option<Colour>),
    /// World units. `null` is auto-fit, and so is `0` — Miro's own `fs: 0`.
    FontSize(Option<f64>),
    /// `"left"`, `"center"` or `"right"`.
    Align(String),
    /// The padlock. Applied even to items that are already locked — it is the way out.
    Locked(bool),
    /// Accepted so that the refusal is a sentence rather than a control that quietly does
    /// nothing. See [`NO_WEIGHT`].
    FontWeight(String),
}

impl StyleEdit {
    /// The wire name, so an error can name the control that was pressed.
    const fn control(&self) -> &'static str {
        match self {
            Self::Fill(_) => "fill",
            Self::Stroke(_) => "stroke",
            Self::StrokeWidth(_) => "stroke_width",
            Self::Opacity(_) => "opacity",
            Self::TextColour(_) => "text_color",
            Self::FontSize(_) => "font_size",
            Self::Align(_) => "align",
            Self::Locked(_) => "locked",
            Self::FontWeight(_) => "font_weight",
        }
    }
}

/// One change to where something is, how big it is, or what is in front of it.
///
/// Externally tagged like [`StyleEdit`], so the four reordering verbs arrive as bare strings:
/// `"bring_to_front"`, beside `{"rotation":45}` and `{"position":{"x":100,"y":40}}`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transform {
    /// Moves the selection so its **minimum centre** lands here, which is the number
    /// [`Summary::x`] reports. Every unlocked member travels by the same delta, so the
    /// selection keeps its shape.
    Position { x: f64, y: f64 },
    /// Scaled size in world units, applied to each unlocked member.
    Size { w: f64, h: f64 },
    /// Degrees clockwise.
    Rotation(f64),
    BringToFront,
    SendToBack,
    BringForward,
    SendBackward,
}

impl Transform {
    /// Whether this verb changes depth rather than geometry.
    const fn is_reorder(self) -> bool {
        matches!(
            self,
            Self::BringToFront | Self::SendToBack | Self::BringForward | Self::SendBackward
        )
    }
}

// ---------------------------------------------------------------------------
// Document → summary
// ---------------------------------------------------------------------------

/// Folds the selection into what the chrome needs.
///
/// Reads through the **projection** rather than re-decoding each item out of the CRDT: the
/// projection is the copy the painter draws and the hit-test queries, so a panel built from
/// it describes what is on screen, and a select-all of 1,300 items costs a walk of a hash map
/// instead of 1,300 Loro reads. `board` is still the authority on *existence* — an id the
/// document no longer contains is dropped, so the panel cannot describe a deleted item.
pub fn summarise(board: &Board, projection: &Projection, selection: &[SceneId]) -> Summary {
    let live: Vec<&Projected> = selection
        .iter()
        .filter_map(|id| projection.get(*id))
        .filter(|projected| board.contains(projected.doc_id))
        .collect();
    if live.is_empty() {
        return Summary::empty();
    }
    let items: Vec<&Item> = live.iter().map(|projected| &projected.item).collect();

    let mut kinds: Vec<&'static str> = Vec::new();
    for item in &items {
        let tag = item.kind.tag();
        if !kinds.contains(&tag) {
            kinds.push(tag);
        }
    }

    let count = items.len();
    let locked_count = items.iter().filter(|item| item.style.locked).count();

    // Both halves of an outline come from one call, so the colour a panel shows and the width
    // beside it can never be read from two different answers about the same item.
    let strokes: Vec<(Option<Colour>, f64)> =
        items.iter().copied().filter_map(stroke_of).collect();

    let fill = fold(items.iter().copied().filter_map(fill_of).collect());
    let stroke = fold(strokes.iter().map(|(colour, _)| colour.clone()).collect());
    let stroke_width = fold(strokes.iter().map(|(_, width)| Some(*width)).collect());

    // Miro's rule, and `context_bar`'s: a fill suppresses the opacity slider **only when the
    // fill is the whole item**. Measured across the selection rather than per item, because
    // the bar is one bar — a sticky and a shape selected together do paint an outline between
    // them, so the slider is the only control that can fade what they have in common.
    let fill_is_the_whole_item = fill.is_some() && stroke.is_none();
    let opacity = if fill_is_the_whole_item {
        None
    } else {
        fold(items.iter().map(|item| Some(item.style.opacity.unwrap_or(1.0))).collect())
    };

    // The three typography controls come from the same predicate that gates writing them, so
    // a control the panel never drew cannot land a property nothing will read back.
    let default_text = theme_stroke();
    let worded: Vec<&Item> =
        items.iter().copied().filter(|item| text_home(&item.kind)).collect();
    let text_color = fold(
        worded
            .iter()
            .map(|item| Some(Colour::from_doc(item.style.text_color.unwrap_or(default_text))))
            .collect(),
    );
    let font_size = fold(worded.iter().map(|item| item.style.font_size).collect());
    let align = fold(
        worded
            .iter()
            .map(|item| Some(item.style.align.unwrap_or(Align::Left).tag().to_owned()))
            .collect(),
    );

    let x = fold(items.iter().map(|item| Some(item.placement.x)).collect());
    let y = fold(items.iter().map(|item| Some(item.placement.y)).collect());
    let w = fold(items.iter().map(|item| Some(item.placement.scaled_size().0)).collect());
    let h = fold(items.iter().map(|item| Some(item.placement.scaled_size().1)).collect());
    let rotation = fold(items.iter().map(|item| Some(item.placement.rotation)).collect());
    let locked = fold(items.iter().map(|item| Some(item.style.locked)).collect());

    let mut unsupported = Vec::new();
    if !worded.is_empty() {
        unsupported.push(Unsupported { control: "font_weight", why: NO_WEIGHT });
    }

    Summary {
        count,
        kinds,
        locked_count,
        editable_count: count - locked_count,
        size_editable: count == 1 && locked_count == 0,
        bounds: union_bounds(&live),
        fill,
        stroke,
        stroke_width,
        opacity,
        text_color,
        font_size,
        align,
        x,
        y,
        w,
        h,
        rotation,
        locked,
        z_order: count > locked_count,
        unsupported,
    }
}

/// Folds one property's readings into a [`Prop`].
///
/// The caller passes **only the items that carry the property**, each as an `Option<T>` where
/// `None` means "carries it, and the document says nothing". An empty input is the property
/// being absent from the whole selection, which is the caller's cue to omit the key.
fn fold<T: PartialEq>(readings: Vec<Option<T>>) -> Option<Prop<T>> {
    let mut readings = readings.into_iter();
    let first = readings.next()?;
    for next in readings {
        if next != first {
            return Some(Prop::Mixed);
        }
    }
    Some(match first {
        Some(value) => Prop::Uniform { value },
        None => Prop::Unset,
    })
}

/// The union of the selection's world rects, taken from the projection rather than
/// recomputed.
///
/// `Projected::bounds` already has rotation and scale applied and is the rectangle the R-tree
/// is keyed on, so a box derived any other way would be a second answer able to disagree with
/// what a marquee selects.
fn union_bounds(live: &[&Projected]) -> Option<Box4> {
    let mut out: Option<Box4> = None;
    for projected in live {
        let rect = projected.bounds;
        out = Some(match out {
            None => Box4 {
                x: rect.min.x,
                y: rect.min.y,
                w: rect.max.x - rect.min.x,
                h: rect.max.y - rect.min.y,
            },
            Some(box4) => {
                let min_x = box4.x.min(rect.min.x);
                let min_y = box4.y.min(rect.min.y);
                let max_x = (box4.x + box4.w).max(rect.max.x);
                let max_y = (box4.y + box4.h).max(rect.max.y);
                Box4 { x: min_x, y: min_y, w: max_x - min_x, h: max_y - min_y }
            }
        });
    }
    out
}

/// `None` for a kind with no fill at all; `Some(None)` for a fill the document leaves unset,
/// which a shape and a frame can say and a sticky cannot.
///
/// A sticky resolves to the colour the **painter** uses when the document is silent, so the
/// swatch is never blank and never disagrees with the note on screen. That fallback is read
/// from `Theme::LIGHT` rather than copied as a constant: `project.rs` draws a sticky with
/// `background.map_or(theme.sticky, …)`, so this is the same value by construction. A
/// hand-copied hex is exactly the trap that left `locked` reporting a constant `false` for a
/// field that had since come to exist.
fn fill_of(item: &Item) -> Option<Option<Colour>> {
    match &item.kind {
        ItemKind::Sticky { background, .. } => {
            Some(Some(Colour::from_doc(background.unwrap_or_else(theme_sticky))))
        }
        // Both keep their interior on the style, and both can say *no fill* — which is a
        // statement rather than an absence. See `Style::fill`.
        ItemKind::Frame { .. } | ItemKind::Shape { .. } => {
            Some(item.style.fill.map(Colour::from_doc))
        }
        _ => None,
    }
}

/// Where an item's outline lives, which is not the same place for every kind.
///
/// Named once rather than re-decided at each arm that touches a border, because getting it
/// right in three arms and wrong in the fourth is how a control ends up half-working.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StrokeHome {
    /// Ink and a connector: the stroke *is* the geometry, so its colour and width sit on the
    /// kind beside the points.
    Kind,
    /// A shape: an outline it merely *has*, carried by `Style` exactly like a fill.
    Style,
    /// A sticky, a frame, an image, a card — nothing this panel can edit.
    Nowhere,
}

const fn stroke_home(kind: &ItemKind) -> StrokeHome {
    match kind {
        ItemKind::Ink { .. } | ItemKind::Connector { .. } => StrokeHome::Kind,
        ItemKind::Shape { .. } => StrokeHome::Style,
        _ => StrokeHome::Nowhere,
    }
}

/// Whether this kind has typography the panel may change.
///
/// The same three kinds `vellum_app::inspect::text_of` admits. A link card's title is
/// metadata scraped off somebody else's page rather than words the user wrote, and a table, a
/// chart, a mind map and a kanban keep their text inside their own token, where an item-level
/// font size would not reach it.
///
/// It gates **both** directions here, which is the one place this module deliberately
/// diverges from `inspect.rs`: that file reports typography for three kinds and *writes* it
/// for any kind, so a control that was never drawn could still land a property nothing reads
/// back. Reporting and applying from one predicate is what makes the panel's silence mean
/// something.
const fn text_home(kind: &ItemKind) -> bool {
    matches!(kind, ItemKind::Sticky { .. } | ItemKind::Text { .. } | ItemKind::Frame { .. })
}

/// The outline as `(colour, width)`, wherever this kind keeps it.
///
/// Takes the whole item rather than its kind, which is what lets a shape answer at all: a
/// shape's outline is in its *style*, so a function handed only the kind cannot see one — and
/// that is exactly why the border section stayed hidden for all 41 shape forms while the same
/// panel edited their fill one row above. The asymmetry was the tell.
///
/// The fallbacks are the painter's: an absent shape stroke draws in the theme's border colour
/// at a hairline, so that is what is reported.
fn stroke_of(item: &Item) -> Option<(Option<Colour>, f64)> {
    match &item.kind {
        ItemKind::Ink { color, thickness, .. } | ItemKind::Connector { color, thickness, .. } => {
            Some((Some(Colour::from_doc(color.unwrap_or_else(theme_stroke))), *thickness))
        }
        ItemKind::Shape { .. } => Some((
            Some(Colour::from_doc(item.style.stroke.unwrap_or_else(theme_border))),
            item.style.stroke_width.unwrap_or(HAIRLINE),
        )),
        _ => None,
    }
}

/// One device pixel at 100% zoom — the width a shape's outline draws at when the document
/// names none.
const HAIRLINE: f64 = 1.0;

/// A cleared outline on a shape: transparent, not absent.
///
/// ⚠ The difference is the whole point. `Style::stroke` documents `None` as *inherit the
/// theme's border*, which is still a visible hairline; a zero alpha refuses one. Both shape
/// paths multiply by the border's alpha, so this is **no** border rather than a fainter one.
const CLEARED: Color = Color::rgba(0, 0, 0, 0);

/// The canvas palette's tokens, converted rather than copied.
///
/// `Theme::LIGHT` is the single source for what the board is painted with, so reading it here
/// means a summary cannot report a colour the renderer does not use. The desktop panel
/// duplicates one of these as a `const` and pins it with a test, because it cannot reach a
/// float palette from a `const` context. Nothing here is `const`, so the duplicate — and the
/// test that has to exist to keep it honest — is simply unnecessary.
fn theme_sticky() -> Color {
    from_rgba(Theme::LIGHT.sticky)
}

fn theme_stroke() -> Color {
    from_rgba(Theme::LIGHT.stroke)
}

fn theme_border() -> Color {
    from_rgba(Theme::LIGHT.border)
}

fn from_rgba(rgba: Rgba) -> Color {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::rgba(channel(rgba.r), channel(rgba.g), channel(rgba.b), channel(rgba.a))
}

// ---------------------------------------------------------------------------
// Summary → document
// ---------------------------------------------------------------------------

/// Runs `f` as **one undo step**, then closes the group and reprojects, whatever happened.
///
/// ⚠ This is the chokepoint the module header is about, and the two rules it exists to keep
/// are both about the way out:
///
/// - `end_undo_group` runs before anything is allowed to fail. Loro's undo manager has no
///   depth count, `group_end` is the only thing that closes a group, and a group left open
///   breaks **every** later grouped operation for the rest of the session — not merely the
///   one that leaked. There is deliberately no `?` between the two calls.
/// - `Projection::rebuild` runs on the error path too. A closure that failed part way has
///   usually already changed the document, so skipping the rebuild would leave the scene, the
///   R-tree and the content bounds describing a board that no longer exists.
///
/// If `begin_undo_group` fails there is already a group open — held by a text session
/// mid-word, most likely — and this returns **without** closing it. Closing somebody else's
/// group would commit their unfinished gesture as part of ours. The caller's job is to settle
/// any live text session before dispatching a board-mutating command, which is the rule
/// `vellum-app` arrived at after finding this same leak three separate times.
fn grouped<T>(
    board: &mut Board,
    projection: &mut Projection,
    f: impl FnOnce(&mut Board) -> vellum_doc::Result<T>,
) -> Result<T, String> {
    if let Err(error) = board.begin_undo_group() {
        return Err(format!("an edit is already in progress, so this one did not run ({error})"));
    }
    let outcome = f(board);
    // Both unconditional, in this order, before the outcome is looked at at all.
    board.end_undo_group();
    let reprojected = projection.rebuild(board);

    let value = outcome.map_err(|error| format!("the edit failed: {error}"))?;
    reprojected
        .map_err(|error| format!("the board changed but could not be redrawn: {error}"))?;
    Ok(value)
}

/// The selected items that still exist, as `(id, item)`.
///
/// Read out of the board rather than the projection because these are about to be written
/// back, and the document is the authority at write time. The cost is one read per *selected*
/// item rather than per item on the board.
fn targets(
    board: &Board,
    projection: &Projection,
    selection: &[SceneId],
) -> Vec<(DocId, Item)> {
    selection
        .iter()
        .filter_map(|id| projection.get(*id))
        .filter_map(|projected| {
            board.item(projected.doc_id).ok().map(|item| (projected.doc_id, item))
        })
        .collect()
}

/// Refuses a number that must never reach a placement.
///
/// `parseFloat("")` is `NaN` and a DOM panel will send one eventually. A `NaN` in a placement
/// poisons the R-tree's bounds, `Projection::content_bounds` and therefore every later fit —
/// a board that cannot be looked at again, from one empty field.
fn finite(value: f64, what: &str) -> Result<f64, String> {
    if value.is_finite() { Ok(value) } else { Err(format!("{what} needs a number")) }
}

/// Applies one styling control to every eligible selected item, as a single undo step.
///
/// Returns how many items changed. `Ok(0)` is **not a failure** — selecting a drawing and
/// asking for a font size is a legitimate no-op, and the panel should say nothing about it.
/// `Err` carries a sentence meant to be shown: a control the document cannot honour, a
/// selection that is entirely locked, a malformed colour, or a write that failed.
///
/// ⚠ **Locked items are skipped, except by [`StyleEdit::Locked`] itself**, which has to reach
/// them or locking is a one-way door.
pub fn apply_style(
    board: &mut Board,
    projection: &mut Projection,
    selection: &[SceneId],
    edit: StyleEdit,
) -> Result<usize, String> {
    if selection.is_empty() {
        return Ok(0);
    }
    // Reported once, before anything is opened, rather than once per item.
    if matches!(edit, StyleEdit::FontWeight(_)) {
        return Err(NO_WEIGHT.to_owned());
    }

    let all = targets(board, projection, selection);
    if all.is_empty() {
        return Ok(0);
    }

    // The padlock reaches everything; every other control leaves a locked item alone.
    //
    // Filtered into an owned list rather than tested inside the write loop: a `contains` on
    // the eligible ids per item is quadratic, and a select-all on the reference board is
    // 1,300 items — a shape this codebase has already paid for once, in the projection walk
    // that ran linearly beside an R-tree that was right there.
    let padlock = matches!(edit, StyleEdit::Locked(_));
    let eligible: Vec<(DocId, Item)> =
        all.into_iter().filter(|(_, item)| padlock || !item.style.locked).collect();
    if eligible.is_empty() {
        return Err(ALL_LOCKED.to_owned());
    }

    // Everything that can be *wrong* is decided here — before the group opens and before a
    // byte of the document has moved — so a malformed colour is a message rather than an undo
    // step that changed half a selection.
    let control = edit.control();
    let resolved = resolve(&edit).map_err(|why| format!("{control}: {why}"))?;

    grouped(board, projection, move |board| {
        let mut changed = 0;
        for (id, item) in &eligible {
            if apply_one(board, *id, item, &resolved)? {
                changed += 1;
            }
        }
        Ok(changed)
    })
}

/// A [`StyleEdit`] with its wire values already turned into document values.
///
/// `Copy`, so the write loop can read it without cloning per item — every field is a document
/// primitive and all of them are `Copy` already.
#[derive(Clone, Copy)]
enum Resolved {
    Fill(Option<Color>),
    Stroke(Option<Color>),
    StrokeWidth(f64),
    Opacity(f64),
    TextColour(Option<Color>),
    FontSize(Option<f64>),
    Align(Align),
    Locked(bool),
}

fn resolve(edit: &StyleEdit) -> Result<Resolved, String> {
    Ok(match edit {
        StyleEdit::Fill(colour) => Resolved::Fill(colour.as_ref().map(Colour::to_doc).transpose()?),
        StyleEdit::Stroke(colour) => {
            Resolved::Stroke(colour.as_ref().map(Colour::to_doc).transpose()?)
        }
        StyleEdit::TextColour(colour) => {
            Resolved::TextColour(colour.as_ref().map(Colour::to_doc).transpose()?)
        }
        StyleEdit::StrokeWidth(width) => Resolved::StrokeWidth(finite(*width, "a stroke width")?),
        StyleEdit::Opacity(opacity) => Resolved::Opacity(finite(*opacity, "an opacity")?),
        // `0` is auto-fit — Miro's own `fs: 0`, and what every sticky on the reference board
        // carries — so a panel clearing the field gets what it means rather than a note whose
        // words vanish. A negative size is a mistake and says so.
        StyleEdit::FontSize(size) => Resolved::FontSize(match size {
            None => None,
            Some(value) => {
                let value = finite(*value, "a font size")?;
                if value < 0.0 {
                    return Err("a font size cannot be negative".to_owned());
                }
                (value > 0.0).then_some(value)
            }
        }),
        StyleEdit::Align(tag) => Resolved::Align(
            Align::from_tag(tag).ok_or_else(|| format!("\"{tag}\" is not an alignment"))?,
        ),
        StyleEdit::Locked(locked) => Resolved::Locked(*locked),
        // Refused before this is reached. Kept as an arm so a new control cannot be added
        // without deciding what it resolves to.
        StyleEdit::FontWeight(_) => return Err(NO_WEIGHT.to_owned()),
    })
}

/// Writes one resolved control to one item, answering whether anything moved.
///
/// Every arm derives its target from what the item **has**, never from a `match` on what it
/// *is* — which is why a shape takes a fill here and a `match` on `Sticky | Frame` did not.
fn apply_one(
    board: &mut Board,
    id: DocId,
    item: &Item,
    edit: &Resolved,
) -> vellum_doc::Result<bool> {
    let mut style = item.style.clone();
    let mut kind = item.kind.clone();
    let mut touched_style = false;
    let mut touched_kind = false;

    match *edit {
        Resolved::Fill(fill) => match &mut kind {
            // A sticky's colour *is* the sticky, so it lives on the kind beside the words —
            // `Style::fill`'s own doc comment says so. Clearing it means "the board's
            // default", which is the closest a note has to no fill.
            ItemKind::Sticky { background, .. } => {
                *background = fill;
                touched_kind = true;
            }
            // A shape and a frame both keep their interior on the **style**, which is what
            // the summary reads back and what both painters draw from.
            ItemKind::Frame { .. } | ItemKind::Shape { .. } => {
                style.fill = fill;
                touched_style = true;
            }
            _ => {}
        },
        Resolved::Stroke(colour) => match stroke_home(&kind) {
            StrokeHome::Kind => touched_kind = set_stroke_color(&mut kind, colour),
            StrokeHome::Style => {
                style.stroke = Some(colour.unwrap_or(CLEARED));
                touched_style = true;
            }
            StrokeHome::Nowhere => {}
        },
        Resolved::StrokeWidth(width) => match stroke_home(&kind) {
            StrokeHome::Kind => touched_kind = set_stroke_width(&mut kind, width),
            // Not held away from zero the way a stroke's own width is: a shape is hit by its
            // *fill*, so a borderless shape is still selectable, and refusing zero would make
            // "no border" reachable only through the clear button.
            StrokeHome::Style => {
                style.stroke_width = Some(width.max(0.0));
                touched_style = true;
            }
            StrokeHome::Nowhere => {}
        },
        Resolved::Opacity(opacity) => {
            style.opacity = Some(opacity.clamp(0.0, 1.0));
            touched_style = true;
        }
        // The three typography controls are gated on the predicate the summary reports them
        // from, so a control the panel never drew cannot land a property nothing reads back.
        Resolved::TextColour(colour) => {
            if text_home(&kind) {
                style.text_color = colour;
                touched_style = true;
            }
        }
        Resolved::FontSize(size) => {
            if text_home(&kind) {
                style.font_size = size;
                touched_style = true;
            }
        }
        Resolved::Align(align) => {
            if text_home(&kind) {
                style.align = Some(align);
                touched_style = true;
            }
        }
        // Written through the same `set_style` as everything else, so one press is one undo
        // step and a lock survives a reload like any other style. `vellum-doc` deliberately
        // does not *enforce* the flag, which is precisely what lets this write clear it.
        Resolved::Locked(locked) => {
            style.locked = locked;
            touched_style = true;
        }
    }

    if touched_kind {
        board.set_kind(id, kind)?;
    }
    if touched_style {
        board.set_style(id, style)?;
    }
    Ok(touched_kind || touched_style)
}

fn set_stroke_color(kind: &mut ItemKind, value: Option<Color>) -> bool {
    match kind {
        ItemKind::Ink { color, .. } | ItemKind::Connector { color, .. } => {
            *color = value;
            true
        }
        _ => false,
    }
}

fn set_stroke_width(kind: &mut ItemKind, width: f64) -> bool {
    // A zero-width stroke is invisible and cannot be hit, which is a way to lose a drawing
    // without deleting it.
    let width = width.max(0.25);
    match kind {
        ItemKind::Ink { thickness, .. } | ItemKind::Connector { thickness, .. } => {
            *thickness = width;
            true
        }
        _ => false,
    }
}

/// Applies a position, a size, a rotation or a change of depth, as a single undo step.
///
/// Returns how many items changed; `Ok(0)` means the numbers were already what was asked for.
///
/// ⚠ **Locked items are skipped and still counted.** A [`Transform::Position`] measures its
/// anchor across the *whole* selection including the locked members, so typing an X with a
/// pinned background frame selected moves the movable things onto the frame rather than
/// measuring from a box that pretends the frame is absent. That is the difference between an
/// anchor and a participant.
pub fn apply_transform(
    board: &mut Board,
    projection: &mut Projection,
    selection: &[SceneId],
    t: Transform,
) -> Result<usize, String> {
    if selection.is_empty() {
        return Ok(0);
    }
    let all = targets(board, projection, selection);
    if all.is_empty() {
        return Ok(0);
    }
    // ⚠ The anchor is taken across the **whole** selection, before the lock filter, and that
    // ordering is the rule rather than an implementation detail: a locked item is an anchor,
    // not a participant.
    let anchor_x = all.iter().map(|(_, item)| item.placement.x).fold(f64::INFINITY, f64::min);
    let anchor_y = all.iter().map(|(_, item)| item.placement.y).fold(f64::INFINITY, f64::min);

    let movable: Vec<(DocId, Item)> =
        all.into_iter().filter(|(_, item)| !item.style.locked).collect();
    if movable.is_empty() {
        return Err(ALL_LOCKED.to_owned());
    }

    if t.is_reorder() {
        let ids: Vec<DocId> = movable.iter().map(|(id, _)| *id).collect();
        return reorder(board, projection, &ids, t);
    }

    let plan = plan_placement(t, anchor_x, anchor_y)?;
    let writes: Vec<(DocId, Placement)> = movable
        .iter()
        .filter_map(|(id, item)| {
            let next = plan(item.placement);
            (next != item.placement).then_some((*id, next))
        })
        .collect();
    if writes.is_empty() {
        return Ok(0);
    }
    grouped(board, projection, move |board| {
        let mut changed = 0;
        for (id, placement) in &writes {
            board.set_placement(*id, *placement)?;
            changed += 1;
        }
        Ok(changed)
    })
}

/// Turns one geometric [`Transform`] into a function from an item's placement to its next
/// one, with every number already checked.
///
/// A closure rather than a `match` inside the write loop, so that a refusal — a `NaN`, a
/// negative size — happens once, before the undo group is opened, instead of part way through
/// a selection with half of it already moved.
fn plan_placement(
    t: Transform,
    anchor_x: f64,
    anchor_y: f64,
) -> Result<Box<dyn Fn(Placement) -> Placement>, String> {
    // Annotated rather than left to the `Ok(match …)` tail: every arm produces a different
    // closure type, and naming the trait object here is what makes each one coerce to it.
    let plan: Box<dyn Fn(Placement) -> Placement> = match t {
        Transform::Position { x, y } => {
            // The deltas are re-checked as well as the inputs: an anchor taken over
            // placements that are all `NaN` folds to infinity, and `x - inf` is not a number
            // anything should be moved by.
            let dx = finite(finite(x, "an X position")? - anchor_x, "an X position")?;
            let dy = finite(finite(y, "a Y position")? - anchor_y, "a Y position")?;
            Box::new(move |p: Placement| Placement { x: p.x + dx, y: p.y + dy, ..p })
        }
        Transform::Size { w, h } => {
            // Guarded rather than silently clamped at zero: a zero-sized item cannot be hit
            // tested, so it becomes unselectable and therefore unrecoverable. `width` is the
            // *unscaled* field, so the size asked for on the board is divided by the scale.
            let w = finite(w, "a width")?.max(1.0);
            let h = finite(h, "a height")?.max(1.0);
            Box::new(move |p: Placement| {
                let scale = p.scale.max(f64::EPSILON);
                Placement { width: w / scale, height: h / scale, ..p }
            })
        }
        Transform::Rotation(degrees) => {
            let degrees = finite(degrees, "a rotation")?.rem_euclid(360.0);
            Box::new(move |p: Placement| Placement { rotation: degrees, ..p })
        }
        // Answered by `reorder`, which touches no placement at all. Unreachable through
        // `apply_transform`, and an arm rather than a wildcard so a new verb has to be
        // classified rather than silently becoming a no-op move.
        Transform::BringToFront
        | Transform::SendToBack
        | Transform::BringForward
        | Transform::SendBackward => return Err("that is a reordering, not a placement".to_owned()),
    };
    Ok(plan)
}

/// Moves the selection through the paint order.
///
/// Forward and backward step past exactly **one** thing currently drawn over the item, using
/// the paint order the projection already computed, rather than past everything. A neighbour
/// that is itself selected is skipped, or a selection would shuffle within itself and appear
/// not to move at all.
///
/// An item already at the edge it is being sent to is left alone, so the count is what
/// actually moved rather than what was asked for — and so a second press does not fill the
/// undo stack with steps that undo nothing. That skip applies only until something *has*
/// moved; see the comment on it, because getting it wrong reverses the selection's own order.
/// The one case it still overcounts is a selection spanning two parents where a later item is
/// at its own parent's edge, which costs a number and never a wrong board.
///
/// Depth is not position, so a lock ignored here is the weakest of the four holes — nothing
/// lands anywhere else — but a lock a whole class of commands ignores is a lock nobody can
/// rely on, and *send to back* over a select-all is exactly how a pinned background frame
/// ends up in front of the board again.
fn reorder(
    board: &mut Board,
    projection: &mut Projection,
    ids: &[DocId],
    t: Transform,
) -> Result<usize, String> {
    let mut ordered: Vec<(i32, DocId)> = projection.iter().map(|(_, p)| (p.z, p.doc_id)).collect();
    ordered.sort_unstable();
    let edges = edges_by_parent(board, ids);
    let ids = ids.to_vec();

    grouped(board, projection, move |board| {
        let mut changed = 0;
        match t {
            Transform::BringToFront | Transform::SendToBack => {
                let front = matches!(t, Transform::BringToFront);
                // Reverse for the back, so the selection keeps its own internal order once
                // every member has been pushed to the bottom.
                let order: Vec<DocId> =
                    if front { ids.clone() } else { ids.iter().rev().copied().collect() };
                for id in order {
                    // ⚠ `changed == 0` is load-bearing and is not an optimisation.
                    //
                    // `edges` is a snapshot, and the first push invalidates it: sending
                    // `[A, X, B]` back with `A` and `B` selected pushes `B`, after which `A`
                    // is no longer at the back and genuinely has to move to get past it.
                    // Skipping it on the stale snapshot leaves `B, A` where the desktop
                    // leaves `A, B` — the selection's own internal order reversed, which is
                    // the whole reason the back pass runs in reverse. So the snapshot is
                    // trusted only while nothing has moved, which is exactly when it is
                    // still true.
                    if changed == 0 && edges.contains(&(id, front)) {
                        continue;
                    }
                    if front {
                        board.bring_to_front(id)?;
                    } else {
                        board.send_to_back(id)?;
                    }
                    changed += 1;
                }
            }
            _ => {
                let forward = matches!(t, Transform::BringForward);
                for id in &ids {
                    let Some(index) = ordered.iter().position(|(_, other)| other == id) else {
                        continue;
                    };
                    let neighbour = if forward {
                        ordered.get(index + 1)
                    } else {
                        index.checked_sub(1).and_then(|i| ordered.get(i))
                    };
                    let Some((_, neighbour)) = neighbour else { continue };
                    if ids.contains(neighbour) {
                        continue;
                    }
                    if forward {
                        board.raise_above(*id, *neighbour)?;
                    } else {
                        board.lower_below(*id, *neighbour)?;
                    }
                    changed += 1;
                }
            }
        }
        Ok(changed)
    })
}

/// Which of these items are already at an edge of their own sibling stack, as
/// `(id, is_the_front_edge)` pairs.
///
/// The same question `Board::move_to_edge` asks itself before doing anything, asked here so
/// the reported count is honest: without it, *bring to front* on something already at the
/// front reports a change nobody can see and that undo has nothing to reverse.
///
/// Computed **once per distinct parent** rather than once per item. `Board::children` walks
/// and allocates, and a select-all sent to the back would otherwise be that walk per item —
/// which on the reference board is 1,300 walks of a 1,300-long list for one menu press. Every
/// item on a flat board shares one parent, so this is almost always a single call.
fn edges_by_parent(board: &Board, ids: &[DocId]) -> Vec<(DocId, bool)> {
    let mut parents: Vec<Option<DocId>> = Vec::new();
    for id in ids {
        let parent = board.parent_of(*id);
        if !parents.contains(&parent) {
            parents.push(parent);
        }
    }
    let mut edges = Vec::new();
    for parent in parents {
        let siblings = board.children(parent);
        if let Some(first) = siblings.first() {
            edges.push((*first, false));
        }
        if let Some(last) = siblings.last() {
            edges.push((*last, true));
        }
    }
    edges
}
