//! The two buttons on a link card: the ↗ that opens its page and the ▶ on a video's poster.
//!
//! The browser drew cards from the day it drew anything, and **you could not open one**. Every
//! other way in that the desktop app offers — the properties panel's *Open page*, the right
//! button, the `⋮` menu — is chrome this viewer does not have and is not getting, because it
//! has no verbs. So on a board that is mostly link cards the badge is not one affordance among
//! several; it is the only one, and its absence made the whole board inert.
//!
//! # One function answers "where is it", and both halves ask it
//!
//! [`badge`] and the private `play_box` are the only places either rectangle is worked out.
//! [`BadgeLayer::push`] draws what they answer and [`pressed`] hit-tests what they answer, so
//! the paint and the press cannot come to disagree — which is `draw::kanban_runs`' rule and
//! `CardLayout::badge`'s, and this repository has paid for breaking it more than once. A second
//! copy of a layout is not a layout bug when it goes wrong; it is a click that lands where the
//! paint is not, and nothing about the symptom says *geometry*.
//!
//! The corollary is worth stating because it is the failure this codebase repeats: **a control
//! that is drawn and not pressable, or pressable and not drawn, is the defect here.** CLAUDE.md
//! counts the first at least nine times, and records the second being shipped once *as the fix
//! for the first* — permission chips made live and never painted, a blank strip that swallowed
//! clicks. Both halves have a named caller and the two are one screen apart in this file.
//!
//! # ⚠ World units, never `camera.zoom()`
//!
//! Everything here goes to the list in camera-relative **world** units, because the board view's
//! clip transform already carries the zoom. Multiplying here applies it twice, and the symptom
//! is not subtle-but-wrong: at a fitted 6% the badge would be drawn at 6% of its own box. That
//! was the worst bug of the web port — one wrong multiplication on the image arm, which read as
//! *"the images do not load"* **and** as "the images are blurry", because a pre-multiplied size
//! also asks `Renderer::observe_detail` for zoom² too few texels. `shapes.rs` and the image arm
//! in `lib.rs` both carry this warning; it is not repeated here out of habit.
//!
//! The one number that *is* screen-derived is the arrow's minimum stroke weight, and it is
//! derived at the call site rather than here — see [`BadgeLayer::push`].
//!
//! # Three deliberate divergences from the desktop painter
//!
//! Each is the same rule applied to a different painter, not a corner cut. The invariant being
//! ported is **agree with what is on screen**, and what is on screen differs here.
//!
//! 1. **Nothing rotates.** Natively the plate and the triangle both turn with the card, and
//!    CLAUDE.md feedback 30 records the bug where the plate turned and the triangle did not —
//!    *"a play button lying on its side inside an upright ring"*. In this viewer the card itself
//!    does not turn: `DrawList::push_scene_item` draws one axis-aligned quad at `item.bounds`,
//!    and `crate::layout`'s picture and text slots are both built from `bounds` with no rotation
//!    anywhere on the path. So a rotated badge would sit at an angle inside an upright card —
//!    feedback 30's bug reproduced *by porting its fix*. Rotation belongs here on the day the
//!    card body gets it, and on that day this module and `crate::layout` change together.
//! 2. **`bounds`, not `rect()`.** `shapes.rs` warns to use `rect()` and is right *for a shape*,
//!    which spins its own silhouette. Here `bounds` is what every other piece of the card is
//!    placed from, so it is what these must be placed from. The two agree exactly for anything
//!    unrotated, which is every card on the reference board.
//! 3. **No display mode.** Natively `CardMode::Link` centres the badge on the collapsed row.
//!    This viewer has no mode control and `crate::layout` draws every card the same way, so
//!    there is one geometry — top-right, inset by the card's own padding.
//!
//! # What is deliberately not here
//!
//! **A hover state.** Natively the badge inverts under the pointer. This viewer ignores a
//! `pointermove` from a device that never pressed — deliberately, and `input.rs` records the
//! phantom-contact bug that rule exists to prevent — so there is no hover to read. A hover that
//! only lit for a finger already holding the board down would be worse than none.
//!
//! **A cache, and a `retain_visible` to go with it.** Nothing here is tessellated or shaped
//! between frames: the plate and the arrow are quads and the triangle is three vertices built on
//! the spot. `text.rs`, `strokes.rs` and `shapes.rs` all need eviction because wasm linear memory
//! never returns to the OS and a zoom sweep otherwise leaves one cache entry per octave resident
//! for the life of the tab. This layer allocates nothing that outlives its frame, so adding the
//! discipline would be ceremony. It is stated rather than merely absent, so nobody adds it
//! looking for a symmetry that is not there.

