//! Resize and rotate handles — the browser's half of `vellum_app`'s `handle.rs` wiring.
//!
//! # What this is, and what it deliberately is not
//!
//! A selection in a tab draws a ring and nothing else: it can be picked up and moved
//! ([`crate::edit`]) and it can be given numbers through the panel ([`crate::style`]), but it
//! cannot be reshaped by hand. This is the gesture that closes that — a press on a grip, a
//! drag that previews, a release that commits **one** undo step.
//!
//! **None of the geometry is here.** Where a handle sits, what a press lands on and what a
//! drag of one does to a [`Placement`] all live in [`vellum_project::handle`], which is pure,
//! has ~30 tests, and is the *same* module `vellum-app` drives. That is the whole point: two
//! front ends that disagree about where a corner is are two front ends that disagree about
//! where a click lands, and this repository has paid for a second copy of a layout more than
//! once. This file is the state a gesture carries, the two hops into the document, and the
//! draw call — nothing that could be tested without a browser.
//!
//! ⚠ **[`vellum_project::handle`] does not exist until the move happens.** `handle.rs` is
//! still in `vellum-app` at the time of writing and this file does not compile until it is
//! moved. The move is byte-for-byte free — see the report accompanying this file, and
//! `vellum-app/src/lib.rs`'s existing `pub use vellum_project::{connector, project, theme};`
//! for the shape it takes.
//!
//! # Nothing here can be tested, and what is done about it
//!
//! `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so **no test in this crate is ever
//! compiled**. `cargo test --workspace` will say nothing about this file, which is not the
//! same as passing. Two things answer that:
//!
//! - Every decision worth asserting was pushed into `vellum_project::handle` *by not being
//!   written here at all*. The arithmetic in [`Drag::placement_of`] is the desktop's
//!   `Drag::placement_of` arm for arm, and each arm is one call.
//! - [`velm_handle_report`] and [`velm_handle_points`] are the honest check that is
//!   available, on the `velm_edit_report` precedent: a fixture dispatches real pointer events
//!   at the real listeners and then asks what happened. `velm_handle_points` exists so a
//!   fixture can aim at a grip *by asking where it is* rather than re-deriving the layout in
//!   JavaScript — a second copy of the layout is precisely the failure this module opens by
//!   refusing.
//!
//! # ⚠ The four traps this file exists downstream of
//!
//! **A multi-selection gets four corners and rotate, never edge handles.** An edge drag
//! scales one axis, and one-axis scaling of a *rotated* member asks for a sheared rectangle —
//! a [`Placement`] is an axis-aligned box plus a rotation, and no value of one is a shear.
//! [`handle::GROUP_HANDLES`] is the list and it is what this file draws and hit-tests, so the
//! restriction cannot be lost by a paint path and a press path drifting apart.
//!
//! **The group box is captured at the press.** It is derived *from* the members, so
//! recomputing it per frame would feed each scaled box into the next scale and the selection
//! would run away from the pointer exponentially. [`Drag::group`] holds the box and the angle
//! the pointer was at when the button went down, and neither is written again. The box the
//! *painter* draws while a drag is live is derived from that captured one too
//! ([`Drag::group_now`]) rather than re-measured — not for correctness of the transform,
//! which is already safe, but because a re-measured box under a rotating selection swells and
//! shrinks and the grips move out from under the finger holding one.
//!
//! **A resize moves two things per member, and a rotation moves two.** A member's size
//! *and* its distance from the held corner; its own angle *and* its centre travelling around
//! the group's. Both are `handle::scale_member` / `handle::rotate_member`, and the reason
//! they are separate functions from the single-item ones is that a group transform is a
//! different operation rather than a bigger version of the same one.
//!
//! **A locked item is not resized or rotated** — and it is filtered in *both* the paint path
//! and the press path here, from one function ([`offer`]), so the grips cannot be drawn
//! around a box the press answers differently about.
//!
//! # ⚠ The undo group
//!
//! One per gesture, opened once at the release and never during the drag. `crate::edit`'s
//! [`crate::edit::grouped`] is this crate's chokepoint and this file uses it rather than
//! opening a group of its own: its closure returns `T` rather than `Result<T>`, so a `?`
//! cannot escape between begin and end, which is trap 11 made unrepresentable instead of
//! merely rescued.
//!
//! # ⚠ World units, and screen constants divided by the zoom
//!
//! Everything crossing into the draw list is in **world** units, in the board view. A grip's
//! side is a number of *device* pixels **divided** by the zoom, exactly as the selection ring
//! above it is — multiplying by the zoom is the mistake that made every image on the web
//! client draw at 6% of its own box.
//!
//! # ⚠ `panic = "abort"`
//!
//! On wasm an abort takes the tab down with no message and no stack. There is no `unwrap`, no
//! `expect`, no slice index and no unchecked arithmetic anywhere in this file.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;

use vellum_doc::{Board, ItemId as DocId, ItemKind, Placement};
use vellum_project::handle::{self, Group, Handle};
use vellum_project::project::Projection;
use vellum_project::theme::Theme;
use vellum_render::{DrawList, QuadInstance, Rgba};
use vellum_scene::{Camera, ItemId as SceneId, WorldPoint};

use crate::edit::EditState;

