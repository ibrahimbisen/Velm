//! The two layouts that lay levels out in straight columns (or rows): the one-sided
//! tree and the balanced two-sided map.
//!
//! Both are the same three steps — flatten the visible tree, ask [`crate::tidy`] for
//! one coordinate per node along the breadth axis, then place the levels along the
//! depth axis — and the balanced form is *literally* the tree form run twice, once
//! per side, sharing a root. Keeping it that way rather than writing a second
//! algorithm is what guarantees a two-sided map has the same non-overlap and
//! compactness properties as a one-sided one; there is no second implementation to
//! get wrong.
//!
//! # Where the levels go
//!
//! Levels are spaced by the **widest node on each level** plus [`LayoutOptions::level_gap`],
//! and nodes are aligned by their *near* edge within their level rather than centred
//! in it. Both choices are for the same reason: a mind map is read as an outline, so
//! the labels on one level should start on one line. Centring them in a column sized
//! by the longest label on that level leaves short labels floating, which reads as a
//! mistake.
//!
//! # How the two sides are chosen
//!
//! Splitting the root's children between the sides is done by **weight, preserving
//! order**: each branch is laid out on its own to find its true breadth extent, and
//! the split point is the one that most nearly halves the total. Order-preserving
//! matters more than perfect balance — a user who typed six branches expects the
//! first three on one side, not an interleaving that optimises a number they cannot
//! see. Weighing by real extent rather than by node count matters because one
//! branch of thirty leaves and one of thirty nodes in a chain are wildly different
//! heights.

use crate::geometry::Rect;
use crate::layout::visible::Visible;
use crate::layout::{Axis, Direction, Layout, LayoutKind, LayoutOptions, Placement, Side};
use crate::tidy::{Gaps, tidy};
use crate::tree::{MindMap, NodeId};

/// Root at one edge, every branch running the same way.
pub(crate) fn tree(map: &MindMap, options: &LayoutOptions, direction: Direction) -> Layout {
    let visible = Visible::whole(map);
    Layout::new(LayoutKind::Tree { direction }, place(&visible, options, direction, Side::Primary))
}

/// Root in the middle, branches split either side of it.
pub(crate) fn balanced(map: &MindMap, options: &LayoutOptions, axis: Axis) -> Layout {
    let (primary, secondary) = axis.directions();
    let root = map.root();
    let branches = map.visible_children(root);

    // One branch cannot be balanced against nothing, and a two-sided map of a single
    // branch is just that branch: fall through to the one-sided form so the result
    // is the one the user would draw by hand.
    if branches.len() < 2 {
        return tree(map, options, primary);
    }

    let weights: Vec<f64> =
        branches.iter().map(|&b| branch_extent(map, options, primary, b)).collect();
    let split = balance_point(&weights);

    let near = Visible::branches(map, root, &branches[..split]);
    let far = Visible::branches(map, root, &branches[split..]);
    let mut placements = place(&near, options, primary, Side::Primary);
    let mut other = place(&far, options, secondary, Side::Secondary);

    // Both passes place the root at `origin` — its breadth coordinate is normalised
    // to zero and its depth coordinate is the centre of level 0, which is its own
    // box — so the second copy is redundant rather than conflicting.
    debug_assert_eq!(other[0].rect.centre(), placements[0].rect.centre());
    other.remove(0);
    placements.append(&mut other);
    Layout::new(LayoutKind::Balanced { axis }, placements)
}

