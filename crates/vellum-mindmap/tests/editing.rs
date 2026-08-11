//! Structural edits: what happens to the tree, and what happens to the picture.
//!
//! Three claims are worth an integration test rather than a unit test, because each
//! spans the tree, the layout and the reflow:
//!
//! 1. **Collapsing a deep subtree reflows without overlapping.** Collapse is not a
//!    render-time filter; it changes what the layout is laid out *on*, and the rest
//!    of the map has to close up around the gap correctly.
//! 2. **Reparenting a node into its own descendant is refused cleanly.** Cleanly
//!    means more than "an error came back": the map must be byte-identical
//!    afterwards and must still pass every invariant, because a half-applied
//!    reparent is a ring, and a ring makes every traversal in this crate
//!    non-terminating.
//! 3. **An edit does not lose the user's place.** Stated carefully, because the
//!    naive version of this claim is false and it is worth knowing exactly how: a
//!    one-node edit moves nothing further than the row it added, the leftover global
//!    offset is spent optimally, and in the radial form nothing is ever left exactly
//!    in place at all. Each of those is a separate test below.

use std::collections::HashMap;

use vellum_mindmap::{
    Axis, Direction, Layout, LayoutKind, LayoutOptions, MindMap, MindMapError, Node, NodeId,
    Point, Rect, Size,
};

fn all_kinds() -> Vec<LayoutKind> {
    vec![
        LayoutKind::Tree { direction: Direction::Right },
        LayoutKind::Balanced { axis: Axis::Horizontal },
        LayoutKind::Radial { start_angle: 0.0, sweep: std::f64::consts::TAU },
    ]
}

/// A regular tree: `branching` children per node, `depth` levels below the root.
fn regular(branching: usize, depth: usize) -> (MindMap, Vec<Vec<NodeId>>) {
    let mut map = MindMap::with_root(Node::new("root").with_size(Size::new(160.0, 44.0)));
    let mut levels = vec![vec![map.root()]];
    for level in 0..depth {
        let mut next = Vec::new();
        for &parent in &levels[level] {
            for i in 0..branching {
                let size = Size::new(100.0 + (i as f64) * 20.0, 32.0);
                next.push(map.add_child(parent, Node::new("n").with_size(size)).unwrap());
            }
        }
        levels.push(next);
    }
    (map, levels)
}

fn assert_no_overlap(layout: &Layout, what: &str) {
    let mut boxes: Vec<(Rect, NodeId)> =
        layout.placements().iter().map(|p| (p.rect, p.node)).collect();
    boxes.sort_by(|a, b| a.0.min.y.total_cmp(&b.0.min.y));
    let mut active: Vec<(Rect, NodeId)> = Vec::new();
    for &(rect, node) in &boxes {
        active.retain(|(other, _)| other.max.y > rect.min.y);
        for &(other, other_node) in &active {
            assert!(!rect.intersects(other), "{what}: {node:?} overlaps {other_node:?}");
        }
        active.push((rect, node));
    }
}

fn centres(layout: &Layout) -> HashMap<NodeId, Point> {
    layout.placements().iter().map(|p| (p.node, p.rect.centre())).collect()
}

// --- collapse ---------------------------------------------------------------

#[test]
fn collapsing_a_deep_subtree_hides_it_and_the_rest_closes_up() {
    // 4 branches × 4 × 4 × 3 = 341 nodes, deep enough that a collapse at depth 2
    // removes a genuinely large region rather than a leaf or two.
    let (mut map, levels) = regular(4, 3);
    let victim = levels[2][5];
    let hidden = map.descendant_count(victim);
    assert!(hidden >= 4, "the subtree being folded should be worth folding");

    for kind in all_kinds() {
        let options = LayoutOptions { kind, ..LayoutOptions::default() };
        let open = map.layout(&options);
        assert_no_overlap(&open, &format!("{kind:?} open"));

        map.set_collapsed(victim, true).unwrap();
        let folded = map.layout(&options);

        // The folded node is still drawn — it is the handle you click to unfold —
        // and everything under it is gone from the layout but not from the map.
        assert!(folded.get(victim).is_some());
        assert_eq!(folded.len(), open.len() - hidden, "{kind:?}");
        for &descendant in &map.subtree(victim)[1..] {
            assert!(folded.get(descendant).is_none(), "{kind:?} still placed {descendant:?}");
            assert!(map.contains(descendant), "collapse must not remove anything");
        }

        assert_no_overlap(&folded, &format!("{kind:?} folded"));

        // Unfolding restores the previous picture exactly. Layout is a pure function
        // of the tree, so this is the strongest available statement that collapse
        // left no residue.
        map.set_collapsed(victim, false).unwrap();
        assert_eq!(map.layout(&options), open, "{kind:?} did not restore");
    }
    map.validate().unwrap();
}