/// How thick a grip's outline is drawn, in **device** pixels.
///
/// Two, and it is deliberately *not* `crate::SELECTION_RING_WIDTH`, which is one. Feedback 32
/// is the whole reason those are two numbers: the user photographed a board with everything
/// selected — *"the border on the selected items are too thick"* — and the ring alone was
/// thinned, because a ring holds its device width while the item under it shrinks with the
/// zoom, so at a fitted camera two pixels is a band *around* a small item. A grip is not a
/// band around anything; it is an 8-pixel square you have to see in order to aim at. Sizing
/// it from the ring's constant would silently undo the distinction that change established.
const HANDLE_BORDER: f64 = 2.0;

/// How far a press may be from a grip's centre and still count, for a **finger**, in
/// **CSS** pixels.
///
/// ⚠ **A web-only addition, and it is not in the desktop's numbers.** `handle::GRAB_SLOP`
/// gives a 9-device-pixel reach, which is right for a mouse and is roughly four CSS pixels on
/// a retina tablet — a target no finger can hit. A finger's target is a *physical* size, so
/// this is expressed in CSS pixels and multiplied by the device ratio at the press, which is
/// why [`Aim`] carries the ratio: 22 here is a 44-pixel diameter, which is the smallest
/// target Apple's own guidance accepts.
const TOUCH_REACH_CSS: f64 = 22.0;

/// The largest fraction of a selection's shorter on-screen side a grip's reach may take.
///
/// ⚠ **Also a web-only addition, and it is what keeps a small item movable.** Nine grips each
/// reaching 22 CSS pixels around a selection only 40 CSS pixels across leaves no part of that
/// selection that is not a grip — so on a touchscreen the item could be resized from anywhere
/// and *moved* from nowhere, which is the more common gesture made unreachable by the rarer
/// one. A quarter is below the half a centre-to-edge-midpoint distance is, so a core that
/// still starts a move is guaranteed to exist at every size; on anything large the cap does
/// not bind at all and the full reach applies.
const REACH_FRACTION: f64 = 0.25;

/// What a pointer is, for deciding how big a grip is to aim at.
///
/// Two fields rather than a bare `bool` because a finger's target is a physical size and the
/// camera works in device pixels — trap 4. `device_ratio` is the same `devicePixelRatio` the
/// press site in `crate::input` has already read to convert the event's own coordinates, so
/// it is handed in rather than read again: one conversion, in one place, is that module's own
/// stated rule.
#[derive(Debug, Clone, Copy)]
pub struct Aim {
    /// A finger. `false` for a mouse or a pen, which are precise.
    pub coarse: bool,
    /// CSS pixels to device pixels.
    pub device_ratio: f64,
}

impl Aim {
    /// The reach, in **world** units, for a selection whose shorter on-screen side is
    /// `extent` device pixels.
    ///
    /// `max` against the mouse reach so a 1x touchscreen never gets a *smaller* target than a
    /// mouse would; the fraction caps it from above so a small item keeps a movable core.
    fn reach(self, zoom: f64, extent_device: f64) -> f64 {
        let precise = f64::from(handle::HANDLE_SIZE / 2.0 + handle::GRAB_SLOP);
        let wanted = if self.coarse {
            (TOUCH_REACH_CSS * self.device_ratio.max(1.0)).max(precise)
        } else {
            precise
        };
        let capped = wanted.min((extent_device * REACH_FRACTION).max(precise));
        capped / zoom.max(f64::EPSILON)
    }
}

// ---------------------------------------------------------------------------------------
// What is on offer — the one layout the painter and the press path share.
// ---------------------------------------------------------------------------------------

/// The grips a selection is currently offering, if any.
///
/// ⚠ **One function answers this for the painter and for the press.** A second derivation is
/// a grip drawn where a press is not answered, which is `draw::kanban_runs`' rule in this
/// repository and the reason that function is `pub(crate)` rather than private. Here it also
/// carries the two refusals — a locked item and a connector — so neither can be enforced in
/// one path and forgotten in the other.
#[derive(Debug, Clone, Copy)]
pub enum Offer {
    /// Nothing selected, nothing transformable, or editing is off.
    None,
    /// One item: eight resize grips on its own rotated box, plus rotate.
    One {
        placement: Placement,
        /// Radians clockwise, so a grip drawn on a turned item turns with it.
        rotation: f32,
    },
    /// More than one: four corners and rotate on a shared axis-aligned box. **No edges** —
    /// see [`handle::GROUP_HANDLES`].
    Group(Group),
}

impl Offer {
    /// Where every offered grip sits, in world space.
    ///
    /// The single derivation. `zoom` is needed because the rotate grip's stand-off is a
    /// constant number of *screen* pixels and has to shrink in world units as the board is
    /// zoomed in.
    #[must_use]
    pub fn positions(&self, zoom: f64) -> Vec<(Handle, WorldPoint)> {
        match self {
            Self::None => Vec::new(),
            Self::One { placement, .. } => handle::positions(placement, zoom),
            Self::Group(group) => handle::group_positions(group, zoom),
        }
    }