use vellum_doc::ItemKind;
use vellum_project::look::CARD_PADDING;
use vellum_project::project::{Projected, Projection};
use vellum_project::theme::Theme;
use vellum_render::{DrawList, MeshTransform, QuadInstance, Rgba};
use vellum_scene::{Camera, ItemId as SceneId, WorldPoint, WorldRect};

// ---------------------------------------------------------------------------------------
// The measurements
//
// ⚠ Copied from `vellum-app/src/draw.rs`, where every one of them is private to that module.
// That is a second copy and this file does not pretend otherwise. They belong in
// `vellum_project::look`, whose own header states the test they meet exactly: "a number belongs
// here when both front ends need it and the board would look wrong if they disagreed". The hoist
// is not made here because `look.rs` is not this file's to edit — and the geometry below is
// arranged as free functions over plain numbers precisely so that the move is mechanical.
//
// `CARD_PADDING` is *not* copied: it already lives in `look` and is imported above. It decides
// where this badge is inset to **and** where `crate::layout` puts the card's picture and words,
// so a second copy of that one would let the badge drift away from the card around it.
// ---------------------------------------------------------------------------------------

/// The open-page badge's edge length, as a multiple of the card's font size.
///
/// Larger than a favicon because this one is a **target**, not a label: it has to be comfortable
/// to hit at a working zoom, where a favicon only has to be recognisable.
const BADGE_SCALE: f64 = 1.75;

/// How much of the badge the mark occupies, as a fraction of its edge. The rest is the plate's
/// margin — a glyph that reaches the rim reads as a box with a line in it rather than as a
/// button with something on it.
const BADGE_GLYPH: f64 = 0.46;

/// The plate's corner radius, as a fraction of its edge.
///
/// Miro's is a rounded square. It was a circle once, and the reasoning was sound — a disc never
/// lines up with the picture's own edges — and it produced something that read as a bubble stuck
/// onto the card rather than a control belonging to it.
const BADGE_RADIUS: f32 = 0.22;

/// The mark's stroke weight, as a fraction of the badge's edge. Thin, because it is a line
/// drawing rather than a filled arrow. Floored at one device pixel by the caller, which is
/// **not** the same as one world unit — see [`BadgeLayer::push`].
const BADGE_WEIGHT: f32 = 0.075;

/// The plate's rim, in **world** units, matching the desktop painter.
///
/// ⚠ Deliberately not floored at a device pixel the way the arrow's weight is, and that
/// asymmetry is the point. At a fitted 4% one device pixel is ~25 world units against a badge
/// some 20 world units across — a "hairline" that would swallow the plate whole and draw every
/// card's corner as a solid grey square. The arrow can take the floor because it is measured
/// against the same scale it is drawn at; a rim cannot, because it is measured against the
/// plate. So the rim simply fades out on a fitted board, where the whole badge is under a pixel
/// anyway, and the *arrow* is what keeps the badge legible at every zoom in between.
const BADGE_RIM: f32 = 1.0;

/// The most of a card's width a badge may take, whatever its type size says.
const BADGE_MAX_WIDTH_FRACTION: f64 = 0.22;

/// …and of its height.
const BADGE_MAX_HEIGHT_FRACTION: f64 = 0.5;