#[test]
fn collapsing_the_root_leaves_one_node() {
    let (mut map, _) = regular(3, 3);
    map.set_collapsed(map.root(), true).unwrap();
    for kind in all_kinds() {
        let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.placements()[0].node, map.root());
        assert_eq!(layout.bounds().unwrap(), layout.rect(map.root()).unwrap());
        assert!(layout.connectors(&Default::default()).is_empty());
    }
    assert!(map.node_count() > 1, "the rest of the map is folded, not deleted");
}

#[test]
fn folding_a_whole_level_shrinks_the_map_along_the_breadth_axis() {
    let (mut map, levels) = regular(4, 3);
    let options = LayoutOptions {
        kind: LayoutKind::Tree { direction: Direction::Right },
        ..LayoutOptions::default()
    };
    let open = map.layout(&options).bounds().unwrap();

    for &id in &levels[2] {
        map.set_collapsed(id, true).unwrap();
    }
    let folded = map.layout(&options).bounds().unwrap();

    assert!(folded.height() < open.height(), "{folded:?} vs {open:?}");
    assert!(folded.width() < open.width(), "one whole level of columns went away");
}

#[test]
fn nested_collapses_are_independent() {
    let (mut map, levels) = regular(3, 3);
    let outer = levels[1][0];
    let inner = map.children(outer)[1];

    map.set_collapsed(inner, true).unwrap();
    map.set_collapsed(outer, true).unwrap();
    let options = LayoutOptions::default();
    let both = map.layout(&options);
    assert!(both.get(inner).is_none(), "the inner node is inside the outer fold");

    // Unfolding the outer one must not unfold the inner one: fold state is per node
    // and is remembered, which is the behaviour that makes folding usable as an
    // outline tool rather than a toggle.
    map.set_collapsed(outer, false).unwrap();
    let outer_only = map.layout(&options);
    assert!(outer_only.get(inner).is_some());
    assert!(map.children(inner).iter().all(|&c| outer_only.get(c).is_none()));
}

// --- reparenting ------------------------------------------------------------

#[test]
fn reparenting_a_node_into_its_own_descendant_is_refused_without_corrupting_the_tree() {
    let (mut map, levels) = regular(3, 3);
    let node = levels[1][1];
    let snapshot = serde_json::to_string(&map).unwrap();

    // Every descendant, at every depth, plus the node itself.
    let mut refused = 0;
    for &target in &map.subtree(node) {
        assert_eq!(
            map.reparent(node, target, None),
            Err(MindMapError::WouldCycle { node, new_parent: target }),
            "{target:?} is inside {node:?}'s own subtree"
        );
        refused += 1;

        // After each refusal: byte-identical, still a tree, still layable out.
        assert_eq!(serde_json::to_string(&map).unwrap(), snapshot);
        map.validate().unwrap();
    }
    assert!(refused > 10, "only tried {refused} targets");

    // The tree is still usable afterwards — the point of "cleanly".
    for kind in all_kinds() {
        let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
        assert_eq!(layout.len(), map.node_count());
        assert_no_overlap(&layout, &format!("{kind:?} after refusals"));
    }
}

#[test]
fn every_other_way_a_reparent_can_be_refused_also_leaves_the_map_alone() {
    let (mut map, levels) = regular(3, 2);
    let root = map.root();
    let node = levels[1][0];
    let doomed = levels[1][2];
    map.remove_subtree(doomed).unwrap();
    let snapshot = serde_json::to_string(&map).unwrap();

    let cases = [
        (map.reparent(root, node, None), MindMapError::CannotReparentRoot(root)),
        (map.reparent(doomed, root, None), MindMapError::NoSuchNode(doomed)),
        (map.reparent(node, doomed, None), MindMapError::NoSuchNode(doomed)),
        (
            map.reparent(node, root, Some(99)),
            MindMapError::IndexOutOfRange { parent: root, index: 99, len: 1 },
        ),
    ];
    for (result, expected) in cases {
        assert_eq!(result, Err(expected));
    }
    assert_eq!(serde_json::to_string(&map).unwrap(), snapshot);
    map.validate().unwrap();
}