    /// The shorter side of the box the grips belong to, in world units.
    ///
    /// What [`Aim::reach`] caps against, converted to device pixels by the caller. A rotated
    /// item's *drawn* size is used rather than its rotated envelope: the grips sit on the
    /// item's own edges, so it is the item's own sides they must leave a core inside.
    fn extent(&self) -> f64 {
        match self {
            Self::None => 0.0,
            Self::One { placement, .. } => {
                let (w, h) = placement.scaled_size();
                w.min(h)
            }
            Self::Group(group) => group.width.min(group.height),
        }
    }

    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    const fn is_group(&self) -> bool {
        matches!(self, Self::Group(_))
    }
}

/// What the selection offers right now, with every refusal applied once.
///
/// # The refusals, and why each one is here rather than at a call site
///
/// - **Editing off.** The whole layer is inert until [`crate::edit::velm_set_editing`] is
///   called, exactly as `crate::edit` is, so wiring this cannot regress the viewer.
/// - **A gesture already in flight.** While `crate::edit` is moving or sweeping, a second set
///   of grips is a second gesture offered during the first — and during a marquee the
///   selection changes on every pointer move, so the grips would flicker across the board.
/// - **A locked item gets no grips.** A resize is the one gesture that would otherwise still
///   reach one, because grips are hit-tested before the scene is asked anything at all.
/// - **A connector gets no grips.** Its box is derived from its bindings rather than chosen,
///   so eight resize grips on it are eight grips that move nothing a user can see, and a
///   rotate grip turns the line without turning what it joins. The desktop offers two round
///   end grips instead; this client draws none, so it must also *answer* none — a press path
///   that resized an invisible rectangle is worse than a missing feature.
/// - **Fewer than two unlocked members** in a multi-selection: there is no group box to
///   transform against. The desktop applies the same floor.
#[must_use]
pub fn offer(edit: &EditState, projection: &Projection) -> Offer {
    if !edit.enabled() || edit.is_dragging() {
        return Offer::None;
    }
    let selection = edit.selection();
    match selection {
        [] => Offer::None,
        [only] => {
            let Some(projected) = projection.get(*only) else { return Offer::None };
            if projected.item.style.locked
                || matches!(projected.item.kind, ItemKind::Connector { .. })
            {
                return Offer::None;
            }
            Offer::One { placement: projected.item.placement, rotation: projected.rotation() }
        }
        many => {
            let placements: Vec<Placement> = many
                .iter()
                .filter_map(|id| projection.get(*id))
                .filter(|projected| !projected.item.style.locked)
                .map(|projected| projected.item.placement)
                .collect();
            if placements.len() < 2 {
                return Offer::None;
            }
            handle::group_bounds(&placements).map_or(Offer::None, Offer::Group)
        }
    }
}

/// The grip nearest a world point within `reach`, or `None`.
///
/// **Nearest, not first.** Zoomed out the reach is large in world units — hundreds of them at
/// a fitted 4% — so several grips are routinely inside it at once, and taking the first would
/// answer by array order rather than by aim. `min_by` keeps the first of equal distances, so
/// [`Handle::ALL`]'s corners-first order still decides an exact tie between a corner and the
/// two edges meeting it, which is what someone aiming at a corner meant.
///
/// `total_cmp` rather than `partial_cmp().unwrap()`: a `NaN` extent out of a corrupt document
/// is a dead tab under `panic = "abort"`, and this orders rather than refuses.
fn nearest(places: &[(Handle, WorldPoint)], at: WorldPoint, reach: f64) -> Option<Handle> {
    places
        .iter()
        .map(|(grip, place)| (*grip, (place.x - at.x).hypot(place.y - at.y)))
        .filter(|(_, distance)| *distance <= reach)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(grip, _)| grip)
}

// ---------------------------------------------------------------------------------------
// The gesture.
// ---------------------------------------------------------------------------------------

/// Which verb a grip started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Resize(Handle),
    Rotate,
}

/// The transform in flight.
#[derive(Debug, Clone)]
struct Drag {
    mode: Mode,
    /// Every member, with the placement it had **when the press landed**.
    ///
    /// ⚠ The originals rather than the last frame's positions, and that is what makes the
    /// drag idempotent: `crate::live` can rebuild the projection under a gesture at any
    /// moment — its timer is not the frame loop — which throws the preview away. Because
    /// every [`Drag::preview`] rewrites `original` transformed by the *whole* travel since
    /// the press, the very next pointer move repaints it correctly and the stomp is
    /// invisible. Make it incremental and a poll landing mid-drag silently halves the gesture.
    items: Vec<(SceneId, DocId, Placement)>,
    /// The shared box for a multi-selection, and the angle the pointer stood at when the
    /// button went down.
    ///
    /// ⚠ **Captured, never recomputed.** See the module header: the box is derived from the
    /// members, so re-measuring it per frame feeds each scaled box into the next scale.
    group: Option<(Group, f64)>,
    /// Only a lone image keeps its proportions from a corner. A group already scales
    /// uniformly through `handle::group_scale`, so asking again would be redundant, and a
    /// mixed selection has no one ratio to keep.
    lock_aspect: bool,
    press: WorldPoint,
    at: WorldPoint,
    /// ⇧ — snaps a rotation to `handle::SNAP_DEGREES`. Read live from the move event rather
    /// than latched at the press, so it can be pressed and released mid-gesture.
    snap: bool,
}