/// How much of the poster's short edge the ▶ takes.
///
/// A quarter: big enough to be an obvious target and to read as a play button at a fitted zoom,
/// small enough that the frame behind it — which is what tells you *which* video it is — stays
/// legible around it.
const PLAY_FRACTION: f64 = 0.25;

/// How much of the plate the triangle spans.
const PLAY_GLYPH: f64 = 0.44;

/// The play button's plate. Charcoal at 62%.
///
/// **Dark and semi-transparent rather than the accent**, deliberately: it sits on an arbitrary
/// photograph, and every video player in the world draws this mark that way — the one place
/// where following the convention beats following the palette.
const PLAY_PLATE: Rgba = Rgba::new(0.06, 0.07, 0.08, 0.62);

/// Below this the badge stops being a button and starts being a speck. A degenerate card gets no
/// badge rather than an unpressable one.
const SMALLEST_BADGE: f64 = 1.0;

// ---------------------------------------------------------------------------------------
// The geometry — pure, and the one answer to "where is it"
// ---------------------------------------------------------------------------------------

/// The badge's box in **card-local** units: origin at the card's top-left, y down.
///
/// ⚠ **Two clamps, and they guard different failures.** Getting either wrong was a real defect
/// found by review after the desktop tests were green (CLAUDE.md feedback 25).
///
/// - `side` is bounded by the width *and* the height as well as by the type scale. A card
///   dragged down to nothing would otherwise wear a badge larger than the item it belongs to.
/// - `top` is bounded separately, because **the inset is derived from the width while the size
///   is clamped by the height** — so on a wide, short card `pad` alone pushes the badge out
///   through the bottom even though `side` itself was clamped. Measured against this viewer's
///   own numbers: a 600×26 card has `pad` 21 and a badge side of 13, so the unclamped top of 21
///   puts its bottom edge at 34 — eight units below the card. Clamped, it sits at 13 and lands
///   exactly on the card's bottom edge.
///
/// The `height <= side` early return is what makes the second clamp safe rather than merely
/// plausible: it is the reason `height - side` below is known positive, so the `max(0.0)` on it
/// is genuinely belt and braces and not load-bearing. Said plainly because a guard described as
/// doing more than it does is how the *next* change removes the wrong one.
fn badge_local(width: f64, height: f64, font_size: f64) -> Option<(f64, f64, f64, f64)> {
    let pad = width * CARD_PADDING;
    let side = (font_size * BADGE_SCALE)
        .min(width * BADGE_MAX_WIDTH_FRACTION)
        .min(height * BADGE_MAX_HEIGHT_FRACTION);
    if side < SMALLEST_BADGE || height <= side {
        return None;
    }
    let top = pad.min((height - side).max(0.0));
    Some((width - pad - side, top, side, side))
}

/// The ▶'s box in **card-local** units, centred on the poster it sits over.
///
/// Sized from the picture band rather than from the type, because it is a target on an image and
/// has to stay proportionate to it. Clamped to the card for `badge_local`'s second reason: the
/// band is derived from the card's height and its own padding, and on a wide, short card that
/// arithmetic can put the band — and so the button centred on it — past the card's own edge,
/// where it would be painted outside the item and could never be pressed.
fn play_local(width: f64, height: f64, band: (f64, f64, f64, f64)) -> Option<(f64, f64, f64, f64)> {
    let (bx, by, bw, bh) = band;
    let side = (bw.min(bh) * PLAY_FRACTION).clamp(SMALLEST_BADGE, height.max(SMALLEST_BADGE));
    // No `side < SMALLEST_BADGE` here, deliberately: the clamp above has already established
    // it. A guard that cannot fire reads as protection and is not, which is worse than none —
    // it is what makes the *next* reader delete the clamp instead.
    if width < side || height < side {
        return None;
    }
    let x = (bx + (bw - side) / 2.0).clamp(0.0, (width - side).max(0.0));
    let y = (by + (bh - side) / 2.0).clamp(0.0, (height - side).max(0.0));
    Some((x, y, side, side))
}