#[test]
fn a_legal_reparent_moves_the_subtree_and_the_layout_follows() {
    let (mut map, levels) = regular(3, 3);
    let moving = levels[1][0];
    let destination = levels[2][8];
    let carried = map.subtree(moving);
    assert!(!map.is_ancestor_of(moving, destination), "the destination is elsewhere");

    map.reparent(moving, destination, None).unwrap();
    map.validate().unwrap();
    assert_eq!(map.parent(moving), Some(destination));
    assert_eq!(map.subtree(moving), carried, "the subtree came along unchanged");

    for kind in all_kinds() {
        let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
        assert_eq!(layout.len(), map.node_count());
        assert_no_overlap(&layout, &format!("{kind:?} after reparent"));
        // The moved subtree now sits beyond its new parent, not its old one.
        assert!(layout.get(moving).unwrap().depth > layout.get(destination).unwrap().depth - 1);
        assert_eq!(layout.get(moving).unwrap().depth, layout.get(destination).unwrap().depth + 1);
    }
}

#[test]
fn removing_a_subtree_leaves_a_layout_of_exactly_what_is_left() {
    let (mut map, levels) = regular(3, 3);
    let victim = levels[1][1];
    let removed = map.remove_subtree(victim).unwrap();
    map.validate().unwrap();

    for kind in all_kinds() {
        let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
        assert_eq!(layout.len(), map.node_count());
        assert!(removed.iter().all(|&id| layout.get(id).is_none()));
        assert_no_overlap(&layout, &format!("{kind:?} after removal"));
    }
}

// --- keeping the user's place -----------------------------------------------
//
// Two separate properties, and the tests keep them separate because only one of
// them is a decision this crate makes.
//
//   * The tidy pass is *local*: a small edit disturbs a small amount of the map.
//     That is a property of the algorithm.
//   * The one degree of freedom left over — where the whole drawing sits — is spent
//     optimally. That is `Layout::stabilise_against`, and it is what the grid sweep
//     below checks.

/// Total absolute displacement of each node that survived an edit.
fn displacements(before: &Layout, after: &Layout) -> Vec<f64> {
    let anchor = centres(before);
    let mut out: Vec<f64> = after
        .placements()
        .iter()
        .filter_map(|p| anchor.get(&p.node).map(|&old| (p.rect.centre() - old).manhattan_length()))
        .collect();
    out.sort_by(f64::total_cmp);
    out
}

#[test]
fn adding_a_leaf_moves_nothing_further_than_one_row() {
    // The locality claim, stated in the units it is true in: inserting a node adds
    // exactly one row to one sibling list, and nothing anywhere in the map may move
    // further than that row is tall — wherever it was inserted. A "row" is the new
    // node's extent along the *breadth* axis, which is its height in a tree that
    // grows sideways and its width in one that grows downwards.
    let size = Size::new(120.0, 32.0);
    let gap = LayoutOptions::default().sibling_gap;

    for (kind, row) in [
        (LayoutKind::Tree { direction: Direction::Right }, size.height + gap),
        (LayoutKind::Tree { direction: Direction::Down }, size.width + gap),
        (LayoutKind::Balanced { axis: Axis::Horizontal }, size.height + gap),
    ] {
        let options = LayoutOptions { kind, ..LayoutOptions::default() };
        for position in [0usize, 7, 15] {
            let (mut map, levels) = regular(4, 3);
            let before = map.layout(&options);
            map.add_child(levels[2][position], Node::new("new").with_size(size)).unwrap();
            let after = map.layout_stable(&options, Some(&before));

            let moved = displacements(&before, &after);
            assert!(
                *moved.last().unwrap() <= row + 1e-9,
                "{kind:?} at {position}: something moved {} px, more than one {row}px row",
                moved.last().unwrap()
            );
            assert_no_overlap(&after, &format!("{kind:?} after an insert at {position}"));
        }
    }
}

#[test]
fn adding_a_leaf_at_the_end_of_a_tree_leaves_almost_all_of_it_untouched() {
    // The best case, and the common one while typing: everything above the new node
    // in the tidy order has nothing to move for, and the stabiliser finds the offset
    // that keeps it that way.
    let (mut map, levels) = regular(4, 3);
    let options = LayoutOptions {
        kind: LayoutKind::Tree { direction: Direction::Right },
        ..LayoutOptions::default()
    };
    let before = map.layout(&options);
    map.add_child(*levels[2].last().unwrap(), Node::new("new").with_size(Size::new(120.0, 32.0)))
        .unwrap();
    let after = map.layout_stable(&options, Some(&before));

    let moved = displacements(&before, &after);
    let unmoved = moved.iter().filter(|d| **d == 0.0).count();
    assert!(
        unmoved * 5 >= moved.len() * 4,
        "only {unmoved} of {} nodes stayed exactly put",
        moved.len()
    );
}