impl Drag {
    /// The whole travel since the press, in world units. Never an accumulated step.
    fn offset(&self) -> (f64, f64) {
        (self.at.x - self.press.x, self.at.y - self.press.y)
    }

    /// Where one member ends up, given how far the gesture has travelled.
    ///
    /// ⚠ **The single place this is worked out.** The live preview and the commit both ask
    /// here, so a member cannot land anywhere other than where it was last drawn. The
    /// desktop's `Drag::placement_of` records that those two used to hold separate copies and
    /// that a resize would have made them disagree the moment either was edited; every arm
    /// below is that function's arm, unchanged, and each is one call into the shared geometry.
    fn placement_of(&self, original: &Placement, pointer: WorldPoint) -> Placement {
        match self.mode {
            Mode::Resize(grip) => match self.group {
                // A group scales uniformly about the corner opposite the one held; a single
                // item resizes along its own axes. Different operations, so the group case is
                // not a special case of the other.
                Some((group, _)) => {
                    let (factor, fixed) = handle::group_scale(&group, grip, self.offset());
                    handle::scale_member(original, factor, fixed)
                }
                None => handle::resize(original, grip, self.offset(), self.lock_aspect),
            },
            Mode::Rotate => match self.group {
                Some((group, from)) => {
                    let centre = (group.x, group.y);
                    let mut degrees = handle::angle_to(centre, pointer) - from;
                    if self.snap {
                        degrees =
                            (degrees / handle::SNAP_DEGREES).round() * handle::SNAP_DEGREES;
                    }
                    handle::rotate_member(original, centre, degrees)
                }
                None => handle::rotate(original, pointer, self.snap),
            },
        }
    }

    /// The group box as it stands now, for the **painter only**.
    ///
    /// ⚠ Derived from the captured box rather than re-measured from the previewing members,
    /// and the distinction matters for a reason the transform's own safety does not cover:
    /// re-measuring cannot corrupt the transform (that reads [`Drag::group`], which never
    /// changes), but the axis-aligned envelope of a *rotating* selection swells and shrinks
    /// as its members turn — so the grips would crawl out from under the finger holding one.
    /// A rotation leaves the box alone because the box is the frame you are turning things
    /// inside, and it has no rotation of its own to gain.
    fn group_now(&self) -> Option<Group> {
        let (group, _) = self.group?;
        match self.mode {
            Mode::Resize(grip) => {
                let (factor, fixed) = handle::group_scale(&group, grip, self.offset());
                Some(Group {
                    x: fixed.0 + (group.x - fixed.0) * factor,
                    y: fixed.1 + (group.y - fixed.1) * factor,
                    width: group.width * factor,
                    height: group.height * factor,
                })
            }
            Mode::Rotate => Some(group),
        }
    }

    /// Writes the preview into the projection.
    ///
    /// ⚠ **`Projection::moved`, not the document.** Every frame of a drag would otherwise be
    /// a Loro transaction plus a full reprojection for a shape the user has not committed to.
    /// `moved` is an `O(log n)` remove-and-reinsert in the R-tree that keeps the index, the
    /// hit-test and the painter in step, which is why the painter needs no offset of its own:
    /// the members are *already* at the previewed size and angle.
    fn preview(&self, projection: &mut Projection) {
        for (scene, _, original) in &self.items {
            projection.moved(*scene, self.placement_of(original, self.at));
        }
    }

    /// Puts every member back where the press found it.
    fn restore(&self, projection: &mut Projection) {
        for (scene, _, original) in &self.items {
            projection.moved(*scene, *original);
        }
    }
}

/// The grip gesture, if there is one. A field on `crate::Viewer`, beside `edit`.
#[derive(Debug, Default)]
pub struct Handles {
    drag: Option<Drag>,
}

impl Handles {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What is on offer this instant, taking a live gesture into account.
    ///
    /// The press path asks [`offer`] directly — no gesture can be in flight at a press. The
    /// painter asks this, so that while a grip is held the grips stay on the box being
    /// transformed rather than on a re-measured one.
    #[must_use]
    pub fn offer_now(&self, edit: &EditState, projection: &Projection) -> Offer {
        match self.drag.as_ref() {
            Some(drag) => match drag.group_now() {
                Some(group) => Offer::Group(group),
                // A single member: the projection already holds its previewed placement, so
                // reading it is what makes the grips follow the shape as it is reshaped.
                None => drag
                    .items
                    .first()
                    .and_then(|(scene, _, _)| projection.get(*scene))
                    .map_or(Offer::None, |projected| Offer::One {
                        placement: projected.item.placement,
                        rotation: projected.rotation(),
                    }),
            },
            None => offer(edit, projection),
        }
    }

