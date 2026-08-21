//! Resize and rotate handles: where they sit, what a press lands on, and what a drag
//! of one does to a placement.
//!
//! Pure geometry. Nothing here reads the document, the camera or the selection — it
//! takes a [`Placement`] and gives back positions or a new [`Placement`], which is what
//! lets the whole of it be tested without a window.
//!
//! # Everything is resolved against the rotated box
//!
//! A handle sits on the item as drawn, not on the axis-aligned box the spatial index
//! holds — the same rule `crate::connector` states for anchors, and for the same
//! reason: the corner of a widget turned 30° is not the corner of its bounding box, and
//! a handle drawn there would be visibly off the shape it belongs to.
//!
//! [`vellum_connect::WidgetBounds`] already does that arithmetic correctly and is
//! tested against a 90° placement, so this borrows it rather than writing a second
//! rotation that could disagree with the first.

use vellum_connect::{Anchor, WidgetBounds};
use vellum_doc::Placement;
use vellum_scene::WorldPoint;

/// The smallest an item can be dragged to, in world units.
///
/// Not zero, and not negative. `crate::inspect` applies the same floor for the same
/// reason: an item with no area cannot be hit-tested, so it becomes unselectable and
/// therefore unrecoverable — the user would have dragged it out of existence with no
/// way to get it back but undo.
pub const MIN_SIZE: f64 = 8.0;

/// How far above the item's top edge the rotate handle floats, in **screen** pixels.
pub const ROTATE_OFFSET: f32 = 24.0;

/// A handle's side length in screen pixels. Constant on screen at every zoom, like the
/// selection ring — a handle that grew with the board would swallow the item it
/// belongs to.
pub const HANDLE_SIZE: f32 = 8.0;

/// How close a press has to be to count as landing on a handle, in screen pixels.
///
/// Larger than the handle itself. An 8px square is a hard target with a mouse, and the
/// cost of being generous is only that a press very near a corner resizes instead of
/// moving — which is what someone aiming at a corner meant.
pub const GRAB_SLOP: f32 = 5.0;

/// One of the nine things a selection offers to drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    /// Floats above the top edge — Miro's placement, and the one that cannot be
    /// confused with a resize because it is not on the outline at all.
    Rotate,
}

impl Handle {
    /// Corners first, then edges, then rotate. **Hit-testing walks this order**, so a
    /// corner wins over the two edges it touches — which is what someone aiming at a
    /// corner meant, and the ambiguity is otherwise decided by floating-point luck.
    pub const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::TopRight,
        Self::BottomRight,
        Self::BottomLeft,
        Self::Top,
        Self::Right,
        Self::Bottom,
        Self::Left,
        Self::Rotate,
    ];

    /// Where the handle sits on the item's box, as a normalised anchor.
    const fn anchor(self) -> Anchor {
        match self {
            Self::TopLeft => Anchor { x: 0.0, y: 0.0 },
            Self::Top | Self::Rotate => Anchor { x: 0.5, y: 0.0 },
            Self::TopRight => Anchor { x: 1.0, y: 0.0 },
            Self::Right => Anchor { x: 1.0, y: 0.5 },
            Self::BottomRight => Anchor { x: 1.0, y: 1.0 },
            Self::Bottom => Anchor { x: 0.5, y: 1.0 },
            Self::BottomLeft => Anchor { x: 0.0, y: 1.0 },
            Self::Left => Anchor { x: 0.0, y: 0.5 },
        }
    }

    /// Which edges this handle moves, as `(horizontal, vertical)` in local space where
    /// `-1` is the left/top edge, `+1` the right/bottom, and `0` "leave this axis".
    ///
    /// An edge handle zeroes one axis, which is what makes dragging the right edge
    /// change the width and nothing else.
    const fn axes(self) -> (f64, f64) {
        match self {
            Self::TopLeft => (-1.0, -1.0),
            Self::Top => (0.0, -1.0),
            Self::TopRight => (1.0, -1.0),
            Self::Right => (1.0, 0.0),
            Self::BottomRight => (1.0, 1.0),
            Self::Bottom => (0.0, 1.0),
            Self::BottomLeft => (-1.0, 1.0),
            Self::Left => (-1.0, 0.0),
            Self::Rotate => (0.0, 0.0),
        }
    }

    /// Whether this grip drives **both** axes at once.
    ///
    /// The four corners do; the four edges each drive one; rotate drives neither.
    pub const fn is_corner(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight | Self::BottomLeft | Self::BottomRight)
    }

    pub const fn is_rotate(self) -> bool {
        matches!(self, Self::Rotate)
    }

    /// Which of the box's four edges this handle actually moves, as
    /// `[(left, right), (top, bottom)]`.
    ///
    /// Only [`crate::snap`] needs this, and it needs it precisely: a resize must correct the
    /// edge under the pointer and never the one opposite, or snapping slides the whole box
    /// out from under the hand that is holding it still. `Rotate` moves no edge, which is
    /// why it answers all-false rather than being unreachable — a rotation that snapped to
    /// an edge is a bug, not an omission.
    pub const fn moving_edges(self) -> [(bool, bool); 2] {
        match self {
            Self::TopLeft => [(true, false), (true, false)],
            Self::Top => [(false, false), (true, false)],
            Self::TopRight => [(false, true), (true, false)],
            Self::Right => [(false, true), (false, false)],
            Self::BottomRight => [(false, true), (false, true)],
            Self::Bottom => [(false, false), (false, true)],
            Self::BottomLeft => [(true, false), (false, true)],
            Self::Left => [(true, false), (false, false)],
            Self::Rotate => [(false, false), (false, false)],
        }
    }
}

/// The shared box a **multi-selection** is transformed against: the axis-aligned envelope
/// of every member's rotated bounds.
///
/// Its own type rather than a [`Placement`] because it has no rotation and no scale. Giving
/// it those would invite the group to be rotated as a rigid body and then resized along its
/// own axes, and that is where the shear below comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Group {
    /// Centre.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Group {
    /// The corner diagonally opposite `handle`, which a resize holds still.
    fn fixed_corner(self, handle: Handle) -> (f64, f64) {
        let (ax, ay) = handle.axes();
        (
            self.x - ax * self.width / 2.0,
            self.y - ay * self.height / 2.0,
        )
    }
}

