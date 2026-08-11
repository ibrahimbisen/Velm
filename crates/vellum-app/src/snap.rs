//! Relative snapping — Miro calls it **Align objects**, and it is the whole of
//! *"i love how miro has implemented it, it is not super strict, very loose but very
//! useful"*.
//!
//! # What Miro does, and what of it is here
//!
//! Miro's Preferences (the `⋮` menu, top left) carry one toggle called *Align objects*.
//! With it on, moving, resizing or creating something makes blue guide lines appear that
//! **suggest** alignments with other objects and **equal spacing** between them. It is not
//! snap-to-grid, which Miro has separately and which a lot of its users complain about
//! precisely because it *is* strict. Two things make the difference, and both are decisions
//! this module makes rather than details it inherits:
//!
//! - **The tolerance is in screen pixels, not board units.** [`SNAP_PIXELS`] is converted
//!   to world units by dividing by the zoom, so the pull feels identical at 4% and at 800%.
//!   A world-unit tolerance is the thing that makes a snapping implementation feel *strict*:
//!   zoomed out it grabs everything within half a screen, zoomed in it never fires.
//! - **Each axis is decided on its own.** A move that lines up vertically and not
//!   horizontally is corrected in x and left alone in y. Requiring both is what makes an
//!   object feel like it is fighting you.
//!
//! Everything here is **pure**: rectangles in, a correction and some guides out. No camera,
//! no document, no window — so the behaviour is testable without any of them, which is what
//! the rest of this crate does with `handle` and `edit`.
//!
//! # What is snapped to
//!
//! Six lines per box — `min`, the centre and `max` on each axis — against the same six of
//! every candidate. That is Miro's set: edges *and* centres, so a small sticky can be
//! centred on a big frame as easily as aligned to its left edge.
//!
//! Equal spacing is the second half and it is what makes the feature *useful* rather than
//! merely tidy. Given two boxes already separated by a gap, dragging a third offers the
//! position that repeats that gap — and the position exactly between them. Only boxes that
//! overlap the dragged one on the *other* axis are considered, because "evenly spaced" means
//! nothing between items in different rows.

use vellum_scene::{WorldPoint, WorldRect};

/// How close, **in device pixels**, two lines have to be before they snap.
///
/// Four, down from six, on the report that it grabbed too eagerly — *"make the
/// snapping softer"*. The number is the whole of how the feature feels and it is the only
/// honest lever: everything else that could be called softening either stops the snap
/// aligning anything (a partial correction lands on neither the edge nor the pointer) or
/// makes it *stickier* rather than looser (hysteresis holds a snap you are trying to leave).
///
/// Reach it by cutting how much is in range instead. There is more of that than the number
/// suggests, because each axis offers three lines against each candidate's three: nine ways
/// to be caught per axis per neighbour, so on a busy board *something* is nearly always
/// within reach. Two thirds of the range is a third fewer chances to be grabbed by a line
/// that was not the point.
///
/// **Device pixels, not logical points** — `docs/06` and trap 4: screen coordinates are
/// physical everywhere. On a retina display this is already about two points of travel.
pub const SNAP_PIXELS: f64 = 4.0;

/// How many neighbours the **equal-spacing** pass considers, nearest first.
///
/// Alignment is linear in the candidate count and can afford every visible item; spacing
/// compares *pairs* and cannot. On a board where a thousand items are on screen this is the
/// difference between 1,000 comparisons and 1,000,000 of them, sixty times a second. Forty
/// is far more than any real run of evenly spaced things and is bounded work.
const SPACING_NEIGHBOURS: usize = 40;

/// Which way a guide line runs.
///
/// A `Vertical` guide has a fixed **x** and runs up and down the board — the guide you see
/// when two things line up on their left edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