    /// A press at a world point. Answers whether a grip took it.
    ///
    /// `false` means "not mine" and the caller must carry on to `crate::edit` exactly as it
    /// does today, which is what makes wiring this **additive**: with editing off every
    /// answer is `false` and the viewer behaves as it did before this file existed.
    ///
    /// ⚠ **Grips beat items.** This has to be asked *before* `crate::edit::pointer_down`,
    /// because a grip sits on an item's own outline and a press there would otherwise pick
    /// the item up and slide it. It is also why a live gesture is cancelled first: a press
    /// arriving while one is still in flight means a release was lost — routine on a
    /// touchscreen — and starting a second on top of the first would leave the members
    /// previewed at a shape nothing puts back.
    pub fn press(
        &mut self,
        at: WorldPoint,
        aim: Aim,
        edit: &EditState,
        projection: &mut Projection,
        zoom: f64,
    ) -> bool {
        self.cancel(projection);
        if !edit.enabled() {
            return false;
        }
        let offered = offer(edit, projection);
        if offered.is_none() {
            return false;
        }
        let reach = aim.reach(zoom, offered.extent() * zoom);
        let Some(grip) = nearest(&offered.positions(zoom), at, reach) else { return false };

        let items: Vec<(SceneId, DocId, Placement)> = edit
            .selection()
            .iter()
            .filter_map(|scene| {
                let projected = projection.get(*scene)?;
                // A locked item cannot be *picked* any more, but a selection made before it
                // was locked survives — so the gesture filters too, or locking something
                // mid-selection would leave it reshapable until it was deselected.
                if projected.item.style.locked {
                    return None;
                }
                Some((*scene, projected.doc_id, projected.item.placement))
            })
            .collect();
        if items.is_empty() {
            return false;
        }

        // ⚠ Captured once, from the placements this gesture will actually write. Recomputing
        // per frame is the exponential runaway; taking it from the *whole* selection rather
        // than the unlocked members would make the drawn box and the transformed box two
        // different rectangles.
        let group = if items.len() > 1 {
            let placements: Vec<Placement> = items.iter().map(|(_, _, p)| *p).collect();
            handle::group_bounds(&placements)
                .map(|group| (group, handle::angle_to((group.x, group.y), at)))
        } else {
            None
        };
        // Only a lone image keeps its ratio from a corner: *"if i change the size holding
        // from the corners it should not change the aspect ratio … it should only distort the
        // image if i am changing it by holding from the sides or the top"*. Everything else
        // is a box you size to its content, and forcing a ratio there fights the common case.
        let lock_aspect = items.len() == 1
            && items.iter().any(|(scene, _, _)| {
                projection
                    .get(*scene)
                    .is_some_and(|p| matches!(p.item.kind, ItemKind::Image { .. }))
            });

        // ⚠ **No widening to contents, and that is not an omission.** A move carries what is
        // parented to it; a resize does not scale a frame's contents, and Miro's does not
        // either — a frame is a viewport over the board, so growing one reveals more rather
        // than magnifying what is on it.
        let mode = if grip.is_rotate() { Mode::Rotate } else { Mode::Resize(grip) };
        self.drag = Some(Drag { mode, items, group, lock_aspect, press: at, at, snap: false });
        true
    }

    /// The pointer moved with the button down. Answers whether the gesture consumed it, so
    /// the caller can skip both the marquee and the camera pan.
    ///
    /// `snap` is ⇧ — read live rather than latched at the press, so it can be pressed and
    /// released part way through a rotation, which is how every application that has it
    /// behaves. There is no equivalent on a touchscreen; that is a stated gap rather than a
    /// silent one.
    pub fn drag_to(&mut self, at: WorldPoint, snap: bool, projection: &mut Projection) -> bool {
        let Some(drag) = self.drag.as_mut() else { return false };
        drag.at = at;
        drag.snap = snap;
        drag.preview(projection);
        true
    }

    /// The button came up. Answers the placements to write, or `None` when nothing was in
    /// flight.
    ///
    /// ⚠ **A rotation is effective with no offset at all.** The desktop's `is_effective`
    /// records this: the pointer travels but the item's centre does not, so `offset == (0,0)`
    /// cannot be the test for every mode and using it is what would make a rotate commit
    /// nothing. The no-op case is caught where it belongs instead — the writer skips any
    /// member whose placement did not actually change, so a click on a grip leaves no undo
    /// step behind it whatever the mode.
    fn release(&mut self) -> Option<Vec<(DocId, Placement)>> {
        let drag = self.drag.take()?;
        Some(
            drag.items
                .iter()
                .map(|(_, doc, original)| (*doc, drag.placement_of(original, drag.at)))
                .collect(),
        )
    }

    /// The gesture ended without a release — `pointercancel`, a second finger landing,
    /// Escape, editing being switched off, the page being hidden.
    ///
    /// ⚠ **Takes `&mut Projection`, and that is the argument with the sharpest consequence.**
    /// [`Drag::preview`] writes into the projection; a cancel that cannot reach it leaves
    /// every member drawn at a shape the document does not have, with nothing later putting
    /// it back except an unrelated reprojection. `pointercancel` is routine on a touchscreen,
    /// so this is the ordinary path and not the exotic one.
    ///
    /// Feedback 27's rule, which this repository has paid for on five separate gestures now:
    /// **give every way a gesture can end without a release a call to the function that
    /// closes it.** Idempotent, so calling it defensively costs nothing.
    pub fn cancel(&mut self, projection: &mut Projection) {
        let Some(drag) = self.drag.take() else { return };
        drag.restore(projection);
    }