/// The handles a multi-selection offers: the four corners, and rotate.
///
/// **No edge handles, and that is a real restriction rather than an omission.** An edge
/// drag scales one axis. A [`Placement`] is an axis-aligned box *plus* a rotation, so
/// scaling one axis of a group whose members are rotated asks for a sheared rectangle, and
/// there is no `Placement` that is one. Corner handles scale both axes by the same factor,
/// which is a similarity transform and stays representable at any member rotation.
pub const GROUP_HANDLES: [Handle; 5] = [
    Handle::TopLeft,
    Handle::TopRight,
    Handle::BottomRight,
    Handle::BottomLeft,
    Handle::Rotate,
];

/// The envelope of a set of placements, or `None` for an empty set.
pub fn group_bounds(items: &[Placement]) -> Option<Group> {
    let mut extremes: Option<(f64, f64, f64, f64)> = None;
    for placement in items {
        // Corners, not the unrotated box: a widget turned 45° reaches further than its own
        // width, and a group box that cut through its members would be visibly wrong.
        for corner in bounds(placement).corners() {
            extremes = Some(match extremes {
                None => (corner.x, corner.y, corner.x, corner.y),
                Some((left, top, right, bottom)) => (
                    left.min(corner.x),
                    top.min(corner.y),
                    right.max(corner.x),
                    bottom.max(corner.y),
                ),
            });
        }
    }
    let (left, top, right, bottom) = extremes?;
    Some(Group {
        x: (left + right) / 2.0,
        y: (top + bottom) / 2.0,
        width: (right - left).max(MIN_SIZE),
        height: (bottom - top).max(MIN_SIZE),
    })
}

/// Where a multi-selection's handles sit in world space.
///
/// Axis-aligned, because [`Group`] has no rotation: the box is recomputed from the members
/// after every commit, so it re-derives its own orientation rather than carrying one.
pub fn group_positions(group: &Group, zoom: f64) -> Vec<(Handle, WorldPoint)> {
    let stand_off = f64::from(ROTATE_OFFSET) / zoom.max(f64::EPSILON);
    GROUP_HANDLES
        .into_iter()
        .map(|handle| {
            let anchor = handle.anchor();
            let at = WorldPoint::new(
                group.x + (anchor.x - 0.5) * group.width,
                group.y + (anchor.y - 0.5) * group.height,
            );
            let at = if handle.is_rotate() {
                WorldPoint::new(at.x, at.y - stand_off)
            } else {
                at
            };
            (handle, at)
        })
        .collect()
}

/// The multi-selection handle under a world point, by the same nearest-within-radius rule
/// [`hit`] uses — and for the same reason: at a fitted zoom the grab radius is hundreds of
/// world units, so "the first one that qualifies" picks by array order rather than by aim.
pub fn group_hit(group: &Group, at: WorldPoint, zoom: f64) -> Option<Handle> {
    let radius = f64::from(HANDLE_SIZE / 2.0 + GRAB_SLOP) / zoom.max(f64::EPSILON);
    group_positions(group, zoom)
        .into_iter()
        .map(|(handle, position)| {
            (handle, (position.x - at.x).hypot(position.y - at.y))
        })
        .filter(|(_, distance)| *distance <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(handle, _)| handle)
}

/// The uniform factor a corner drag scales the group by, and the corner it holds still.
///
/// One factor for both axes — see [`GROUP_HANDLES`] for why a group cannot be stretched.
/// Driven by whichever axis the pointer moved *further* in proportion, so dragging mostly
/// sideways scales by the width and mostly downwards by the height, which is what the hand
/// expects. Floored so the box can neither invert nor collapse below [`MIN_SIZE`].
pub fn group_scale(group: &Group, handle: Handle, delta: (f64, f64)) -> (f64, (f64, f64)) {
    let (ax, ay) = handle.axes();
    let width = (group.width + ax * delta.0).max(MIN_SIZE);
    let height = (group.height + ay * delta.1).max(MIN_SIZE);
    let (fx, fy) = (width / group.width, height / group.height);
    // The axis that changed more, by distance from 1.
    let factor = if (fx - 1.0).abs() >= (fy - 1.0).abs() { fx } else { fy };
    (factor.max(f64::EPSILON), group.fixed_corner(handle))
}

/// One member's placement after the group is scaled by `factor` about `fixed`.
///
/// Both the size *and* the centre scale: holding one corner of the group still means every
/// member's distance from that corner grows with the box, which is the difference between
/// resizing a group and resizing each of its members in place.
///
/// `width`/`height` are written and `scale` is left alone, matching [`resize`] and the
/// properties panel — the two are redundant and moving both makes them disagree.
pub fn scale_member(original: &Placement, factor: f64, fixed: (f64, f64)) -> Placement {
    let scale = original.scale.max(f64::EPSILON);
    let (drawn_w, drawn_h) = original.scaled_size();
    Placement {
        x: fixed.0 + (original.x - fixed.0) * factor,
        y: fixed.1 + (original.y - fixed.1) * factor,
        width: (drawn_w * factor).max(MIN_SIZE) / scale,
        height: (drawn_h * factor).max(MIN_SIZE) / scale,
        ..*original
    }
}

/// One member's placement after the group is turned by `degrees` about `centre`.
///
/// **Two things move, not one.** The member's own `rotation` gains the delta, and its
/// centre travels around the group's — which is the whole reason a multi-selection rotate
/// is a different operation from a single one rather than a bigger version of it.
pub fn rotate_member(original: &Placement, centre: (f64, f64), degrees: f64) -> Placement {
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (dx, dy) = (original.x - centre.0, original.y - centre.1);
    Placement {
        x: centre.0 + dx * cos - dy * sin,
        y: centre.1 + dx * sin + dy * cos,
        rotation: (original.rotation + degrees).rem_euclid(360.0),
        ..*original
    }
}

/// The angle from a box's centre to a world point, in the same clockwise degrees
/// [`Placement::rotation`] uses and with the rotate handle's own offset already accounted
/// for — so a drag that starts on the handle begins at zero.
pub fn angle_to(centre: (f64, f64), at: WorldPoint) -> f64 {
    let (dx, dy) = (at.x - centre.0, at.y - centre.1);
    if dx.hypot(dy) < f64::EPSILON {
        return 0.0;
    }
    // Same conversion as `rotate`: `atan2` measures anticlockwise from +x, the handle
    // starts at the top (−y), and rotation runs clockwise.
    (dy.atan2(dx).to_degrees() + 90.0).rem_euclid(360.0)
}