/// Where a card's ↗ badge is, in **world** units, or `None` if it has no openable address.
///
/// ⚠ **The gate is the address, not the field.** `url.is_some()` is the tempting predicate and
/// it is the wrong one: an `Embed` that Miro itself failed to resolve carries a title and no
/// usable address, and a `mailto:` card is a perfectly ordinary thing to have on a board. Either
/// would get a button whose only possible answer is a refusal — which is the dead control this
/// function exists to prevent, arrived at by trusting the wrong predicate. Not hypothetical: it
/// shipped on the desktop and was found in review (CLAUDE.md feedback 25).
pub fn badge(projected: &Projected) -> Option<WorldRect> {
    openable(&projected.item.kind)?;
    badge_box(projected)
}

// A `pub fn play` mirroring `badge` was written here and removed: nothing outside this module
// needs the ▶'s box, because unlike the ↗ it does not push the words aside. Both the drawing
// and the press go through `play_box` directly, so the symmetry would have been a public
// function with no caller — which is the one defect this repository has found nine times.
// `plays_video`'s reasoning, which that doc carried, has moved onto `play_box` itself.

/// The address a press at this world point would open, ready for [`open_in_new_tab`].
///
/// **The ▶ and the ↗ open the same page**, so one predicate answers for both rather than two
/// arms that could come to disagree about which address a card has. They cannot overlap — the
/// badge is in a corner and the ▶ is centred on the poster — so the order is arbitrary; the ▶ is
/// asked first only because it is the larger target.
///
/// ⚠ **Unrotated, matching the painter.** Every piece of a card in this viewer is placed from the
/// axis-aligned `bounds` and drawn without rotation, so this looks for the buttons where they
/// are drawn. Undoing a rotation here would be tidier and would make the click disagree with the
/// screen, which is the one thing a hit test may never do.
///
/// The string that comes back is already normalised — see [`normalise`] — so the caller has
/// nothing left to decide.
pub fn pressed(projected: &Projected, at: WorldPoint) -> Option<String> {
    let url = openable(&projected.item.kind)?;
    let hit = play_box(projected, &url).is_some_and(|r| r.contains(at))
        || badge_box(projected).is_some_and(|r| r.contains(at));
    hit.then_some(url)
}

/// The badge's world rectangle, **without** the address gate.
///
/// Private, and every public entry point above resolves [`openable`] before calling it — which
/// is what keeps the gate and the geometry from being asked twice per card per frame while
/// leaving exactly one derivation of where the rectangle is. Calling this directly would draw a
/// button on a card that cannot be opened, which is the whole thing [`badge`] exists to refuse.
fn badge_box(projected: &Projected) -> Option<WorldRect> {
    let bounds = projected.bounds;
    let local = badge_local(bounds.width(), bounds.height(), card_font_size(projected)?)?;
    Some(to_world(bounds.min, local))
}

/// The ▶'s world rectangle, given an address already known to be openable.
///
/// The band comes from [`crate::layout::picture`], which is also what *draws* the poster — so
/// the ▶ is centred on the picture actually on screen rather than on one this module computed
/// for itself. That module answers `None` for an `Embed` even when the item carries a thumbnail,
/// because this viewer draws no picture for one, so a video `Embed` gets no ▶ here where the
/// desktop app would give it one. That is the painter being followed rather than a rule being
/// dropped: a ▶ floating over a block of text is a button aimed at nothing.
fn play_box(projected: &Projected, url: &str) -> Option<WorldRect> {
    if !vellum_link::plays_video(url) {
        return None;
    }
    let bounds = projected.bounds;
    let slot = crate::layout::picture(projected)?;
    // The picture slot is absolute world; the local arithmetic wants it relative to the card.
    let band = (
        slot.rect.min.x - bounds.min.x,
        slot.rect.min.y - bounds.min.y,
        slot.rect.width(),
        slot.rect.height(),
    );
    let local = play_local(bounds.width(), bounds.height(), band)?;
    Some(to_world(bounds.min, local))
}

