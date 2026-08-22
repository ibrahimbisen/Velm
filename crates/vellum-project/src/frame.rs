//! Whether a frame has clipped one of its own contents out of the drawing.
//!
//! # Why this module exists
//!
//! A frame is a viewport over the board: things sit *on* it, and Miro hides the part of an
//! item that has left it. Velm does the same, at item granularity rather than per pixel — an
//! item wholly outside its ancestor frame is not drawn at all. That is cheap, it is the case
//! a viewer actually notices, and it is what stops an item dragged off a frame leaking across
//! the board.
//!
//! ⚠ **Item granularity, not per pixel.** An item that *straddles* its frame's edge is drawn
//! whole, hanging over the edge. The full behaviour needs a scissor rect per frame, which is
//! a renderer change; this is the half that matters and it is stated here rather than left to
//! be discovered.
//!
//! This lived in `vellum-app`'s painter, where the browser painter could not reach it — so
//! the two front ends drew different boards. An item the user dragged off a frame on the Mac
//! was hidden natively and still visible in the tab, and the board is supposed to look the
//! same in both. It is here for the reason [`crate::look`] gives at length: **one derivation,
//! not two**, because this repository has paid for a second copy three times already and the
//! failure is never on the day the copies are written.
//!
//! There is a second, sharper reason this particular decision belongs in a shared crate.
//! `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so **nothing in it can be tested at
//! all**; `vellum-app`'s copy had two tests and no coverage of a nested chain, a cycle or a
//! dangling parent. Here the tests simply run.
//!
//! # What this deliberately does not decide
//!
//! Only whether the item is **drawn**. A clipped item is still in the document, still in the
//! R-tree, and still answers a hit-test — see the note on [`clipped_by_frame`].

use vellum_doc::ItemKind;
use vellum_scene::{ItemId as SceneId, WorldRect};

use crate::project::{Projected, Projection};

/// How deep the containment chain is walked before giving up. Miro's own nesting is
/// a frame containing a group containing items; 64 is far past anything real.
///
/// Private on purpose: a second walker with its own bound is the copy this module exists to
/// prevent, and a caller that wants to know "is this clipped" wants [`clipped_by_frame`].
const MAX_NESTING: usize = 64;

/// Whether an ancestor frame clips this item away entirely.
///
/// Walks the containment chain upwards and answers `true` at the first **frame** whose box
/// does not intersect the item's. A group is not a frame and does not clip — conflating the
/// two would hide anything dragged out of a group, which is a grouping gesture and not a
/// filing one.
///
/// ⚠ **This hides the item; it does not deselect it.** A clipped item is still returned by
/// `Scene::hit_test` and by a marquee, and its selection ring is still drawn — that surviving
/// ring is what identified this behaviour as the cause the last time it was mistaken for a
/// missing texture. The application half of the answer is that dragging an item off a frame
/// re-files it (`ActiveState::reframe`), so the ordinary way into this state closes itself.
pub fn clipped_by_frame(projected: &Projected, projection: &Projection) -> bool {
    // `move`, so the closure holds the `&Projection` itself rather than a reference to
    // it: the ancestor it hands back has to live as long as `projection`, not as long as
    // one call to the closure.
    clipped_by(projected.parent, &projected.bounds, move |id| projection.get(id))
}