/// The rotated box a placement occupies, at its drawn size.
fn bounds(placement: &Placement) -> WidgetBounds {
    crate::connector::widget_bounds(placement)
}

/// Rotates a local-space vector by the placement's rotation, into world space.
fn to_world(placement: &Placement, local: (f64, f64)) -> (f64, f64) {
    let radians = placement.rotation.to_radians();
    let (sin, cos) = radians.sin_cos();
    (local.0 * cos - local.1 * sin, local.0 * sin + local.1 * cos)
}

/// Rotates a world-space vector into the placement's local space — the inverse of
/// [`to_world`], which is what turns a screen drag into a change along the item's own
/// axes rather than the screen's.
fn to_local(placement: &Placement, world: (f64, f64)) -> (f64, f64) {
    let radians = placement.rotation.to_radians();
    let (sin, cos) = radians.sin_cos();
    (world.0 * cos + world.1 * sin, -world.0 * sin + world.1 * cos)
}

/// Where every handle sits in world space.
///
/// `zoom` is needed only for the rotate handle: its stand-off is a constant number of
/// *screen* pixels, so in world units it has to shrink as the board is zoomed in, or it
/// would drift metres away from the item at a fitted zoom.
pub fn positions(placement: &Placement, zoom: f64) -> Vec<(Handle, WorldPoint)> {
    let bounds = bounds(placement);
    Handle::ALL
        .into_iter()
        .map(|handle| {
            let point = bounds.resolve(handle.anchor());
            let point = if handle.is_rotate() {
                // Straight out from the top edge, in the item's own "up", so the handle
                // stays above the item's head however far it is turned.
                let out = to_world(placement, (0.0, -f64::from(ROTATE_OFFSET) / zoom));
                WorldPoint::new(point.x + out.0, point.y + out.1)
            } else {
                WorldPoint::new(point.x, point.y)
            };
            (handle, point)
        })
        .collect()
}

/// The handle under a world point, if any.
///
/// `zoom` converts the screen-space grab radius into world units, so a handle is the
/// same size to aim at whatever the board is scaled to.
pub fn hit(placement: &Placement, at: WorldPoint, zoom: f64) -> Option<Handle> {
    let radius = f64::from(HANDLE_SIZE / 2.0 + GRAB_SLOP) / zoom;
    // The **nearest** qualifying handle, not the first. Zoomed out the radius is large
    // in world units — 325 of them at a 4% fit — so several handles can be inside it at
    // once, and taking the first would answer with whichever happens to come earliest
    // in `ALL` rather than the one under the pointer.
    //
    // `min_by` keeps the first of equal distances, so `Handle::ALL`'s corners-first
    // order still decides an exact tie between a corner and the edges meeting it.
    positions(placement, zoom)
        .into_iter()
        .map(|(handle, point)| (handle, (point.x - at.x).hypot(point.y - at.y)))
        .filter(|(_, distance)| *distance <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(handle, _)| handle)
}

// ----- connector ports ------------------------------------------------------------
//
// Miro's four blue dots. *"in miro there are these 4 blue dots around the picture and there
// should be 4 blue dots on images agents sticky notes and when i hold and draw i should be
// able to connect it to other agents sticky notes or agents or pictures."*
//
// This closes the known defect that read *"there are no anchor dots on the canvas"* — the
// panel could already insist on a side, and the only way to *draw* a connector was to arm
// the connector tool and drag between two items, which is a tool nobody finds.

/// A port's radius in **screen** pixels. Round, and smaller than a resize handle.
///
/// The shape is the whole distinction. A square handle changes an item's size and a round
/// one starts a line — `docs/05-design-language.md` §4's rule ("square handles, no glow")
/// already spends roundness on the rotate grip for exactly this reason, so a second round
/// grip is consistent rather than novel. They are also never on screen together at the same
/// place: a port sits *outside* the edge and a resize handle sits *on* it.
pub const PORT_RADIUS: f32 = 4.0;

/// How far outside the item's edge a port floats, in **screen** pixels, measured from the
/// edge to the port's centre.
///
/// Outside rather than on the edge, which is what Miro draws and what makes the gesture
/// unambiguous: a press on the outline is a resize, a press just beyond it is a connector.
/// Overlapping the two would make the commonest gesture on the board — resizing — depend on
/// a pixel.
///
/// # 9 → 20 → 40, at the user's word, twice
///
/// *"please put alot more spacing between the dragging connector dots that are around the
/// object and actual object itself its too close"*, then, having seen 20: *"2 times more
/// distance pelase"*. Nine screen pixels is roughly Miro's own stand-off and it was chosen
/// for parity; on a real board it puts the dot inside the selection ring's visual weight, so
/// the four ports read as part of the outline rather than as four separate things to grab.
///
/// The distance is also strictly better for the gesture, which is why it was not merely
/// taste even at 20: the corner resize handles reach `HANDLE_SIZE / 2 + GRAB_SLOP` = 9
/// pixels out from the corner, so at the original stand-off an edge port's grab circle
/// *overlapped* the corner handle's, and which of the two a press meant was decided by which
/// was asked first. `a_port_clears_the_corner_handles_reach` pins the clearance.
///
/// **What sets the ceiling**, for whoever is asked for more: a port has to stay obviously
/// attached to its item, and on a *small* item the four dots are further from the box than
/// the box is wide. At 40 that is anything under 80 world units at 100% zoom, which is
/// smaller than every default size in the application — so nothing you can place by dragging
/// reaches it. Much past this and the dots start reading as four loose objects on the board.
pub const PORT_OFFSET: f32 = 40.0;

/// How close a press has to be to count as landing on a port, in **screen** pixels.
///
/// Bigger than [`GRAB_SLOP`], because a port is smaller than a handle and is aimed at from
/// outside the item where there is nothing else to hit. The cost of being generous is only
/// that a press very near the edge draws a line instead of starting a marquee.
pub const PORT_SLOP: f32 = 6.0;