    /// Drops a gesture whose preconditions have gone, and prunes members the projection no
    /// longer holds.
    ///
    /// ⚠ **Called from [`push`], on the way to the painter, and that placement is the whole
    /// point.** Editing can be switched off, a board can be replaced by `crate::live`'s poll,
    /// an undo can remove what is being reshaped — and none of those paths is wired by this
    /// module or can be asked to remember. `EditState::rings` prunes the selection in exactly
    /// this place for exactly this reason: a stale gesture cannot survive a single frame,
    /// whatever created it. Public so a caller that wants it sooner has it.
    pub fn settle(&mut self, edit: &EditState, projection: &mut Projection) {
        if self.drag.is_some() && !edit.enabled() {
            self.cancel(projection);
            return;
        }
        // Written as a value out of the match rather than as an assignment inside it: the
        // borrow of `self.drag` has to have ended before the slot can be cleared, and saying
        // so explicitly beats relying on where the borrow checker decides it ends.
        let emptied = match self.drag.as_mut() {
            None => return,
            Some(drag) => {
                drag.items.retain(|(scene, _, _)| projection.get(*scene).is_some());
                drag.items.is_empty()
            }
        };
        if emptied {
            // Nothing to restore: every member it held has left the projection.
            self.drag = None;
        }
    }

    /// `0` idle, `1` resizing, `2` rotating — for [`velm_handle_report`].
    fn mode_code(&self) -> u8 {
        match self.drag.as_ref().map(|drag| drag.mode) {
            None => 0,
            Some(Mode::Resize(_)) => 1,
            Some(Mode::Rotate) => 2,
        }
    }

    fn carried(&self) -> usize {
        self.drag.as_ref().map_or(0, |drag| drag.items.len())
    }
}

// ---------------------------------------------------------------------------------------
// The document write.
// ---------------------------------------------------------------------------------------

/// Writes a released gesture. Answers whether the document changed.
///
/// # Three things about how it is written
///
/// **`set_placement`, not `translate`.** A resize moves an item's size *and* its centre and a
/// rotation moves its angle, and neither has a delta form the document understands — so the
/// whole placement goes. That is what `crate::style::apply_transform` already does for the
/// panel's typed numbers, and it carries the same cost, stated here rather than discovered:
/// an edit arriving from the server *during* the drag is overwritten for these two fields,
/// where a move composes. The alternative — recomputing against whatever the document holds
/// at the release — would make the commit disagree with the preview the user was watching,
/// which is worse.
///
/// **The originals come from the press, not from the projection.** By the time a gesture
/// releases, the projection holds the *preview*; transforming that again would apply the
/// gesture twice. This is the same trap `crate::edit::commit` names from the other side.
///
/// **Members that did not actually move are skipped**, so a click on a grip leaves no undo
/// step at all — which is the no-op test for a rotation, whose offset is meaningless.
///
/// ⚠ One undo group, through [`crate::edit::grouped`] rather than a group opened here. Its
/// closure returns `T` rather than `Result<T>`, so trap 11's failure — a `?` escaping between
/// begin and end, which breaks every grouped operation on the board for the rest of the
/// session — is unrepresentable rather than rescued. Per-item errors are logged and folded
/// into the count, which is also the honest shape: an item deleted from under a drag by an
/// undo is a member that is simply gone, not a failed gesture.
fn commit(
    board: &mut Board,
    projection: &mut Projection,
    writes: &[(DocId, Placement)],
) -> Result<bool, String> {
    let planned: Vec<(DocId, Placement)> = writes
        .iter()
        .filter(|(doc, next)| {
            board.item(*doc).is_ok_and(|item| item.placement != *next)
        })
        .copied()
        .collect();
    if planned.is_empty() {
        // ⚠ Still reprojected. The preview is in the projection and the document has not
        // changed, so without this every member stays drawn — and *hit-tested* — at a shape
        // nothing wrote. A click landing where an item is not reads as a different bug
        // entirely, which is `Editor::edit`'s hardest-won line.
        crate::edit::resettle(board, projection);
        return Ok(false);
    }
    let written = crate::edit::grouped(&mut *board, |board| {
        let mut written = 0usize;
        for (doc, placement) in &planned {
            match board.set_placement(*doc, *placement) {
                Ok(()) => written += 1,
                Err(error) => log::debug!("velm handles: reshaping {doc}: {error}"),
            }
        }
        written
    });
    crate::edit::resettle(board, projection);
    written.map(|count| count > 0)
}

// ---------------------------------------------------------------------------------------
// The hops `crate::input` calls — one line each, so the file this module does not own stays
// almost unchanged and the decisions stay here where they can be read together.
// ---------------------------------------------------------------------------------------

/// A pointer went down. Answers whether a grip took the gesture.
///
/// ⚠ **Ask this before `crate::edit::pointer_down` and skip that call when it answers
/// `true`.** Grips sit on an item's own outline, so a press on one that reached the edit
/// layer first would select the item and start sliding it, and the grip would never fire.
pub fn pointer_down(viewer: &mut crate::Viewer, at: WorldPoint, aim: Aim) -> bool {
    let zoom = viewer.camera.zoom();
    let crate::Viewer { handles, edit, projection, .. } = viewer;
    // ⚠ **A gesture still live in the edit layer means a release was lost** — a swallowed
    // `pointerup`, a system edge gesture — which is routine on a touchscreen.
    // `crate::edit::pointer_down` does put it back, but that runs *after* this call and
    // [`offer`] refuses while one is in flight, so without this line a lost release would
    // cost a whole press before the grips came back. Idempotent, and free otherwise; the
    // edit layer's own cancel a moment later is equally free.
    edit.cancel(projection);
    handles.press(at, aim, edit, projection, zoom)
}