/// One line to draw while a gesture is in progress.
///
/// The span is carried rather than derived, because Miro draws a guide only across the
/// items it actually joins rather than across the whole screen — a full-width line says
/// *something on this row lines up*, which is a different and much less useful statement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    pub axis: Axis,
    /// Where the line sits on its own axis, in world units.
    pub at: f64,
    /// The line's extent along the *other* axis, in world units.
    pub from: f64,
    pub to: f64,
    /// The gap this guide is reporting, when it is an equal-spacing hint rather than an
    /// alignment. Drawn differently: an alignment is a plain line, a spacing hint is a
    /// short segment with a tick at each end, because it is measuring rather than aligning.
    pub gap: Option<f64>,
}

/// The correction a gesture should take, and what to draw for it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snap {
    /// Added to the gesture's own offset. Zero on an axis with no match.
    pub dx: f64,
    pub dy: f64,
    pub guides: Vec<Guide>,
}

impl Snap {
    pub fn is_empty(&self) -> bool {
        self.guides.is_empty()
    }
}

/// The three lines a box offers on one axis: its two edges and its centre.
fn lines(rect: &WorldRect, axis: Axis) -> [f64; 3] {
    match axis {
        Axis::Vertical => [rect.min.x, rect.center().x, rect.max.x],
        Axis::Horizontal => [rect.min.y, rect.center().y, rect.max.y],
    }
}

/// The box's extent along the axis a guide of `axis` would *span*.
fn span(rect: &WorldRect, axis: Axis) -> (f64, f64) {
    match axis {
        Axis::Vertical => (rect.min.y, rect.max.y),
        Axis::Horizontal => (rect.min.x, rect.max.x),
    }
}

/// The best correction on one axis, and every candidate that agrees with it.
///
/// "Best" is the smallest movement, which is what makes the nearest thing win when several
/// are in reach. Ties go to whichever candidate is found first, and it does not matter: they
/// are at the same coordinate, so the correction is identical and only the guide's span
/// changes — and every agreeing candidate is folded into that span anyway.
fn align_axis(
    moving: &WorldRect,
    candidates: &[WorldRect],
    tolerance: f64,
    axis: Axis,
    centres: bool,
) -> Option<(f64, Guide)> {
    let mine = lines(moving, axis);
    let mut best: Option<(f64, f64)> = None; // (delta, the line landed on)
    for candidate in candidates {
        let theirs = lines(candidate, axis);
        // A resize sees edges only. The moving side collapses to a single line — the edge
        // the handle is dragging — but the *candidates* still offer three, and the middle
        // one is a centre: without this a corner drag snaps onto another item's centre,
        // which is a line the user cannot see and did not ask for.
        let offered: &[f64] = if centres { &theirs } else { &[theirs[0], theirs[2]] };
        for &theirs in offered {
            for line in mine {
                let delta = theirs - line;
                if delta.abs() <= tolerance
                    && best.is_none_or(|(current, _)| delta.abs() < current.abs())
                {
                    best = Some((delta, theirs));
                }
            }
        }
    }

    let (delta, at) = best?;
    // The span covers the moving box *after* the correction, plus every candidate holding
    // the same line — so a guide through three aligned stickies is drawn from the topmost
    // to the bottommost rather than only across the pair that triggered it.
    let (mut from, mut to) = span(moving, axis);
    for candidate in candidates {
        if lines(candidate, axis).iter().any(|line| (line - at).abs() <= 1e-9) {
            let (a, b) = span(candidate, axis);
            from = from.min(a);
            to = to.max(b);
        }
    }
    Some((delta, Guide { axis, at, from, to, gap: None }))
}