/// The card's address if it is one this viewer will open, already normalised.
///
/// One definition, asked by [`badge`], `play_box`, [`pressed`] and [`BadgeLayer::push`] alike, so
/// a card cannot draw a button that the press path then declines to answer for.
///
/// [`vellum_link::host_of`] is the predicate, and it is the same one the desktop app's
/// `has_link` and `open_in_browser` both use — one derivation of the question *is this address a
/// web page*, which is the last thing that should exist twice. It refuses `mailto:`,
/// `javascript:`, `data:`, `file:` and `ftp:` by scheme before it will look for a host, which is
/// exactly the guarantee wanted before a string out of a board file reaches `window.open`.
fn openable(kind: &ItemKind) -> Option<String> {
    let url = match kind {
        ItemKind::LinkPreview { url, .. } | ItemKind::Embed { url, .. } => url.as_deref()?,
        _ => return None,
    };
    vellum_link::host_of(url)?;
    normalise(url)
}

/// A card's address as something `window.open` may be handed.
///
/// ⚠ **A browser needs one guarantee the desktop app does not.** `host_of` accepts a bare host
/// with no scheme — `example.com/thing` answers `Some("example.com")` — which is right for naming
/// a site and dangerous as an argument to `window.open`, because a browser resolves a schemeless
/// string **relative to the page**: the viewer would navigate to `https://velmd.host/example.com`
/// and show its own 404. So a schemeless address is given the scheme a browser's own address bar
/// would give it rather than being refused, which keeps the badge on exactly the cards the
/// desktop app puts one on.
///
/// The final check is belt and braces on the one path in this crate that hands a string out of a
/// board file to the browser: whatever route got here, the result starts `http://` or `https://`
/// or nothing is returned. `host_of` has already refused `javascript:` and `data:` by scheme —
/// this is the assertion that stays true if that ever changes.
fn normalise(url: &str) -> Option<String> {
    let url = url.trim();
    let normalised = if has_web_scheme(url) { url.to_owned() } else { format!("https://{url}") };
    has_web_scheme(&normalised).then_some(normalised)
}