/// The decision, over any ancestry lookup.
///
/// Split out **solely so the states a real document cannot represent can be tested**: a
/// `Projection` is built from a Loro tree, which cannot hold a parent cycle, and its item map
/// is private to [`crate::project`], so neither a cycle nor a parent id that is absent can be
/// reached from a sibling module. Both are exactly the cases the bound below exists for, and
/// an untested bound is a bound nobody has run. Private — [`clipped_by_frame`] is the API.
fn clipped_by<'a>(
    mut parent: Option<SceneId>,
    bounds: &WorldRect,
    lookup: impl Fn(SceneId) -> Option<&'a Projected>,
) -> bool {
    // Bounded rather than `while let`: a corrupt document could in principle
    // describe a cycle, and a render loop is the worst place to discover one.
    for _ in 0..MAX_NESTING {
        let Some(id) = parent else { return false };
        let Some(ancestor) = lookup(id) else { return false };
        if matches!(ancestor.item.kind, ItemKind::Frame { .. })
            && !ancestor.bounds.intersects(bounds)
        {
            return true;
        }
        parent = ancestor.parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use vellum_doc::{Board, NewItem, Placement, StyledText};

    fn frame(title: &str, x: f64, y: f64, w: f64, h: f64) -> NewItem {
        NewItem::new(
            ItemKind::Frame {
                title: StyledText::plain(title),
                order: None,
                speaker_notes: None,
            },
            Placement::new(x, y, w, h),
        )
    }

    fn sticky(text: &str, x: f64, y: f64) -> NewItem {
        NewItem::new(
            ItemKind::Sticky { text: StyledText::plain(text), background: None },
            Placement::new(x, y, 199.0, 228.0),
        )
    }

    /// An item wholly outside its frame is clipped away; one inside it, or one with
    /// no frame at all, is not.
    #[test]
    fn a_frame_clips_only_what_has_left_it() {
        let mut board = Board::new();
        let frame = board.add(frame("Engine bay", 0.0, 0.0, 1000.0, 800.0)).unwrap();
        let inside = board.add(sticky("in", 100.0, 100.0).with_parent(frame)).unwrap();
        let escaped = board.add(sticky("out", 9_000.0, 9_000.0).with_parent(frame)).unwrap();
        let loose = board.add(sticky("loose", 9_000.0, 9_000.0)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let clipped = |doc_id| {
            let id = projection.scene_id(doc_id).unwrap();
            clipped_by_frame(projection.get(id).unwrap(), &projection)
        };

        assert!(!clipped(inside));
        assert!(clipped(escaped), "an item outside its frame was not clipped");
        assert!(!clipped(loose), "an unparented item was clipped by someone's frame");
        assert!(!clipped(frame));
    }

    /// A group is not a frame and does not clip; conflating the two would hide
    /// anything dragged out of a group.
    #[test]
    fn a_group_does_not_clip_its_members() {
        let mut board = Board::new();
        let group = board
            .add(NewItem::new(ItemKind::Group, Placement::new(0.0, 0.0, 100.0, 100.0)))
            .unwrap();
        let far = board.add(sticky("far", 9_000.0, 9_000.0).with_parent(group)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let id = projection.scene_id(far).unwrap();
        assert!(!clipped_by_frame(projection.get(id).unwrap(), &projection));
    }

    /// The chain is *walked*, not glanced at: frame → group → item is Miro's own nesting.
    ///
    /// This is the case a one-step check passes and cannot see. The item's immediate parent
    /// is a group, which never clips, so a version that looked only at `projected.parent`
    /// would answer "not clipped" for an item that has plainly left the frame it is on — and
    /// both existing tests above would stay green, because in each of them the frame is the
    /// immediate parent. Asserted in both directions so a build that clipped *everything*
    /// nested cannot pass either.
    #[test]
    fn a_frame_clips_through_a_group_between_it_and_the_item() {
        let mut board = Board::new();
        let frame = board.add(frame("Interior", 0.0, 0.0, 1000.0, 800.0)).unwrap();
        let group = board
            .add(
                NewItem::new(ItemKind::Group, Placement::new(0.0, 0.0, 400.0, 400.0))
                    .with_parent(frame),
            )
            .unwrap();
        let escaped = board.add(sticky("out", 9_000.0, 9_000.0).with_parent(group)).unwrap();
        let stayed = board.add(sticky("in", 100.0, 100.0).with_parent(group)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let clipped = |doc_id| {
            let id = projection.scene_id(doc_id).unwrap();
            clipped_by_frame(projection.get(id).unwrap(), &projection)
        };

        assert!(
            clipped(escaped),
            "a grandchild outside the frame was drawn: the walk stopped at the group"
        );
        assert!(!clipped(stayed), "a grandchild inside the frame was clipped away");
    }

    /// Ancestry from a `HashMap`, for the states a real `Projection` cannot hold.
    ///
    /// A Loro tree cannot describe a parent cycle, every parent it names exists, and
    /// `Projection`'s item map is private to [`crate::project`] — so a cycle, a dangling
    /// parent and an over-long chain are all unreachable from here through the document.
    /// They are exactly what the bound in [`clipped_by`] exists for, and an untested bound is
    /// a bound nobody has run.
    ///
    /// The `Projected`s are **cloned out of a real projection**, so every field but `parent`
    /// is the genuine article; only the edges are rewritten. Returns the scene ids in the
    /// order the items were given, because a `HashMap`'s iteration order is not one.
    fn rewired(
        edges: &[(usize, Option<usize>)],
        items: &[NewItem],
    ) -> (HashMap<SceneId, Projected>, Vec<SceneId>) {
        let mut board = Board::new();
        let doc: Vec<_> = items.iter().map(|item| board.add(item.clone()).unwrap()).collect();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let scene: Vec<SceneId> =
            doc.iter().map(|id| projection.scene_id(*id).unwrap()).collect();
        let mut nodes = HashMap::new();
        for (index, parent) in edges {
            let mut node = projection.get(scene[*index]).unwrap().clone();
            node.parent = parent.map(|p| scene[p]);
            nodes.insert(scene[*index], node);
        }
        (nodes, scene)
    }

    /// A parent cycle terminates rather than hanging the painter.
    ///
    /// Two assertions, deliberately, so this can fail in a way a test runner reports rather
    /// than only by running out of patience: a cycle of two ordinary items answers `false`
    /// after exhausting the bound, and a cycle that passes *through* a non-intersecting frame
    /// answers `true` at that frame. A test that only checked the first would pass on a build
    /// whose loop exits immediately and clips nothing at all.
    #[test]
    fn a_parent_cycle_terminates_instead_of_hanging_the_painter() {
        // Two stickies, each the other's parent. Neither is a frame, so the walk runs the
        // bound out and gives up.
        let (nodes, ids) =
            rewired(&[(0, Some(1)), (1, Some(0))], &[sticky("a", 0.0, 0.0), sticky("b", 500.0, 0.0)]);
        let bounds = nodes[&ids[0]].bounds;
        assert!(
            !clipped_by(nodes[&ids[0]].parent, &bounds, |id| nodes.get(&id)),
            "a cycle of two stickies reported a clip that no frame asked for"
        );

        // The same cycle with a frame in it, nowhere near the item. The frame is met on the
        // first hop, so the answer arrives long before the bound.
        let (nodes, ids) = rewired(
            &[(0, Some(1)), (1, Some(0))],
            &[frame("elsewhere", 0.0, 0.0, 100.0, 100.0), sticky("b", 9_000.0, 9_000.0)],
        );
        let (frame_id, sticky_id) = (ids[0], ids[1]);
        let bounds = nodes[&sticky_id].bounds;
        assert!(
            clipped_by(Some(frame_id), &bounds, |id| nodes.get(&id)),
            "a frame inside a cycle stopped clipping"
        );
    }

    /// A parent naming an item the projection does not hold is not a clip.
    ///
    /// Reachable in principle between an edit and a reprojection, and drawing the item is the
    /// honest answer: refusing to draw something because its parent could not be found hides
    /// content on the strength of a bookkeeping gap, which is the failure this very behaviour
    /// was mistaken for once already.
    #[test]
    fn a_parent_that_is_not_in_the_projection_does_not_clip() {
        let (nodes, ids) = rewired(&[(0, None)], &[sticky("a", 9_000.0, 9_000.0)]);
        let bounds = nodes[&ids[0]].bounds;
        // Nothing has interned this id: `Projection` counts up from zero and holds one item.
        let absent = ids[0] + 12_345;
        assert!(!clipped_by(Some(absent), &bounds, |id| nodes.get(&id)));
    }

    /// The bound is reached rather than merely declared, and it is not short by one.
    ///
    /// A chain longer than [`MAX_NESTING`] gives up and the item draws. The second half is
    /// what keeps that honest: the same chain with a non-intersecting frame on the **last hop
    /// the bound allows** must still clip. Without it a build whose loop ran a single
    /// iteration — or none — would satisfy the first assertion while having stopped clipping
    /// anything nested at all.
    #[test]
    fn the_walk_gives_up_past_its_bound_and_not_before_it() {
        let bound = u64::try_from(MAX_NESTING).expect("the bound fits a scene id");
        let (seed, ids) = rewired(&[(0, None)], &[sticky("seed", 9_000.0, 9_000.0)]);
        let template = seed[&ids[0]].clone();
        let far = template.bounds;

        // Link n's parent is link n + 1, for twice the bound. Keys are this map's own, not a
        // projection's, so they can be chosen.
        let mut nodes: HashMap<SceneId, Projected> = HashMap::new();
        for index in 0..bound * 2 {
            let mut node = template.clone();
            node.parent = Some(index + 1);
            nodes.insert(index, node);
        }
        assert!(
            !clipped_by(Some(0), &far, |id| nodes.get(&id)),
            "a chain longer than the bound answered instead of giving up"
        );

        // Starting at key 0, the k-th iteration looks up key k - 1 — so `bound - 1` is the
        // last key the walk can reach. A frame there, far from the item, must still clip.
        let (framed, ids) = rewired(&[(0, None)], &[frame("elsewhere", 0.0, 0.0, 100.0, 100.0)]);
        let mut frame_node = framed[&ids[0]].clone();
        frame_node.parent = None;
        nodes.insert(bound - 1, frame_node);
        assert!(
            clipped_by(Some(0), &far, |id| nodes.get(&id)),
            "a frame on the last hop the bound allows was never reached"
        );
    }
}