/// Where the four ports sit in world space, paired with the anchor each one binds to.
///
/// The anchor is [`crate::connector::ANCHORS`]' own tuple, so what the painter draws and
/// what a press writes into `ConnectorEnd::bound` are one value — the `draw::kanban_runs`
/// rule, and here it decides whether a line leaves the edge you aimed at.
///
/// Resolved against the **rotated** box and pushed out along that edge's own outward normal,
/// so the four dots stay on the four edges of a turned item rather than at the compass
/// points of its bounding box. `WidgetBounds::outward_normal` is the same function a
/// connector's own endpoint normal comes from, so a line leaves its port along the direction
/// the port is drawn in.
pub fn ports(placement: &Placement, zoom: f64) -> Vec<((f64, f64), WorldPoint)> {
    let bounds = bounds(placement);
    let out = f64::from(PORT_OFFSET) / zoom;
    crate::connector::ANCHORS
        .into_iter()
        .map(|anchor| {
            let a = Anchor::new(anchor.0, anchor.1);
            let at = bounds.resolve(a);
            // `None` only for an anchor that is not on the outline, which none of the four
            // is — so the fallback is unreachable rather than a case. Spelled as a default
            // rather than an `expect` to keep this a total geometry helper, exactly as
            // `connector::facing_anchor` does.
            let normal = bounds.outward_normal(a);
            let (dx, dy) = normal.map_or((0.0, 0.0), |n| (n.x * out, n.y * out));
            (anchor, WorldPoint::new(at.x + dx, at.y + dy))
        })
        .collect()
}

/// The port under a world point, if any, as the anchor it would bind to.
///
/// The **nearest** qualifying port rather than the first, for [`hit`]'s reason: zoomed out
/// the grab radius is large in world units and two ports can both be inside it, so taking
/// the first would answer with whichever comes earliest in `ANCHORS` instead of the one
/// under the pointer.
pub fn port_hit(placement: &Placement, at: WorldPoint, zoom: f64) -> Option<(f64, f64)> {
    let radius = f64::from(PORT_RADIUS + PORT_SLOP) / zoom;
    ports(placement, zoom)
        .into_iter()
        .map(|(anchor, point)| (anchor, (point.x - at.x).hypot(point.y - at.y)))
        .filter(|(_, distance)| *distance <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(anchor, _)| anchor)
}

/// The placement a resize drag produces.
///
/// `original` is where the item was when the button went down and `delta` is the whole
/// world-space travel since — never an increment. A thousand samples of
/// `original + delta` cannot accumulate error; a thousand increments can, and the item
/// would creep away from the pointer over a long drag.
///
/// Three fields move, not two. `x`/`y` is the item's **centre**, so holding the
/// opposite edge still while this one moves shifts the centre by half the change.
pub fn resize(
    original: &Placement,
    handle: Handle,
    delta: (f64, f64),
    lock_aspect: bool,
) -> Placement {
    let (ax, ay) = handle.axes();
    // Into the item's own axes, so dragging the right edge of a rotated item widens it
    // along its own width rather than along the screen's x.
    let local = to_local(original, delta);
    let (scale_x, scale_y) = (original.scale.max(f64::EPSILON), original.scale);

    let (drawn_w, drawn_h) = original.scaled_size();
    let (mut width, mut height) =
        ((drawn_w + ax * local.0).max(MIN_SIZE), (drawn_h + ay * local.1).max(MIN_SIZE));

    // **A corner keeps the proportions; an edge does not.**
    //
    // *"if i change the size holding from the corners it should not change the aspect
    // ratio … it should only distort the image if i am changing it by holding from the
    // sides or the top"* — which is what every image editor does, and what the two kinds
    // of grip are *for*: a corner moves both axes and so has a proportion to keep, an
    // edge moves one and cannot have one.
    //
    // One factor for both axes, taken from whichever axis the pointer moved further.
    // Using the larger keeps the image under the pointer on the diagonal the drag is
    // actually travelling; using the smaller makes a mostly-horizontal drag barely move
    // and feels stuck.
    if lock_aspect && handle.is_corner() && drawn_w > 0.0 && drawn_h > 0.0 {
        let (fx, fy) = (width / drawn_w, height / drawn_h);
        let factor = if (fx - 1.0).abs() >= (fy - 1.0).abs() { fx } else { fy };
        // The floor has to be applied to the *factor*, not to each axis afterwards, or
        // clamping the short side would silently break the ratio this exists to keep.
        let floor = (MIN_SIZE / drawn_w).max(MIN_SIZE / drawn_h);
        let factor = factor.max(floor);
        width = drawn_w * factor;
        height = drawn_h * factor;
    }

    // How much each axis actually grew, after the floor — clamping the size without
    // clamping the centre shift is what makes an item slide sideways once it has been
    // squashed as far as it will go.
    let grew = (width - drawn_w, height - drawn_h);
    let shift_local = (ax * grew.0 / 2.0, ay * grew.1 / 2.0);
    let shift = to_world(original, shift_local);

    Placement {
        x: original.x + shift.0,
        y: original.y + shift.1,
        // Written as width/height with `scale` left alone, matching what the properties
        // panel does: the two are redundant, and moving both makes them disagree.
        width: width / scale_x,
        height: height / scale_y.max(f64::EPSILON),
        ..*original
    }
}

/// The placement a rotate drag produces.
///
/// `at` is where the pointer is now, in world space. The angle is taken from the item's
/// centre, and the handle's own offset from the top edge falls out of the arithmetic —
/// the pointer does not have to stay on the handle for the rotation to track it, which
/// is what makes a rotate gesture feel like turning something rather than dragging a dot.
pub fn rotate(original: &Placement, at: WorldPoint, snap: bool) -> Placement {
    let (dx, dy) = (at.x - original.x, at.y - original.y);
    if dx.hypot(dy) < f64::EPSILON {
        return *original;
    }
    // `atan2(y, x)` measures from +x anticlockwise; the handle starts at the item's
    // top, which is −y, and `Placement::rotation` runs clockwise. The quarter turn and
    // the sign convert between the two.
    let degrees = dy.atan2(dx).to_degrees() + 90.0;

    let degrees = if snap {
        // Shift snaps to 15°, which is Miro's step and covers every angle anyone
        // reaches for deliberately.
        (degrees / SNAP_DEGREES).round() * SNAP_DEGREES
    } else {
        degrees
    };

    Placement { rotation: degrees.rem_euclid(360.0), ..*original }
}