/// Positions that would make the moving box evenly spaced with a run of others.
///
/// Two offers per neighbouring pair, which between them are every way a person means
/// "space this like those": **continue the run** — the same gap again, beyond either end —
/// and **fill the middle**, where the moving box sits between two others with equal gaps on
/// both sides.
fn spacing_axis(
    moving: &WorldRect,
    candidates: &[WorldRect],
    tolerance: f64,
    axis: Axis,
) -> Option<(f64, Vec<Guide>)> {
    // Only things in the same band. Two stickies a screen apart vertically are not a row,
    // and offering to space against them produces a guide that points at nothing.
    let (my_from, my_to) = span(moving, axis);
    let mut band: Vec<&WorldRect> = candidates
        .iter()
        .filter(|candidate| {
            let (from, to) = span(candidate, axis);
            from <= my_to && to >= my_from
        })
        .collect();
    if band.len() < 2 {
        return None;
    }

    let (my_min, my_max) = match axis {
        Axis::Vertical => (moving.min.x, moving.max.x),
        Axis::Horizontal => (moving.min.y, moving.max.y),
    };
    let extent = my_max - my_min;
    let edges = |rect: &WorldRect| match axis {
        Axis::Vertical => (rect.min.x, rect.max.x),
        Axis::Horizontal => (rect.min.y, rect.max.y),
    };

    band.sort_by(|a, b| edges(a).0.total_cmp(&edges(b).0));
    band.truncate(SPACING_NEIGHBOURS);

    let mut best: Option<(f64, f64, [usize; 2])> = None; // (delta, gap, the pair)
    let mut offer = |delta: f64, gap: f64, pair: [usize; 2]| {
        if gap > 0.0
            && delta.abs() <= tolerance
            && best.is_none_or(|(current, ..)| delta.abs() < current.abs())
        {
            best = Some((delta, gap, pair));
        }
    };

    for i in 0..band.len() - 1 {
        let (_, a_max) = edges(band[i]);
        let (b_min, b_max) = edges(band[i + 1]);
        let gap = b_min - a_max;
        if gap <= 0.0 {
            continue;
        }
        // Continue the run, past the right-hand one and past the left-hand one.
        offer(b_max + gap - my_min, gap, [i, i + 1]);
        let (a_min, _) = edges(band[i]);
        offer(a_min - gap - my_max, gap, [i, i + 1]);

        // And fill the middle of this pair, when the moving box fits between them at all.
        let room = b_min - a_max;
        if room > extent {
            let each = (room - extent) / 2.0;
            offer(a_max + each - my_min, each, [i, i + 1]);
        }
    }

    let (delta, gap, [i, j]) = best?;
    // The guides for a spacing hint are the gaps themselves — one per equal interval —
    // drawn along the middle of the band so they read as measurements between the boxes.
    let mid = (my_from + my_to) / 2.0;
    let mut spans: Vec<(f64, f64)> = vec![(edges(band[i]).1, edges(band[j]).0)];
    let moved = (my_min + delta, my_max + delta);
    for candidate in [band[i], band[j]] {
        let (from, to) = edges(candidate);
        if (moved.0 - to - gap).abs() < 1e-6 {
            spans.push((to, moved.0));
        }
        if (from - moved.1 - gap).abs() < 1e-6 {
            spans.push((moved.1, from));
        }
    }
    let guides = spans
        .into_iter()
        .filter(|(from, to)| to > from)
        .map(|(from, to)| Guide { axis: axis_across(axis), at: mid, from, to, gap: Some(gap) })
        .collect();
    Some((delta, guides))
}

/// A spacing hint runs *across* the axis its gap is measured on: a horizontal gap between
/// two boxes is drawn as a horizontal segment, which is a guide with a fixed **y**.
const fn axis_across(axis: Axis) -> Axis {
    match axis {
        Axis::Vertical => Axis::Horizontal,
        Axis::Horizontal => Axis::Vertical,
    }
}

/// The correction a box being moved should take.
///
/// `tolerance` is in **world** units and the caller is expected to have divided
/// [`SNAP_PIXELS`] by the zoom — see the module header for why that is the whole feel of
/// the feature. `candidates` must not include the box being moved, or it will snap to
/// itself and never move at all.
pub fn snap_move(moving: WorldRect, candidates: &[WorldRect], tolerance: f64) -> Snap {
    let mut snap = Snap::default();
    // Alignment first and spacing only where alignment found nothing, per axis. They can
    // both be in reach at once and they disagree: alignment says *this edge*, spacing says
    // *this gap*, and applying both moves the box to neither. Alignment wins because it is
    // the stronger statement — an edge is exact, a gap is a rhythm.
    for axis in [Axis::Vertical, Axis::Horizontal] {
        let (delta, guides) = match align_axis(&moving, candidates, tolerance, axis, true) {
            Some((delta, guide)) => (delta, vec![guide]),
            // The **same** axis, not the crossed one: `align_axis(Vertical)` and
            // `spacing_axis(Vertical)` both answer with an *x* correction. It is the
            // guide a gap draws that runs the other way, and that crossing happens
            // inside `spacing_axis`, once.
            None => match spacing_axis(&moving, candidates, tolerance, axis) {
                Some(found) => found,
                None => continue,
            },
        };
        match axis {
            Axis::Vertical => snap.dx = delta,
            Axis::Horizontal => snap.dy = delta,
        }
        snap.guides.extend(guides);
    }
    snap
}