/// The pointer moved with the button down. Answers whether a grip consumed the motion.
///
/// ⚠ Ask this **before** `crate::edit::pointer_move`, for the same reason and in the same
/// order as the press.
pub fn pointer_move(viewer: &mut crate::Viewer, at: WorldPoint, snap: bool) -> bool {
    let crate::Viewer { handles, projection, .. } = viewer;
    handles.drag_to(at, snap, projection)
}

/// The pointer came up. Commits the transform, tells the push half, and answers whether a
/// **grip owned the release**.
///
/// ⚠ **Not whether the document changed, and the difference is a real defect rather than a
/// nicety.** `crate::edit::pointer_up` answers "changed" on purpose, because a press that
/// selected nothing and moved nothing *is* a tap and should be allowed to fall through to the
/// tap path that opens a link card's ↗. A press that landed on a grip is never a tap: it was
/// aimed at the grip. Answering "changed" here would let a zero-travel grip click fall
/// through — it passes `TAP_SLOP` comfortably — and `badges::pressed` is then asked at a
/// point that, for a link card, is inside the same top-right corner the `TopRight` grip sits
/// on. Clicking a resize grip would open the page.
///
/// So `false` means one thing only: **no grip was held**, and the caller should carry on to
/// `crate::edit::pointer_up` exactly as it does today. A failed commit still answers `true`:
/// the gesture existed and was this module's to end.
pub fn pointer_up(viewer: &mut crate::Viewer) -> bool {
    let crate::Viewer { handles, board, projection, push, .. } = viewer;
    let Some(writes) = handles.release() else { return false };
    match commit(board, projection, &writes) {
        Ok(changed) => {
            // ⚠ After the write **and** after the reprojection, never before: `tick` exports
            // the board as it stands, so a `note_edit` that ran first and a `tick` that ran
            // before the write would send an empty delta and mark the real edit acknowledged.
            if changed {
                crate::edit::announce(push, board);
            }
        }
        // Logged and swallowed rather than propagated: `commit` has already reprojected on
        // every path out, so the board on screen agrees with the board in memory whatever
        // happened, and there is nothing a pointer handler could usefully do with the string.
        Err(error) => log::error!("velm handles: {error}"),
    }
    true
}

/// The gesture ended without a release. Idempotent, and free when nothing is in flight.
pub fn pointer_cancel(viewer: &mut crate::Viewer) {
    let crate::Viewer { handles, projection, .. } = viewer;
    handles.cancel(projection);
}

// ---------------------------------------------------------------------------------------
// What the painter draws.
// ---------------------------------------------------------------------------------------

/// Draws the grips, and the shared box behind a multi-selection's.
///
/// ⚠ **In the board view, with every screen constant divided by the zoom** — never multiplied
/// by it. A grip sits *on* the item as drawn, including when the item is turned, where a
/// screen-space square would be visibly off the corner it names; and it must hold its size on
/// screen while the item under it shrinks, which is the same trade the selection ring
/// directly above it makes. The caller has already selected the board view for the ring; this
/// re-selects it so the function is correct on its own.
///
/// `&mut Handles` and `&mut Projection` because this is where a stale gesture is settled —
/// see [`Handles::settle`] for why that belongs on the way to the painter rather than at four
/// call sites that would each have to remember.
pub fn push(
    list: &mut DrawList,
    board_view: u32,
    camera: &Camera,
    theme: &Theme,
    edit: &EditState,
    handles: &mut Handles,
    projection: &mut Projection,
) {
    handles.settle(edit, projection);
    let offered = handles.offer_now(edit, projection);
    if offered.is_none() {
        return;
    }
    let zoom = camera.zoom();
    if !zoom.is_finite() || zoom <= 0.0 {
        return;
    }
    let side = f64::from(handle::HANDLE_SIZE) / zoom;
    let border = (HANDLE_BORDER / zoom) as f32;
    list.use_view(board_view);

    // The group's own outline first, so it sits behind its grips. Each member already draws
    // its own ring; this is the box the grips act on, which is not the same thing and is
    // otherwise invisible.
    if let Offer::Group(group) = offered {
        push_group_outline(list, camera, theme, &group, zoom);
    }
    // A single item's grips turn with it; a group's box has no rotation of its own, which is
    // exactly why it offers no edge grips — see the module header.
    let spin = match offered {
        Offer::One { rotation, .. } => rotation,
        _ => 0.0,
    };
    for (grip, at) in offered.positions(zoom) {
        let origin = WorldPoint::new(at.x - side / 2.0, at.y - side / 2.0);
        list.push_quad(
            QuadInstance::solid(
                camera.to_camera_relative(origin),
                [side as f32, side as f32],
                theme.surface,
            )
            .with_border(theme.accent, border)
            // Square grips, per `docs/05-design-language.md` §4 — "square handles, no glow" —
            // except rotate, which is round so that it reads as a different verb before it is
            // touched rather than after.
            .with_corner_radius(if grip.is_rotate() { (side / 2.0) as f32 } else { 0.0 })
            .with_rotation(spin),
        );
    }
}