/// The rotation step Shift snaps to.
pub const SNAP_DEGREES: f64 = 15.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Placement {
        Placement::new(0.0, 0.0, 100.0, 100.0)
    }

    /// The base case, and the one every other assertion is measured against: dragging
    /// the right edge 20 units right makes the item 20 wider and moves its centre 10,
    /// because the centre sits between two edges and only one of them moved.
    #[test]
    fn dragging_an_edge_moves_that_edge_and_half_the_centre() {
        let out = resize(&square(), Handle::Right, (20.0, 0.0), false);
        assert!((out.width - 120.0).abs() < 1e-9, "{}", out.width);
        assert!((out.height - 100.0).abs() < 1e-9, "the other axis moved");
        assert!((out.x - 10.0).abs() < 1e-9, "{}", out.x);
        assert!((out.y - 0.0).abs() < 1e-9);
    }

    /// The opposite edge is the one that has to stay still — it is the thing the user
    /// is holding the shape by.
    #[test]
    fn the_opposite_edge_does_not_move() {
        for (handle, delta) in [
            (Handle::Right, (20.0, 0.0)),
            (Handle::Left, (-20.0, 0.0)),
            (Handle::Top, (0.0, -35.0)),
            (Handle::Bottom, (0.0, 35.0)),
        ] {
            let before = square();
            let after = resize(&before, handle, delta, false);
            let (bw, bh) = before.scaled_size();
            let (aw, ah) = after.scaled_size();
            let fixed_before = (before.x - bw / 2.0, before.y - bh / 2.0, before.x + bw / 2.0, before.y + bh / 2.0);
            let fixed_after = (after.x - aw / 2.0, after.y - ah / 2.0, after.x + aw / 2.0, after.y + ah / 2.0);
            match handle {
                Handle::Right => assert!((fixed_before.0 - fixed_after.0).abs() < 1e-9, "left edge moved"),
                Handle::Left => assert!((fixed_before.2 - fixed_after.2).abs() < 1e-9, "right edge moved"),
                Handle::Top => assert!((fixed_before.3 - fixed_after.3).abs() < 1e-9, "bottom edge moved"),
                Handle::Bottom => assert!((fixed_before.1 - fixed_after.1).abs() < 1e-9, "top edge moved"),
                _ => unreachable!(),
            }
        }
    }

    /// A corner moves both axes; an edge moves one. Getting this wrong makes every
    /// edge handle behave like a corner, which is the whole difference between them.
    #[test]
    fn an_edge_handle_leaves_the_other_axis_alone() {
        let out = resize(&square(), Handle::Top, (40.0, -10.0), false);
        assert!((out.width - 100.0).abs() < 1e-9, "a horizontal drag changed a vertical handle");
        assert!((out.height - 110.0).abs() < 1e-9, "{}", out.height);

        let corner = resize(&square(), Handle::TopRight, (40.0, -10.0), false);
        assert!((corner.width - 140.0).abs() < 1e-9);
        assert!((corner.height - 110.0).abs() < 1e-9);
    }

    /// The floor. An item dragged past nothing must not invert, and must not keep
    /// sliding once it has stopped shrinking — clamping the size but not the centre
    /// shift would walk it sideways for the rest of the gesture.
    #[test]
    fn an_item_cannot_be_dragged_through_itself() {
        let out = resize(&square(), Handle::Right, (-500.0, 0.0), false);
        assert!(out.width >= MIN_SIZE, "{}", out.width);
        assert!(out.height > 0.0);

        // Its left edge — the one being held — is still where it was.
        let left_before = -50.0;
        let left_after = out.x - out.scaled_size().0 / 2.0;
        assert!((left_before - left_after).abs() < 1e-9, "the held edge slid to {left_after}");
    }

    /// A rotated item resizes along **its own** axes. Dragging the right edge of an
    /// item turned 90° must widen it, not make it taller — this is the case that a
    /// resize written against screen axes gets visibly, obviously wrong.
    #[test]
    fn a_rotated_item_resizes_along_its_own_axes() {
        let turned = Placement { rotation: 90.0, ..square() };
        // With the item turned a quarter turn, its local +x points along world +y.
        let out = resize(&turned, Handle::Right, (0.0, 30.0), false);
        assert!((out.width - 130.0).abs() < 1e-6, "width {}", out.width);
        assert!((out.height - 100.0).abs() < 1e-6, "height {}", out.height);
    }

    /// Handles sit on the item as drawn. At 90° the handle named `Right` is at the
    /// bottom of the screen, which is exactly the point: it names a side of the *item*.
    #[test]
    fn handles_follow_the_item_round_as_it_turns() {
        let turned = Placement { rotation: 90.0, ..square() };
        let places = positions(&turned, 1.0);
        let right = places.iter().find(|(h, _)| *h == Handle::Right).unwrap().1;
        assert!(right.x.abs() < 1e-6, "x {}", right.x);
        assert!((right.y - 50.0).abs() < 1e-6, "y {}", right.y);
    }

    /// The rotate handle stands off the top edge by a fixed number of *screen* pixels,
    /// so in world units it has to shrink as the board is zoomed in. At a fitted zoom
    /// of 4% a constant world offset would put it 600 units above the item.
    #[test]
    fn the_rotate_handle_keeps_its_distance_on_screen_not_on_the_board() {
        let at_one = positions(&square(), 1.0);
        let at_four = positions(&square(), 4.0);
        let find = |v: &[(Handle, WorldPoint)]| v.iter().find(|(h, _)| h.is_rotate()).unwrap().1;
        let (one, four) = (find(&at_one), find(&at_four));
        // Both above the top edge at -50, by 24px and 6 world units respectively.
        assert!((one.y - -74.0).abs() < 1e-9, "{}", one.y);
        assert!((four.y - -56.0).abs() < 1e-9, "{}", four.y);
    }

    /// A corner and the two edges meeting it are within a few pixels of each other, so
    /// the order they are tested in decides what a press on a corner does. It has to
    /// be the corner.
    #[test]
    fn a_press_on_a_corner_grabs_the_corner_not_an_edge() {
        let hit = hit(&square(), WorldPoint::new(50.0, 50.0), 1.0);
        assert_eq!(hit, Some(Handle::BottomRight));
    }

    #[test]
    fn a_press_in_open_space_grabs_no_handle() {
        assert_eq!(hit(&square(), WorldPoint::new(0.0, 0.0), 1.0), None);
        assert_eq!(hit(&square(), WorldPoint::new(400.0, 400.0), 1.0), None);
    }

    /// The grab radius is in screen pixels, so zooming out must not make handles
    /// impossible to hit — at 4% the same 13px reach is 325 world units.
    #[test]
    fn handles_stay_the_same_size_to_aim_at_however_far_out_the_board_is() {
        let near = WorldPoint::new(50.0 + 200.0, 50.0);
        assert_eq!(hit(&square(), near, 1.0), None, "a 200-unit miss should not hit at 100%");
        assert_eq!(hit(&square(), near, 0.04), Some(Handle::BottomRight), "…but should at 4%");
    }

    /// Rotation is measured from the item's centre, clockwise, with the handle's rest
    /// position — straight up — as zero.
    #[test]
    fn rotating_measures_clockwise_from_straight_up() {
        let up = rotate(&square(), WorldPoint::new(0.0, -100.0), false);
        assert!((up.rotation - 0.0).abs() < 1e-9, "{}", up.rotation);

        let right = rotate(&square(), WorldPoint::new(100.0, 0.0), false);
        assert!((right.rotation - 90.0).abs() < 1e-9, "{}", right.rotation);

        let down = rotate(&square(), WorldPoint::new(0.0, 100.0), false);
        assert!((down.rotation - 180.0).abs() < 1e-9, "{}", down.rotation);

        let left = rotate(&square(), WorldPoint::new(-100.0, 0.0), false);
        assert!((left.rotation - 270.0).abs() < 1e-9, "{}", left.rotation);
    }

    /// Always in `0..360`, so the properties panel never shows −17° or 412°.
    #[test]
    fn rotation_is_normalised_whichever_way_it_is_turned() {
        for (x, y) in [(1.0, -1.0), (-1.0, -1.0), (-1.0, 1.0), (1.0, 1.0), (0.0, -1.0)] {
            let out = rotate(&square(), WorldPoint::new(x * 80.0, y * 80.0), false);
            assert!((0.0..360.0).contains(&out.rotation), "{}", out.rotation);
        }
    }

    #[test]
    fn shift_snaps_rotation_to_fifteen_degrees() {
        let snapped = rotate(&square(), WorldPoint::new(20.0, -100.0), true);
        assert!((snapped.rotation % SNAP_DEGREES).abs() < 1e-9, "{}", snapped.rotation);
        assert!((snapped.rotation - 15.0).abs() < 1e-9, "{}", snapped.rotation);
    }

    /// A rotate drag turns the item and changes nothing else. Moving the size or the
    /// centre here would make a rotation destructive.
    #[test]
    fn rotating_touches_only_the_rotation() {
        let before = Placement { x: 12.0, y: -8.0, ..square() };
        let after = rotate(&before, WorldPoint::new(100.0, 100.0), false);
        assert!((after.x - before.x).abs() < 1e-9);
        assert!((after.y - before.y).abs() < 1e-9);
        assert!((after.width - before.width).abs() < 1e-9);
        assert!((after.height - before.height).abs() < 1e-9);
        assert!((after.scale - before.scale).abs() < 1e-9);
    }

    /// `scale` and `width` are redundant. The panel writes width and leaves scale
    /// alone; so does this, or the two disagree and the item jumps when the panel is
    /// next touched.
    #[test]
    fn a_scaled_item_keeps_its_scale_and_moves_its_width() {
        let scaled = Placement { scale: 2.0, ..square() };
        assert_eq!(scaled.scaled_size(), (200.0, 200.0));
        let out = resize(&scaled, Handle::Right, (40.0, 0.0), false);
        assert!((out.scale - 2.0).abs() < 1e-9, "scale moved to {}", out.scale);
        assert!((out.scaled_size().0 - 240.0).abs() < 1e-9, "{:?}", out.scaled_size());
    }

    // ----- multi-selection ----------------------------------------------------------

    /// The group box is the envelope of the members' **rotated** corners. A widget turned
    /// 45° reaches further than its own width, and a box that cut through its own members
    /// would be visibly wrong.
    #[test]
    fn a_group_box_contains_its_rotated_members() {
        let flat = Placement::new(0.0, 0.0, 100.0, 20.0);
        let turned = Placement { rotation: 45.0, ..flat };

        let straight = group_bounds(&[flat]).unwrap();
        assert!((straight.width - 100.0).abs() < 1e-9);
        assert!((straight.height - 20.0).abs() < 1e-9);

        let diagonal = group_bounds(&[turned]).unwrap();
        assert!(diagonal.width > 100.0 * 0.7, "a 45° bar needs {} of width", diagonal.width);
        assert!(diagonal.height > 20.0, "height stayed at {}", diagonal.height);
        assert_eq!(group_bounds(&[]), None);
    }

    #[test]
    fn a_group_box_spans_every_member() {
        let left = Placement::new(-100.0, 0.0, 50.0, 50.0);
        let right = Placement::new(200.0, 40.0, 50.0, 50.0);
        let group = group_bounds(&[left, right]).unwrap();
        // From the left member's left edge to the right member's right edge.
        assert!((group.width - (225.0 - -125.0)).abs() < 1e-9, "width {}", group.width);
        assert!((group.x - 50.0).abs() < 1e-9, "centre {}", group.x);
    }

    /// A group offers corners and rotate and **no edges**. Scaling one axis of a group whose
    /// members are rotated asks for a sheared rectangle, and no `Placement` is one.
    #[test]
    fn a_group_offers_no_edge_handles() {
        assert_eq!(GROUP_HANDLES.len(), 5);
        for edge in [Handle::Top, Handle::Right, Handle::Bottom, Handle::Left] {
            assert!(!GROUP_HANDLES.contains(&edge), "{edge:?} would shear a rotated member");
        }
        let group = Group { x: 0.0, y: 0.0, width: 200.0, height: 100.0 };
        // And they are not reachable by aim either.
        let at = WorldPoint::new(100.0, 0.0); // the right edge's midpoint
        assert_ne!(group_hit(&group, at, 1.0), Some(Handle::Right));
    }

    /// Scaling holds the opposite corner still, and moves both the size *and* the centre —
    /// which is the difference between resizing a group and resizing each member in place.
    #[test]
    fn scaling_a_group_holds_the_opposite_corner_and_carries_the_members() {
        let group = Group { x: 0.0, y: 0.0, width: 200.0, height: 200.0 };
        // Drag the bottom-right corner out by the box's own size: a factor of two about the
        // top-left corner.
        let (factor, fixed) = group_scale(&group, Handle::BottomRight, (200.0, 200.0));
        assert!((factor - 2.0).abs() < 1e-9, "factor {factor}");
        assert_eq!(fixed, (-100.0, -100.0));

        // A member at the fixed corner does not move; one at the far corner moves furthest.
        let pinned = Placement::new(-100.0, -100.0, 20.0, 20.0);
        let far = Placement::new(100.0, 100.0, 20.0, 20.0);
        let pinned = scale_member(&pinned, factor, fixed);
        assert!((pinned.x - -100.0).abs() < 1e-9, "the held corner moved to {}", pinned.x);
        assert!((pinned.width - 40.0).abs() < 1e-9, "size {}", pinned.width);

        let far = scale_member(&far, factor, fixed);
        assert!((far.x - 300.0).abs() < 1e-9, "the far member is at {}", far.x);
    }

    /// The floor keeps a group from inverting: dragging a corner past the opposite one
    /// clamps rather than turning the box inside out.
    #[test]
    fn a_group_cannot_be_scaled_through_itself() {
        let group = Group { x: 0.0, y: 0.0, width: 200.0, height: 200.0 };
        let (factor, _) = group_scale(&group, Handle::BottomRight, (-100_000.0, -100_000.0));
        assert!(factor > 0.0, "factor {factor} inverted the box");

        let member = Placement::new(50.0, 50.0, 20.0, 20.0);
        let squashed = scale_member(&member, factor, (-100.0, -100.0));
        assert!(squashed.width >= MIN_SIZE / member.scale, "collapsed to {}", squashed.width);
        assert!(squashed.width.is_finite() && squashed.x.is_finite());
    }

    /// Rotating a group moves **two** things per member: its own angle, and its centre
    /// around the group's. That is why this is a different operation and not a bigger one.
    #[test]
    fn rotating_a_group_turns_each_member_and_carries_it_round() {
        let centre = (0.0, 0.0);
        let member = Placement::new(100.0, 0.0, 40.0, 20.0);

        let quarter = rotate_member(&member, centre, 90.0);
        assert!((quarter.rotation - 90.0).abs() < 1e-9, "angle {}", quarter.rotation);
        // Clockwise on screen: +x goes to +y, because y runs downwards.
        assert!(quarter.x.abs() < 1e-9, "x {}", quarter.x);
        assert!((quarter.y - 100.0).abs() < 1e-9, "y {}", quarter.y);

        // A member already at the centre only turns.
        let hub = Placement { rotation: 30.0, ..Placement::new(0.0, 0.0, 10.0, 10.0) };
        let turned = rotate_member(&hub, centre, 45.0);
        assert!((turned.rotation - 75.0).abs() < 1e-9);
        assert!(turned.x.abs() < 1e-9 && turned.y.abs() < 1e-9);

        // Four quarter turns is the identity, angle included.
        let mut round = member;
        for _ in 0..4 {
            round = rotate_member(&round, centre, 90.0);
        }
        assert!((round.x - member.x).abs() < 1e-6, "{} vs {}", round.x, member.x);
        assert!((round.rotation - member.rotation).abs() < 1e-6);
    }

    /// The angle convention matches `Placement::rotation`: clockwise, zero at the top.
    #[test]
    fn the_group_angle_is_clockwise_from_the_top() {
        let centre = (0.0, 0.0);
        assert!(angle_to(centre, WorldPoint::new(0.0, -100.0)).abs() < 1e-9);
        assert!((angle_to(centre, WorldPoint::new(100.0, 0.0)) - 90.0).abs() < 1e-9);
        assert!((angle_to(centre, WorldPoint::new(0.0, 100.0)) - 180.0).abs() < 1e-9);
        // Degenerate input is zero rather than NaN.
        assert_eq!(angle_to(centre, WorldPoint::new(0.0, 0.0)), 0.0);
    }

}