/// The correction a box being **resized** should take, given which corner or edge is moving.
///
/// Only the edges the handle is actually moving are offered, which is the difference between
/// a resize that snaps and one that fights: dragging the right edge must never correct the
/// left one, and a correction that moved the fixed edge would make the box jump out from
/// under the pointer.
///
/// `moves_min`/`moves_max` name the moving edges per axis — `(false, true)` for a handle on
/// the right or the bottom.
pub fn snap_resize(
    moving: WorldRect,
    candidates: &[WorldRect],
    tolerance: f64,
    moves: [(bool, bool); 2],
) -> Snap {
    let mut snap = Snap::default();
    for (index, axis) in [Axis::Vertical, Axis::Horizontal].into_iter().enumerate() {
        let (min_moves, max_moves) = moves[index];
        if !min_moves && !max_moves {
            continue;
        }
        // A one-sided box, so `align_axis` only ever sees the edges that are free to move.
        // The centre is deliberately not offered during a resize: centring a box on another
        // while dragging its corner moves both of its edges, which is not what the handle
        // under the pointer says it is doing.
        let (from, to) = span(&moving, axis);
        let probe = |line: f64| match axis {
            Axis::Vertical => WorldRect::from_corners(
                WorldPoint::new(line, from),
                WorldPoint::new(line, to),
            ),
            Axis::Horizontal => WorldRect::from_corners(
                WorldPoint::new(from, line),
                WorldPoint::new(to, line),
            ),
        };
        let mine = lines(&moving, axis);
        let mut best: Option<(f64, Guide)> = None;
        for (edge, moving_edge) in [(mine[0], min_moves), (mine[2], max_moves)] {
            if !moving_edge {
                continue;
            }
            if let Some((delta, guide)) =
                align_axis(&probe(edge), candidates, tolerance, axis, false)
                && best.as_ref().is_none_or(|(current, _)| delta.abs() < current.abs())
            {
                best = Some((delta, guide));
            }
        }
        if let Some((delta, guide)) = best {
            match axis {
                Axis::Vertical => snap.dx = delta,
                Axis::Horizontal => snap.dy = delta,
            }
            snap.guides.push(guide);
        }
    }
    snap
}

/// Miro's **Snap to grid**: the correction that puts a moving box onto the board's own grid.
///
/// A different feature from everything above, and the difference is what the user asked for
/// when they asked for both. Relative snapping (`snap_move`) is *loose* — it pulls onto the
/// things already on the board, within a few screen pixels, and does nothing where there is
/// nothing. This is *strict*: there is a line everywhere, so it always has an answer, and a
/// box under it can only ever be on the grid.
///
/// # What is snapped, and why it is the near edges
///
/// The box's **minimum** corner, not its centre and not the nearest of its own three lines.
/// A grid is a coordinate system; laying things out against one means their top-left corners
/// line up, which is what makes two items on the same row look like a row. Snapping the
/// centre would put a 199-wide sticky's *edges* half a unit off every line, which is the one
/// thing a person turning this on is trying to avoid.
///
/// # No tolerance
///
/// Unlike the relative snap there is no "within N pixels" test, because there is nowhere to
/// be outside it: every point is within half a step of a line. A tolerance here would mean
/// the box sometimes lands on the grid and sometimes does not, which is worse than either
/// answer. This is the strict one; strictness is the whole reason it is a separate switch.
///
/// `step` is the world spacing the board is drawn at, so the correction always lands on a
/// line the user can *see*. Non-finite or non-positive steps answer with no correction
/// rather than a division by zero.
///
/// Produces **no guides**. The grid is already on screen — that is what makes it a grid —
/// so a line drawn over one already there says nothing.
pub fn snap_to_grid(moving: WorldRect, step: f64, moves: [(bool, bool); 2]) -> Snap {
    let mut snap = Snap::default();
    if !step.is_finite() || step <= 0.0 {
        return snap;
    }
    let to_line = |value: f64| (value / step).round() * step - value;
    // `[(min, max); 2]` in `[vertical, horizontal]` order, matching `Handle::moving_edges`.
    // A move reports both edges moving on both axes; a resize reports only the ones its
    // handle is dragging, so the fixed edge is not pulled out from under the pointer.
    let [(left, right), (top, bottom)] = moves;
    if left || right {
        // The edge the gesture is actually moving. For a plain move both are true and the
        // minimum is the one that matters; for a right-handle resize only the far edge is.
        snap.dx = if left { to_line(moving.min.x) } else { to_line(moving.max.x) };
    }
    if top || bottom {
        snap.dy = if top { to_line(moving.min.y) } else { to_line(moving.max.y) };
    }
    snap
}