/// The multi-selection's shared box, as four hairlines.
///
/// Four edges rather than one bordered quad: a filled quad — even a transparent one — would
/// sit over the items inside it, and the design language asks for one line doing the work
/// rather than a panel. Muted against the members' own rings, so the box reads as the frame
/// around them rather than as one more selected thing.
fn push_group_outline(
    list: &mut DrawList,
    camera: &Camera,
    theme: &Theme,
    group: &Group,
    zoom: f64,
) {
    let width = (HANDLE_BORDER / zoom) as f32;
    let (left, top) = (group.x - group.width / 2.0, group.y - group.height / 2.0);
    let edges = [
        (left, top, group.width, 0.0),
        (left, top + group.height, group.width, 0.0),
        (left, top, 0.0, group.height),
        (left + group.width, top, 0.0, group.height),
    ];
    let wash: Rgba = theme.accent.with_alpha(0.45);
    for (x, y, w, h) in edges {
        list.push_quad(QuadInstance::solid(
            camera.to_camera_relative(WorldPoint::new(x, y)),
            [(w as f32).max(width), (h as f32).max(width)],
            wash,
        ));
    }
}

// ---------------------------------------------------------------------------------------
// The exports a fixture reads. Prefixed `velm_`, as `crate::edit`'s are.
// ---------------------------------------------------------------------------------------

/// The viewer, if the page has one and nothing further up the stack is holding it.
///
/// Every borrow is a **`try_`** borrow. `panic = "abort"` is set for every release profile
/// including `web`, so a `RefCell` collision here is not a caught panic and a console line —
/// it is a dead tab with nothing on screen and nothing in the log. `None` means "not now",
/// which for a report is exactly the right answer.
///
/// A twin of `crate::edit`'s own private helper. Worth folding into one `pub(crate)` function
/// the day a third module needs it; two four-line copies of a `try_borrow` are not worth a
/// visibility change to a file this module does not own.
fn viewer() -> Option<Rc<RefCell<crate::Viewer>>> {
    crate::VIEWER.with(|slot| slot.try_borrow().ok().and_then(|held| held.clone()))
}

/// The grip layer, as a string, for a fixture to read.
///
/// The only honest check available in a crate where **no test is ever compiled**, and it
/// exists for the reason `velm_edit_report` does: `crate::input` is driven by DOM events, so
/// the only way to verify a gesture is to dispatch real events at the real listeners and then
/// ask what happened. Trap 9's lesson — a fixture that calls the handler directly starts
/// downstream of everything that can go wrong between the browser and the handler, and stays
/// green on a build where the listener was never attached.
///
/// `offered mode carried group`, space separated so a fixture that splits on whitespace
/// cannot be broken by a change of punctuation:
///
/// - `offered` — how many grips are on screen. **9** for a lone item, **5** for a
///   multi-selection, **0** for neither. A fixture asserting `5` is asserting the
///   no-edge-handles rule, which is the restriction most likely to be lost by a later edit.
/// - `mode` — `0` idle, `1` resizing, `2` rotating.
/// - `carried` — how many members a live gesture will write.
/// - `group` — `1` when the grips belong to a shared box.
#[wasm_bindgen]
pub fn velm_handle_report() -> String {
    let idle = "0 0 0 0".to_owned();
    let Some(held) = viewer() else { return idle };
    let Ok(viewer) = held.try_borrow() else { return idle };
    let offered = viewer.handles.offer_now(&viewer.edit, &viewer.projection);
    format!(
        "{} {} {} {}",
        offered.positions(viewer.camera.zoom()).len(),
        viewer.handles.mode_code(),
        viewer.handles.carried(),
        u8::from(offered.is_group()),
    )
}

/// Where every offered grip is, in **physical** screen pixels: `Name:x:y Name:x:y …`, or the
/// empty string when nothing is offered.
///
/// ⚠ **This exists so a fixture can aim at a grip by asking where it is.** The alternative is
/// a fixture that re-derives the layout in JavaScript from the selection's box — a second
/// copy of the geometry this whole module is arranged around having exactly one of, and one
/// that would keep passing while the real layout moved underneath it.
///
/// Physical rather than CSS pixels, because that is what the camera speaks and converting
/// here would be a second copy of `crate::input`'s device-ratio conversion — trap 4, the one
/// this client already states must live in one place. A fixture divides by
/// `devicePixelRatio`, which JavaScript has natively, before putting the numbers into a
/// `PointerEvent`'s `clientX`/`clientY`.
///
/// The name is the `Handle` variant's own `Debug`, so a grip renamed in the shared crate
/// renames here too rather than drifting from a table nobody updates.
#[wasm_bindgen]
pub fn velm_handle_points() -> String {
    let Some(held) = viewer() else { return String::new() };
    let Ok(viewer) = held.try_borrow() else { return String::new() };
    let zoom = viewer.camera.zoom();
    let offered = viewer.handles.offer_now(&viewer.edit, &viewer.projection);
    offered
        .positions(zoom)
        .into_iter()
        .map(|(grip, at)| {
            let screen = viewer.camera.world_to_screen(at);
            format!("{grip:?}:{:.1}:{:.1}", screen.x, screen.y)
        })
        .collect::<Vec<String>>()
        .join(" ")
}