/// Lay one directed tree out and turn it into placements.
fn place(
    visible: &Visible,
    options: &LayoutOptions,
    direction: Direction,
    side: Side,
) -> Vec<Placement> {
    let depth_count = visible.depth_count();
    let breadth = visible.size.iter().map(|&s| direction.breadth(s)).collect();
    let tidy_tree = visible.tidy_tree(breadth);
    let gaps = Gaps::uniform(depth_count, options.sibling_gap, options.subtree_gap);
    let across = tidy(&tidy_tree, &gaps);

    // Level 0 is centred on the origin, so the root's own box straddles it and every
    // other level hangs off the far edge of the one before.
    let level_extent = visible.max_per_depth(|s| direction.depth(s));
    let mut near_edge = vec![0.0; depth_count];
    near_edge[0] = -0.5 * level_extent[0];
    for d in 1..depth_count {
        near_edge[d] = near_edge[d - 1] + level_extent[d - 1] + options.level_gap;
    }

    let root_across = across.first().copied().unwrap_or(0.0);
    (0..visible.len())
        .map(|i| {
            let depth = visible.depth[i];
            let size = visible.size[i];
            let along = near_edge[depth as usize] + 0.5 * direction.depth(size);
            let centre = direction.point(options.origin, along, across[i] - root_across);
            Placement {
                node: visible.ids[i],
                parent: visible.parent_id(i),
                rect: Rect::from_centre(centre, size),
                depth,
                side: if i == 0 { Side::Centre } else { side },
            }
        })
        .collect()
}

/// How much room one branch needs along the breadth axis, laid out on its own.
fn branch_extent(
    map: &MindMap,
    options: &LayoutOptions,
    direction: Direction,
    branch: NodeId,
) -> f64 {
    let visible = Visible::rooted_at(map, branch);
    let breadth: Vec<f64> = visible.size.iter().map(|&s| direction.breadth(s)).collect();
    let tidy_tree = visible.tidy_tree(breadth.clone());
    let gaps =
        Gaps::uniform(visible.depth_count(), options.sibling_gap, options.subtree_gap);
    let across = tidy(&tidy_tree, &gaps);

    let (lo, hi) = across.iter().zip(&breadth).fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(lo, hi), (&centre, &extent)| {
            (lo.min(centre - 0.5 * extent), hi.max(centre + 0.5 * extent))
        },
    );
    if hi >= lo { hi - lo } else { 0.0 }
}