#[cfg(test)]
mod aspect_tests {
    use super::*;

    fn photo() -> Placement {
        // 4:3, the shape most of the reference board's pictures are.
        Placement { x: 0.0, y: 0.0, width: 400.0, height: 300.0, rotation: 0.0, scale: 1.0 }
    }

    /// A corner keeps the ratio; an edge is what distorts.
    ///
    /// *"if i change the size holding from the corners it should not change the aspect
    /// ratio … it should only distort the image if i am changing it by holding from the
    /// sides or the top"*. Both halves are asserted together because the feature is the
    /// *difference* between them — locking every grip would satisfy the first sentence
    /// and remove the only way to crop a picture's proportions deliberately.
    #[test]
    fn a_corner_keeps_a_pictures_ratio_and_an_edge_does_not() {
        let before = photo();
        let ratio = before.width / before.height;

        // A deliberately lopsided drag: mostly horizontal, barely vertical. A naive
        // "average the two axes" would pass a symmetric drag and fail this one.
        let corner = resize(&before, Handle::BottomRight, (120.0, 5.0), true);
        let after = corner.width / corner.height;
        assert!(
            (after - ratio).abs() < 1e-6,
            "a corner distorted a picture: {ratio} became {after}"
        );
        assert!(corner.width > before.width, "the corner drag did not grow it");

        let edge = resize(&before, Handle::Right, (120.0, 0.0), true);
        assert!(
            (edge.width / edge.height - ratio).abs() > 1e-3,
            "an edge drag kept the ratio, so there is no way to distort on purpose"
        );
        assert_eq!(edge.height, before.height, "an edge drag moved the other axis");
    }