/// Both edges on both axes: what a **move** reports to [`snap_to_grid`], since a move takes
/// the whole box with it. Named rather than written out at the call sites so a move and a
/// resize are visibly asking the same function a different question.
pub const MOVES_WHOLE_BOX: [(bool, bool); 2] = [(true, true), (true, true)];

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> WorldRect {
        WorldRect::from_origin_size(WorldPoint::new(x, y), w, h)
    }

    /// The core: an edge within the tolerance is pulled onto it, and the guide spans both
    /// boxes rather than the screen.
    #[test]
    fn a_near_edge_snaps_and_draws_a_guide_across_both_boxes() {
        let target = rect(0.0, 0.0, 100.0, 100.0);
        // Four units to the right of the target's left edge, and far below it.
        let moving = rect(4.0, 400.0, 50.0, 50.0);
        let snap = snap_move(moving, &[target], 6.0);

        assert_eq!(snap.dx, -4.0, "the left edges line up");
        assert_eq!(snap.dy, 0.0, "nothing is in reach vertically");
        let guide = snap.guides.iter().find(|g| g.axis == Axis::Vertical).expect("a guide");
        assert_eq!(guide.at, 0.0);
        assert_eq!((guide.from, guide.to), (0.0, 450.0), "the guide joins the two boxes");
    }

    /// Each axis is decided alone. This is what "not super strict" means in practice: a
    /// gesture that lines up one way is not dragged into lining up the other.
    #[test]
    fn the_axes_are_independent() {
        let target = rect(0.0, 0.0, 100.0, 100.0);
        let moving = rect(3.0, 500.0, 100.0, 100.0);
        let snap = snap_move(moving, &[target], 6.0);
        assert_eq!(snap.dx, -3.0);
        assert_eq!(snap.dy, 0.0);
        assert_eq!(snap.guides.len(), 1, "one axis matched, so one guide: {:?}", snap.guides);
    }

    /// Beyond the tolerance nothing happens at all — no correction and, just as important,
    /// no guide. A guide with no snap behind it is a line that lies.
    #[test]
    fn nothing_outside_the_tolerance_snaps_or_draws() {
        let target = rect(0.0, 0.0, 100.0, 100.0);
        let snap = snap_move(rect(40.0, 400.0, 50.0, 50.0), &[target], 6.0);
        assert_eq!((snap.dx, snap.dy), (0.0, 0.0));
        assert!(snap.is_empty(), "{:?}", snap.guides);
    }

    /// Centres count, not only edges — this is how a small note is centred on a big frame.
    #[test]
    fn a_centre_snaps_to_a_centre() {
        let frame = rect(0.0, 0.0, 1000.0, 600.0);
        // Centre at 497, five short of the frame's 500.
        let moving = rect(447.0, 2000.0, 100.0, 100.0);
        let snap = snap_move(moving, &[frame], 6.0);
        assert_eq!(snap.dx, 3.0);
        assert_eq!(snap.guides[0].at, 500.0);
    }

    /// The nearest line wins when several are in reach, so the correction is always the
    /// smallest movement that explains itself.
    #[test]
    fn the_nearest_line_wins() {
        let left = rect(0.0, 0.0, 100.0, 100.0);
        let right = rect(105.0, 0.0, 100.0, 100.0);
        // Left edge at 103: two from the right box's left edge, three from the left box's
        // right edge.
        let snap = snap_move(rect(103.0, 400.0, 20.0, 20.0), &[left, right], 6.0);
        assert_eq!(snap.dx, 2.0, "snapped to the further of the two");
    }

    /// Equal spacing, which is the half that makes it *useful* rather than merely tidy.
    /// Two boxes 50 apart; a third dragged near the same gap beyond them takes it exactly.
    #[test]
    fn a_third_box_continues_an_even_run() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(150.0, 0.0, 100.0, 100.0);
        // The run continues at x = 300. Four short of it, and no edge is within reach:
        // the nearest edge line is b's right at 250.
        let snap = snap_move(rect(296.0, 0.0, 100.0, 100.0), &[a, b], 6.0);
        assert_eq!(snap.dx, 4.0, "the gap is repeated");
        assert!(
            snap.guides.iter().any(|g| g.gap == Some(50.0)),
            "a spacing hint reports the gap: {:?}",
            snap.guides,
        );
    }

    /// Spacing only applies within a band. Items in another row are not a run.
    #[test]
    fn a_box_in_another_row_is_not_part_of_the_run() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(150.0, 0.0, 100.0, 100.0);
        // Same x arithmetic as above, but 5,000 units down.
        let snap = snap_move(rect(296.0, 5_000.0, 100.0, 100.0), &[a, b], 6.0);
        assert_eq!(snap.dx, 0.0, "nothing to be evenly spaced with");
        assert!(snap.is_empty());
    }

    /// Alignment beats spacing when both are in reach, because an edge is an exact
    /// statement and a gap is a rhythm. Applying both would land on neither.
    #[test]
    fn an_alignment_wins_over_a_spacing_hint() {
        let a = rect(0.0, 0.0, 100.0, 100.0);
        let b = rect(150.0, 0.0, 100.0, 100.0);
        // x = 252 is two from b's right edge (an alignment) and the run would continue at
        // 300, so only the alignment is in reach — put a third box where both are.
        let c = rect(298.0, 0.0, 100.0, 100.0);
        let snap = snap_move(rect(252.0, 0.0, 100.0, 100.0), &[a, b, c], 6.0);
        assert_eq!(snap.dx, -2.0, "the edge, not the gap");
        assert!(snap.guides.iter().all(|g| g.gap.is_none()), "{:?}", snap.guides);
    }

    /// A resize only ever corrects the edge the handle is moving. Correcting the fixed one
    /// would slide the box out from under the pointer.
    #[test]
    fn a_resize_leaves_the_edge_that_is_not_moving_alone() {
        let target = rect(0.0, 0.0, 100.0, 100.0);
        // The moving box's *left* edge is 3 from the target's left, and its right edge is
        // 4 from the target's right. Only the right edge is being dragged.
        let moving = rect(3.0, 400.0, 93.0, 50.0);
        let snap = snap_resize(moving, &[target], 6.0, [(false, true), (false, false)]);
        assert_eq!(snap.dx, 4.0, "the right edge went to 100, not the left to 0");
        assert_eq!(snap.dy, 0.0, "no vertical handle, no vertical correction");
    }

    /// And a resize does not offer centres: dragging one corner onto another item's centre
    /// would have to move both of the box's edges, which is not what the handle says.
    #[test]
    fn a_resize_does_not_snap_to_a_centre() {
        let target = rect(0.0, 0.0, 100.0, 100.0);
        // The moving box's right edge sits 2 from the target's *centre* at 50, and nothing
        // is near either of the target's own edges.
        let moving = rect(-500.0, 400.0, 548.0, 50.0);
        let snap = snap_resize(moving, &[target], 6.0, [(false, true), (false, false)]);
        assert_eq!(snap.dx, 0.0, "a centre is not a resize target: {snap:?}");
    }

    /// Self-snapping is the caller's mistake to avoid and worth stating: a box in its own
    /// candidate list matches itself at zero and pins the gesture in place.
    #[test]
    fn a_box_in_its_own_candidates_never_moves() {
        let moving = rect(4.0, 400.0, 50.0, 50.0);
        let target = rect(0.0, 0.0, 100.0, 100.0);
        let snap = snap_move(moving, &[target, moving], 6.0);
        assert_eq!(snap.dx, 0.0, "it matched itself, which is why callers must exclude it");
    }

    /// A move lands its **near corner** on the nearest line, on both axes independently.
    #[test]
    fn snapping_to_the_grid_puts_the_minimum_corner_on_a_line() {
        let snap = snap_to_grid(rect(103.0, 47.0, 199.0, 228.0), 50.0, MOVES_WHOLE_BOX);
        assert!((snap.dx - -3.0).abs() < 1e-9, "103 → 100, so back by 3; got {}", snap.dx);
        assert!((snap.dy - 3.0).abs() < 1e-9, "47 → 50, so on by 3; got {}", snap.dy);
        assert!(snap.guides.is_empty(), "the grid is already drawn; a line over it says nothing");
    }

    /// Exactly on the grid already, which has to be a no-op rather than a nudge to the
    /// *next* line — a gesture that cannot come to rest is the failure mode of a snap.
    #[test]
    fn a_box_already_on_the_grid_is_left_alone() {
        let snap = snap_to_grid(rect(200.0, -150.0, 100.0, 100.0), 50.0, MOVES_WHOLE_BOX);
        assert_eq!((snap.dx, snap.dy), (0.0, 0.0));
    }

    /// Negative coordinates snap the same way. `round` is half-away-from-zero, which is
    /// symmetric about the origin — the board runs both ways from it and a grid that
    /// behaved differently on the left of the origin would be visible.
    #[test]
    fn the_grid_reaches_the_negative_half_of_the_board() {
        let snap = snap_to_grid(rect(-103.0, -47.0, 60.0, 60.0), 50.0, MOVES_WHOLE_BOX);
        assert!((snap.dx - 3.0).abs() < 1e-9, "-103 → -100; got {}", snap.dx);
        assert!((snap.dy - -3.0).abs() < 1e-9, "-47 → -50; got {}", snap.dy);
    }

    /// A resize corrects **only the edge its handle is dragging**. Correcting the fixed
    /// edge would slide the box out from under the pointer, which is the rule the relative
    /// snap already follows and the reason `moves` is threaded through at all.
    #[test]
    fn a_resize_snaps_the_edge_that_is_moving_and_not_the_one_that_is_not() {
        let box_ = rect(103.0, 47.0, 199.0, 228.0);
        // The right handle: the far x edge moves, nothing on y does.
        let right = crate::handle::Handle::Right.moving_edges();
        let snap = snap_to_grid(box_, 50.0, right);
        assert!((snap.dx - -2.0).abs() < 1e-9, "302 → 300; got {}", snap.dx);
        assert_eq!(snap.dy, 0.0, "the right handle does not move a horizontal edge");

        // …and the left handle takes the near one, from the same box.
        let left = crate::handle::Handle::Left.moving_edges();
        assert!((snap_to_grid(box_, 50.0, left).dx - -3.0).abs() < 1e-9);
    }

    /// A step that is not a positive number answers with no correction rather than a
    /// division by zero. `grid_step` returns `None` at the extremes of the zoom clamp, and
    /// a caller that mapped that to `0.0` must not produce a NaN placement.
    #[test]
    fn a_step_that_is_not_a_spacing_corrects_nothing() {
        for step in [0.0, -50.0, f64::NAN, f64::INFINITY] {
            let snap = snap_to_grid(rect(103.0, 47.0, 60.0, 60.0), step, MOVES_WHOLE_BOX);
            assert_eq!((snap.dx, snap.dy), (0.0, 0.0), "step {step}");
        }
    }
}