#[test]
fn a_symmetric_growth_is_left_alone_rather_than_fudged() {
    // The case the stabiliser cannot help with, asserted so that a future change
    // which "fixes" it by translating anyway is caught. An edit in the middle of a
    // root-pinned tree pushes the branches above it up and the ones below it down;
    // no single offset improves the total, so none is applied.
    let (mut map, levels) = regular(4, 3);
    let options = LayoutOptions::default();
    let before = map.layout(&options);
    map.add_child(levels[2][7], Node::new("new").with_size(Size::new(120.0, 32.0))).unwrap();

    let mut after = map.layout(&options);
    assert_eq!(after.stabilise_against(&before), vellum_mindmap::Vec2::ZERO);
    // ...and it really was optimal, not merely absent.
    assert!(after.displacement_from(&before) <= map.layout(&options).displacement_from(&before));
}

#[test]
fn stabilisation_beats_every_other_rigid_offset_after_each_kind_of_edit() {
    let (mut map, levels) = regular(3, 3);
    let options = LayoutOptions {
        kind: LayoutKind::Balanced { axis: Axis::Horizontal },
        ..LayoutOptions::default()
    };
    let before = map.layout(&options);

    // Three edits of genuinely different shapes, applied one after another.
    map.add_child(levels[2][2], Node::new("added").with_size(Size::new(140.0, 32.0))).unwrap();
    map.remove_subtree(levels[1][2]).unwrap();
    map.reparent(levels[1][0], levels[2][3], None).unwrap();
    map.validate().unwrap();

    let raw = map.layout(&options);
    let stabilised = map.layout_stable(&options, Some(&before));
    assert_no_overlap(&stabilised, "after three edits");
    let best = stabilised.displacement_from(&before);

    for i in -10i32..=10 {
        for j in -10i32..=10 {
            let mut trial = raw.clone();
            trial.translate(vellum_mindmap::Vec2::new(f64::from(i) * 25.0, f64::from(j) * 25.0));
            assert!(
                trial.displacement_from(&before) >= best - 1e-6,
                "offset ({i}, {j}) beat the stabilised layout"
            );
        }
    }
}

#[test]
fn collapsing_the_last_branch_keeps_the_place_of_everything_before_it() {
    let (mut map, levels) = regular(4, 3);
    let options = LayoutOptions {
        kind: LayoutKind::Tree { direction: Direction::Right },
        ..LayoutOptions::default()
    };
    let before = map.layout(&options);
    map.set_collapsed(*levels[1].last().unwrap(), true).unwrap();
    let after = map.layout_stable(&options, Some(&before));

    let moved = displacements(&before, &after);
    let unmoved = moved.iter().filter(|d| **d == 0.0).count();
    assert!(unmoved * 5 >= moved.len() * 4, "only {unmoved} of {} stayed put", moved.len());
    assert_no_overlap(&after, "after collapse");
}

#[test]
fn a_radial_map_reflows_smoothly_even_though_nothing_stays_exactly_put() {
    // The named limit: a ring's angles all depend on how crowded it is, so a leaf
    // added anywhere rotates the ring it lands on. The claim that survives is that
    // the rotation is small for a small edit — not that it is absent.
    let (mut map, levels) = regular(4, 3);
    let options = LayoutOptions {
        kind: LayoutKind::Radial { start_angle: 0.0, sweep: std::f64::consts::TAU },
        ..LayoutOptions::default()
    };
    let before = map.layout(&options);
    let span = before.bounds().unwrap().width().max(before.bounds().unwrap().height());

    map.add_child(levels[2][7], Node::new("new").with_size(Size::new(120.0, 32.0))).unwrap();
    let after = map.layout_stable(&options, Some(&before));

    let moved = displacements(&before, &after);
    assert!(
        *moved.last().unwrap() < 0.05 * span,
        "a one-node edit moved something {} px across a {span} px map",
        moved.last().unwrap()
    );
    assert_no_overlap(&after, "radial after an insert");
}