    /// Everything that is not a picture still resizes freely from a corner — a sticky is
    /// a box you size to its content, and forcing a ratio there fights the common case.
    #[test]
    fn an_unlocked_corner_still_resizes_both_axes_freely() {
        let before = photo();
        let out = resize(&before, Handle::BottomRight, (120.0, 5.0), false);
        assert!((out.width / out.height - before.width / before.height).abs() > 1e-3);
    }

    /// The floor is applied to the *factor*, not per axis: clamping the short side alone
    /// would silently break the ratio this exists to keep.
    #[test]
    fn shrinking_past_the_floor_keeps_the_ratio() {
        let before = photo();
        let out = resize(&before, Handle::TopLeft, (5_000.0, 5_000.0), true);
        let ratio = before.width / before.height;
        assert!(
            (out.width / out.height - ratio).abs() < 1e-6,
            "the minimum-size floor broke the ratio: {} vs {ratio}",
            out.width / out.height
        );
        assert!(out.width >= MIN_SIZE && out.height >= MIN_SIZE);
    }

    // ----- connector ports --------------------------------------------------

    /// The four dots sit **outside** the item, one per edge, and they are the anchors a
    /// connector will actually be bound to.
    ///
    /// Outside is the load-bearing half. A port drawn on the outline overlaps the edge
    /// resize handle, and then which verb a press means — resize or draw a line — is decided
    /// by a pixel. The press path asks about ports first, so an overlap would make resizing
    /// the commonest item on a board unreliable rather than merely ambiguous.
    #[test]
    fn a_port_sits_outside_the_edge_it_belongs_to() {
        let placement = Placement::new(0.0, 0.0, 200.0, 100.0);
        let ports = ports(&placement, 1.0);
        assert_eq!(ports.len(), 4, "four dots, one per edge");

        for (anchor, at) in &ports {
            let on_edge = crate::connector::anchor_point(&placement, *anchor);
            let out = (at.x - on_edge.0).hypot(at.y - on_edge.1);
            assert!(
                (out - f64::from(PORT_OFFSET)).abs() < 1e-6,
                "a {anchor:?} port stands {out} from its edge, not {PORT_OFFSET}"
            );
            // Outward, not inward: the port must be further from the centre than the edge.
            assert!(
                at.x.hypot(at.y) > on_edge.0.hypot(on_edge.1),
                "the {anchor:?} port fell inside the item"
            );
        }

        // And they are the four `ANCHORS`, in that order, so what is drawn and what is
        // written into `ConnectorEnd::bound` are one value.
        let offered: Vec<(f64, f64)> = ports.iter().map(|(anchor, _)| *anchor).collect();
        assert_eq!(offered, crate::connector::ANCHORS.to_vec());
    }