/// Whether this address names the web, by scheme.
///
/// ⚠ **Compared as bytes against an ASCII needle, never by slicing the `str`.** The obvious
/// spelling is `url.get(..8)`, and it is wrong in the way CLAUDE.md feedback 30 is about: `http://`
/// is seven bytes, so on an internationalised host — `http://ünicode.example` — byte 8 falls
/// *inside* the first character, `get` answers `None`, and a perfectly good address is silently
/// refused a badge. A scheme is ASCII by definition, so comparing the leading bytes is exact,
/// cannot split a character, and cannot panic.
fn has_web_scheme(url: &str) -> bool {
    let bytes = url.as_bytes();
    let scheme = |prefix: &[u8]| {
        // `>`, not `>=`: a bare `https://` names no page.
        bytes.len() > prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    scheme(b"http://") || scheme(b"https://")
}

/// The type size this card's words are actually drawn at.
///
/// ⚠ **Asked of [`crate::layout`] rather than recomputed.** That module owns what size a card
/// sets its text at, and the badge is sized from the type so that it grows with the card the way
/// the words do. Copying the expression would be a second derivation of one measurement, which
/// is what `vellum_project::look`'s header calls the failure that *"is never that the copies
/// disagree on the day they are written; it is that one of them is changed a year later."*
///
/// ⚠ **Through `crate::layout::card_font_size`, never through `text_slot`.** `crate::card` asks
/// *this* module where the badge is, in order to shorten a title that would sit under it — so a
/// route back through the slot function would close a cycle, and under `panic = "abort"` an
/// unbounded recursion is not an error anybody sees, it is the tab dying on the frame a link
/// card first became visible. `card_font_size` exists to be the one derivation all three read.
fn card_font_size(projected: &Projected) -> Option<f64> {
    crate::layout::card_font_size(projected).map(f64::from)
}

/// Card-local `(x, y, w, h)` to an absolute world rectangle.
fn to_world(origin: WorldPoint, (x, y, w, h): (f64, f64, f64, f64)) -> WorldRect {
    WorldRect::from_origin_size(WorldPoint::new(origin.x + x, origin.y + y), w, h)
}

// ---------------------------------------------------------------------------------------
// The paint
// ---------------------------------------------------------------------------------------

/// The per-frame drawing of a card's two buttons.
///
/// Stateless, and it stays that way — see the module header on why there is no cache here and no
/// `retain_visible` beside it.
#[derive(Default)]
pub struct BadgeLayer;

impl BadgeLayer {
    pub fn new() -> Self {
        Self
    }

    /// Draw one item's badge and play button, if it has them. Answers whether anything was
    /// pushed.
    ///
    /// The list must be in the **board** view: the origin is camera-relative and the extent is in
    /// world units.
    ///
    /// ⚠ **Push this after the card's picture, not before.** The order things are pushed in *is*
    /// the paint order, which is why `frame()` is one loop dispatching per kind rather than
    /// several passes. The badge sits on top of the poster it overlaps, and the ▶ is centred on
    /// one.
    ///
    /// ⚠ **`false` is not a cue to draw the item some other way.** It means the card has no
    /// openable address, or is too small to hold a button — and nothing is the honest drawing of
    /// both.
    pub fn push(
        &mut self,
        list: &mut DrawList,
        camera: &Camera,
        id: SceneId,
        projection: &Projection,
        theme: &Theme,
    ) -> bool {
        let Some(projected) = projection.get(id) else { return false };
        // Resolved once per card per frame, then handed to both boxes. The public [`badge`] and
        // `play_box` each resolve it for themselves, which is right for a caller holding only a
        // `Projected` and would be two more allocations per card per frame here — on a board
        // that is 91 cards, which is what the reference board is.
        let Some(url) = openable(&projected.item.kind) else { return false };
        let opacity = projected.opacity();

        // One device pixel in the units this list is drawing in.
        //
        // ⚠ **Not the literal `1.0`.** This list is in the board view, where a unit is a *world*
        // unit — so flooring the arrow's weight at one world unit floors it at less than a pixel
        // below zoom 1, and a fitted board is typically well under zoom 1. On exactly the view a
        // whole board is normally looked at, the plate would survive and the arrow inside it go
        // sub-pixel and vanish: *"a badge with no glyph looks like a rendering fault"*, produced
        // by the line that exists to prevent it. That is CLAUDE.md feedback 38's bug, avoided
        // here by deriving the floor from the live zoom at the one place that knows it.
        let zoom = camera.zoom();
        let pixel = if zoom > 0.0 { (1.0 / zoom) as f32 } else { 1.0 };

        let mut drew = false;
        // The ▶ first, so the ↗ is on top if a degenerate card ever puts them in one place.
        if let Some(rect) = play_box(projected, &url) {
            push_play(list, camera, rect, opacity, theme);
            drew = true;
        }
        if let Some(rect) = badge_box(projected) {
            push_badge(list, camera, rect, opacity, theme, pixel);
            drew = true;
        }
        drew
    }
}

/// The ↗: a rounded-square plate and a thin dark box-with-an-arrow-leaving-it on top of it.
///
/// # ⚠ The mark is geometry, and that is not negotiable
///
/// U+2197 (↗) and U+29C9 (⧉) both live outside every face bundled with this build — Noto Sans has
/// neither, and `font_family: None` resolves through fontdb's `sans-serif` alias to exactly that
/// face (CLAUDE.md trap 10). A card asked to shape one draws a tofu box on the single control
/// whose entire job is to look pressable. Seven quads always draw. This list has no path
/// primitive either — everything here is a quad — which is the other half of the same answer.
///
/// # Miro's mark, not Velm's old one
///
/// This is deliberately *not* the heavy accent-coloured arrow on a disc that shipped first. On a
/// build where the user has chosen the blue accent that made the badge the loudest thing on a
/// board made mostly of cards — *"make the outgoing link more like miro as well please"*. Miro's
/// is a thin near-black outline of a box with an arrow leaving through its missing top-right
/// corner, on a rounded-square plate, and it reads as a control rather than as decoration. The
/// ink is `text_muted`, the same colour the card's site name uses, so the two pieces of card
/// chrome agree.
fn push_badge(
    list: &mut DrawList,
    camera: &Camera,
    rect: WorldRect,
    opacity: f32,
    theme: &Theme,
    pixel: f32,
) {
    let origin = camera.to_camera_relative(rect.min);
    let (w, h) = (rect.width() as f32, rect.height() as f32);
    let side = w.min(h);
    if side <= 0.0 {
        return;
    }

    list.push_quad(
        QuadInstance::solid(origin, [w, h], theme.surface)
            .with_corner_radius(side * BADGE_RADIUS)
            .with_border(theme.border, BADGE_RIM)
            .with_opacity(opacity),
    );

    // Half the mark's extent, so the unit coordinates below run -1..1 about the centre.
    let half = side * (BADGE_GLYPH as f32) * 0.5;
    // At least one device pixel: below that the mark stops being drawn at all rather than being
    // drawn faintly, and a badge with no glyph looks like a rendering fault.
    let weight = (side * BADGE_WEIGHT).max(pixel);
    let centre = [origin[0] + w * 0.5, origin[1] + h * 0.5];
    let ink = theme.text_muted;

    // One stroked segment between two points of the unit box, as a rotated quad.
    let mut seg = |a: (f32, f32), b: (f32, f32)| {
        let (ax, ay) = (centre[0] + a.0 * half, centre[1] + a.1 * half);
        let (bx, by) = (centre[0] + b.0 * half, centre[1] + b.1 * half);
        let (dx, dy) = (bx - ax, by - ay);
        // Plus one weight so the round caps land *on* the ends rather than short of them, which
        // is what closes the corners of the box below without mitring anything.
        let length = dx.hypot(dy) + weight;
        list.push_quad(
            QuadInstance::solid(
                [(ax + bx) * 0.5 - length * 0.5, (ay + by) * 0.5 - weight * 0.5],
                [length, weight],
                ink,
            )
            .with_corner_radius(weight * 0.5)
            // The segment's own angle, and **nothing else** — the card is not rotated here. See
            // the module header's first divergence.
            .with_rotation(dy.atan2(dx))
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
    // The arrow through the gap, and its two barbs. They run straight left and straight down from
    // the tip — an arrowhead on a 45° shaft is axis-aligned, which is why this needs no
    // trigonometry beyond what `seg` already does.
    seg((-0.17, 0.17), (0.75, -0.75));
    seg((0.25, -0.75), (0.75, -0.75));
    seg((0.75, -0.75), (0.75, -0.25));
}

/// The ▶ over a video card's poster: a dark disc and a triangle.
///
/// # A mesh, unlike the badge's arrow
///
/// The same reasoning arrived at for a harder glyph: `▶` is U+25B6, outside the plain sans faces
/// (trap 10), so a card asked to shape it draws tofu wherever the fallback chain misses a symbols
/// face. But a triangle is not expressible as axis-aligned rectangles at all, so it goes through
/// the same `MeshBatch` that carries ink and connectors — which is multisampled, so its diagonals
/// are smooth rather than stepped. A triangle at this size would show that badly.
fn push_play(list: &mut DrawList, camera: &Camera, rect: WorldRect, opacity: f32, theme: &Theme) {
    let origin = camera.to_camera_relative(rect.min);
    let (w, h) = (rect.width() as f32, rect.height() as f32);
    let side = w.min(h);
    if side <= 0.0 {
        return;
    }

    list.push_quad(
        QuadInstance::solid(origin, [w, h], PLAY_PLATE.with_alpha(PLAY_PLATE.a * opacity))
            .with_corner_radius(side * 0.5),
    );

    let centre = [origin[0] + w * 0.5, origin[1] + h * 0.5];
    let reach = side * 0.5 * (PLAY_GLYPH as f32);
    // Nudged right so it sits optically centred: a triangle centred on its bounding box reads as
    // left-of-centre, because its mass is toward the flat edge.
    let nudge = reach * 0.18;
    // ⚠ **Relative to the centre, because the transform below translates by it.** These were
    // absolute on the desktop once, so the translation added the centre a *second* time and flung
    // the triangle to twice its own offset — photographed as a stray play mark in the empty board
    // above the card, with the plate sitting correctly on the poster.
    let tip = [reach + nudge, 0.0];
    let top = [-reach + nudge, -reach];
    let bottom = [-reach + nudge, reach];
    let ink = theme.surface.with_alpha(opacity);

    // A translation and no rotation, and the identity here is the *correct* answer rather than an
    // omission — see the module header. The card under this triangle is drawn axis-aligned, so a
    // turned triangle would be the bug the desktop app fixed by turning it.
    let transform = list.meshes_mut().push_transform(MeshTransform::at(centre));
    let start = list.meshes().indices().len() as u32;
    list.meshes_mut().push_indexed(&[top, tip, bottom], &[0, 1, 2], ink, transform);
    let end = list.meshes().indices().len() as u32;
    if end == start {
        // A transform was pushed and nothing referenced it. Pushing an empty range instead would
        // open a mesh batch for zero triangles and split the batch either side of it.
        return;
    }
    list.push_meshes(start..end);
}

// ---------------------------------------------------------------------------------------
// The press
// ---------------------------------------------------------------------------------------

/// Open a card's page in a new tab. Answers whether the browser took it.
///
/// ⚠ **This must be called from inside the pointer handler, synchronously.** `window.open` needs
/// transient user activation, which a `pointerup` grants and a `requestAnimationFrame` callback
/// does not reliably carry — and a popup blocker refuses it **silently**, with no exception and
/// nothing in the console, so the badge would simply do nothing and there would be no evidence
/// anywhere saying why. `input.rs`'s handlers already act on the camera directly rather than
/// queueing for the next frame, so there is nothing in the way of doing this correctly; it is
/// written down because the alternative fails invisibly, which is the worst way for a control to
/// fail.
///
/// `_blank` rather than navigating this tab: the viewer holds a board someone is reading, and
/// replacing it with a shopping page on a mis-tap would be a hostile answer to a fat finger.
///
/// The URL has already been through [`normalise`], so this cannot be handed a `javascript:` or
/// `data:` string — the browser's equivalent of the hazard the desktop app's `open_in_browser`
/// refuses by scheme, and for the same reason: this is a string out of a board file. The check is
/// repeated here rather than assumed, because this function is `pub` and the next caller will not
/// have read the one above it.
pub fn open_in_new_tab(url: &str) -> bool {
    if !has_web_scheme(url) {
        log::warn!("refusing to open an address that is not a web page");
        return false;
    }
    let Some(window) = web_sys::window() else { return false };
    match window.open_with_url_and_target(url, "_blank") {
        // `Ok(None)` is a blocked popup, which is a real outcome rather than an error: it is what
        // a browser answers when it does not believe a gesture was behind the call. Told apart
        // from a thrown error because the two have entirely different fixes.
        Ok(Some(_)) => true,
        Ok(None) => {
            log::warn!("the browser blocked opening the page — this needs a real tap or click");
            false
        }
        Err(_) => {
            log::warn!("the browser refused to open the page");
            false
        }
    }
}