/// The order-preserving split that most nearly halves the total weight. Always
/// leaves at least one branch on each side, so long as there are at least two.
fn balance_point(weights: &[f64]) -> usize {
    let total: f64 = weights.iter().sum();
    let mut running = 0.0;
    let mut best = 1;
    let mut best_gap = f64::INFINITY;
    for k in 1..weights.len() {
        running += weights[k - 1];
        let gap = (running - (total - running)).abs();
        if gap < best_gap {
            best_gap = gap;
            best = k;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Point, Size};
    use crate::tree::Node;

    fn node(w: f64, h: f64) -> Node {
        Node::new("n").with_size(Size::new(w, h))
    }

    fn options(kind: LayoutKind) -> LayoutOptions {
        LayoutOptions { kind, ..LayoutOptions::default() }
    }

    #[test]
    fn the_root_sits_exactly_on_the_origin_in_every_direction() {
        let mut map = MindMap::with_root(node(100.0, 40.0));
        let root = map.root();
        map.add_child(root, node(60.0, 20.0)).unwrap();
        map.add_child(root, node(60.0, 20.0)).unwrap();

        for direction in [Direction::Right, Direction::Left, Direction::Down, Direction::Up] {
            let layout = tree(&map, &options(LayoutKind::Tree { direction }), direction);
            assert_eq!(
                layout.rect(root).unwrap().centre(),
                Point::ORIGIN,
                "{direction:?} moved the root off the origin"
            );
        }
    }

    #[test]
    fn a_right_tree_puts_children_to_the_right_in_order() {
        let mut map = MindMap::with_root(node(100.0, 40.0));
        let root = map.root();
        let a = map.add_child(root, node(80.0, 30.0)).unwrap();
        let b = map.add_child(root, node(80.0, 30.0)).unwrap();
        let layout = tree(&map, &options(LayoutKind::default()), Direction::Right);

        let (ra, rb) = (layout.rect(a).unwrap(), layout.rect(b).unwrap());
        assert!(ra.min.x > layout.rect(root).unwrap().max.x, "children are to the right");
        assert_eq!(ra.min.x, rb.min.x, "one level, one column, near edges aligned");
        assert!(ra.max.y < rb.min.y, "the first child is above the second");
    }

    #[test]
    fn left_and_up_mirror_the_depth_axis_but_not_the_child_order() {
        let mut map = MindMap::with_root(node(100.0, 40.0));
        let root = map.root();
        let a = map.add_child(root, node(80.0, 30.0)).unwrap();
        let b = map.add_child(root, node(80.0, 30.0)).unwrap();

        let left = tree(&map, &options(LayoutKind::default()), Direction::Left);
        assert!(left.rect(a).unwrap().max.x < left.rect(root).unwrap().min.x);
        assert!(
            left.rect(a).unwrap().max.y < left.rect(b).unwrap().min.y,
            "reading order is preserved when the tree is mirrored"
        );

        let up = tree(&map, &options(LayoutKind::default()), Direction::Up);
        assert!(up.rect(a).unwrap().max.y < up.rect(root).unwrap().min.y);
        assert!(up.rect(a).unwrap().max.x < up.rect(b).unwrap().min.x);
    }

    #[test]
    fn levels_are_spaced_by_the_widest_node_on_the_level() {
        let mut map = MindMap::with_root(node(100.0, 40.0));
        let root = map.root();
        let wide = map.add_child(root, node(400.0, 30.0)).unwrap();
        let narrow = map.add_child(root, node(40.0, 30.0)).unwrap();
        let deep = map.add_child(narrow, node(50.0, 30.0)).unwrap();

        let opts = options(LayoutKind::default());
        let layout = tree(&map, &opts, Direction::Right);
        assert_eq!(
            layout.rect(deep).unwrap().min.x - layout.rect(wide).unwrap().max.x,
            opts.level_gap,
            "level 2 clears the widest node on level 1, not the average"
        );
    }

    #[test]
    fn the_balanced_split_halves_the_weight_and_keeps_the_order() {
        // Four branches of 1, 3, 1 and 1 leaves: the split that halves the height is
        // after the second branch, and it does not reorder them.
        let mut map = MindMap::with_root(node(100.0, 40.0));
        let root = map.root();
        let branches: Vec<NodeId> = (0..4).map(|_| map.add_child(root, node(80.0, 30.0)).unwrap()).collect();
        for _ in 0..3 {
            map.add_child(branches[1], node(80.0, 30.0)).unwrap();
        }

        let layout = balanced(&map, &options(LayoutKind::default()), Axis::Horizontal);
        let side = |id: NodeId| layout.get(id).unwrap().side;
        assert_eq!(side(root), Side::Centre);
        assert_eq!(side(branches[0]), Side::Primary);
        assert_eq!(side(branches[1]), Side::Primary);
        assert_eq!(side(branches[2]), Side::Secondary);
        assert_eq!(side(branches[3]), Side::Secondary);

        assert!(layout.rect(branches[0]).unwrap().min.x > 0.0);
        assert!(layout.rect(branches[3]).unwrap().max.x < 0.0);
    }

    #[test]
    fn a_single_branch_falls_through_to_the_one_sided_form() {
        let mut map = MindMap::with_root(node(100.0, 40.0));
        let root = map.root();
        map.add_child(root, node(80.0, 30.0)).unwrap();

        let opts = options(LayoutKind::default());
        let two_sided = balanced(&map, &opts, Axis::Horizontal);
        let one_sided = tree(&map, &opts, Direction::Right);
        assert_eq!(two_sided.placements(), one_sided.placements());
    }

    #[test]
    fn the_root_appears_once_in_a_balanced_layout() {
        let mut map = MindMap::with_root(node(100.0, 40.0));
        let root = map.root();
        for _ in 0..5 {
            map.add_child(root, node(80.0, 30.0)).unwrap();
        }
        let layout = balanced(&map, &options(LayoutKind::default()), Axis::Horizontal);
        assert_eq!(layout.len(), map.node_count());
        assert_eq!(layout.placements().iter().filter(|p| p.node == root).count(), 1);
    }

    #[test]
    fn balance_point_prefers_the_evenest_split() {
        assert_eq!(balance_point(&[1.0, 1.0]), 1);
        assert_eq!(balance_point(&[1.0, 1.0, 1.0, 1.0]), 2);
        assert_eq!(balance_point(&[10.0, 1.0, 1.0, 1.0]), 1);
        assert_eq!(balance_point(&[1.0, 1.0, 1.0, 10.0]), 3);
    }
}