    /// A port stands off by a constant number of **screen** pixels, so it neither swallows
    /// the item at a fitted zoom nor drifts out of reach when zoomed in.
    ///
    /// The same trade the rotate handle makes, and the failure it prevents is the same one:
    /// at a 4% fit a world-unit stand-off of 9 is a quarter of a pixel — four dots on top of
    /// each other and of the item's own corner.
    #[test]
    fn a_ports_stand_off_is_constant_on_screen() {
        let placement = Placement::new(0.0, 0.0, 200.0, 100.0);
        let out_at = |zoom: f64| {
            let (anchor, at) = ports(&placement, zoom)[1];
            let on_edge = crate::connector::anchor_point(&placement, anchor);
            (at.x - on_edge.0).hypot(at.y - on_edge.1) * zoom
        };
        assert!((out_at(0.04) - f64::from(PORT_OFFSET)).abs() < 1e-6);
        assert!((out_at(1.0) - f64::from(PORT_OFFSET)).abs() < 1e-6);
        assert!((out_at(8.0) - f64::from(PORT_OFFSET)).abs() < 1e-6);
    }

    /// A turned item's ports stay on its four edges rather than at the compass points of
    /// its bounding box.
    ///
    /// At 90° the "top" edge of the item is on the **right** of the screen, so a port
    /// resolved against the axis-aligned box would be a dot floating above an edge that is
    /// not there — and the connector leaving it would start somewhere the user did not aim.
    #[test]
    fn a_rotated_items_ports_follow_its_own_edges() {
        let mut placement = Placement::new(0.0, 0.0, 200.0, 100.0);
        placement.rotation = 90.0;
        let ports = ports(&placement, 1.0);
        let (_, top) = ports[0];
        // The item's own "up" now points along +x, so its top-edge port is out to the right
        // by half the *height* plus the stand-off.
        assert!(top.x > 0.0, "the top port did not follow the rotation: {top:?}");
        assert!(top.x.abs() > top.y.abs(), "the top port is still above the box: {top:?}");
        assert!(
            (top.x - (50.0 + f64::from(PORT_OFFSET))).abs() < 1e-6,
            "the top port is at {top:?}, not half the height plus the stand-off out"
        );
    }

    /// A port's grab circle must not reach a corner resize handle's.
    ///
    /// The press path asks about ports **first**, so an overlap does not read as ambiguity —
    /// it reads as *"resizing from the corner sometimes draws a line instead"*, on the
    /// commonest gesture on the board. At the original 9-pixel stand-off the two circles met
    /// exactly, which is how the user came to say the dots were *"too close"*: they were
    /// close enough to be in each other's way, not merely close enough to look crowded.
    ///
    /// Measured on a **small** item, where the four ports and the four corners are nearest
    /// to each other in absolute terms.
    #[test]
    fn a_port_clears_the_corner_handles_reach() {
        let placement = Placement::new(0.0, 0.0, 60.0, 40.0);
        let corners: Vec<WorldPoint> = positions(&placement, 1.0)
            .into_iter()
            .filter(|(handle, _)| handle.is_corner())
            .map(|(_, at)| at)
            .collect();
        let corner_reach = f64::from(HANDLE_SIZE / 2.0 + GRAB_SLOP);
        let port_reach = f64::from(PORT_RADIUS + PORT_SLOP);

        for (anchor, at) in ports(&placement, 1.0) {
            for corner in &corners {
                let gap = (at.x - corner.x).hypot(at.y - corner.y);
                assert!(
                    gap > corner_reach + port_reach,
                    "the {anchor:?} port is {gap:.1} from a corner handle, and the two \
                     grab circles reach {:.1} between them — a press near the corner is \
                     decided by whichever is asked first",
                    corner_reach + port_reach
                );
            }
        }
    }

    /// A press picks the **nearest** port, not the first one within reach.
    ///
    /// Zoomed out the grab radius is large in world units and two ports are routinely both
    /// inside it, so taking the first would answer with whichever comes earliest in
    /// `ANCHORS` — a line leaving the top of a box you grabbed on the left.
    #[test]
    fn a_press_takes_the_nearest_port() {
        let placement = Placement::new(0.0, 0.0, 40.0, 40.0);
        // At a fitted zoom every port is inside every other's radius.
        let zoom = 0.05;
        let ports = ports(&placement, zoom);
        for (anchor, at) in ports {
            assert_eq!(
                port_hit(&placement, at, zoom),
                Some(anchor),
                "a press exactly on the {anchor:?} port answered with another one"
            );
        }

        // And far away is no port at all, rather than the least distant one.
        assert_eq!(port_hit(&placement, WorldPoint::new(9_000.0, 9_000.0), 1.0), None);
    }
}
