//! The flattened, *visible* view of a map that every layout runs on.
//!
//! Collapse is implemented entirely here, and nowhere else. A collapsed node's
//! children never enter these arrays, so the tidy pass, the level extents, the
//! bounds and the hit index all handle folding without a single conditional: they
//! are simply working on a smaller tree. That is why collapsing reflows correctly by
//! construction rather than by a separate "recompute the affected region" path,
//! which is where a hand-rolled layout would put the overlap bug.
//!
//! Nodes come out in pre-order, so index `0` is always the branch root — the
//! invariant [`crate::tidy`] relies on — and a node's children are contiguous in
//! neither position nor id, only in the [`Visible::children`] lists.

use crate::geometry::Size;
use crate::tidy::TidyTree;
use crate::tree::{MindMap, NodeId};

pub(crate) struct Visible {
    /// Compact index → the map's own handle.
    pub ids: Vec<NodeId>,
    /// `None` only for index `0`.
    pub parent: Vec<Option<u32>>,
    pub children: Vec<Vec<u32>>,
    /// Depth measured from index `0`, not from the map's root — the two differ for
    /// the per-branch passes the balanced layout uses to weigh its two sides.
    pub depth: Vec<u32>,
    /// Sanitised on the way in, so no later stage has to defend against a `NaN`
    /// arriving from a text measurement.
    pub size: Vec<Size>,
}

impl Visible {
    /// Every visible node of the map.
    pub fn whole(map: &MindMap) -> Self {
        let root = map.root();
        Self::branches(map, root, map.visible_children(root))
    }

    /// One branch as a tree in its own right, with `id` as index `0`.
    pub fn rooted_at(map: &MindMap, id: NodeId) -> Self {
        Self::branches(map, id, map.visible_children(id))
    }

    /// `root` plus only the listed children and their subtrees.
    ///
    /// The subset is what makes the balanced layout two ordinary tree layouts rather
    /// than a second algorithm: each side is a whole tree that happens to share a
    /// root with the other.
    pub fn branches(map: &MindMap, root: NodeId, branches: &[NodeId]) -> Self {
        let capacity = map.node_count();
        let mut visible = Self {
            ids: Vec::with_capacity(capacity),
            parent: Vec::with_capacity(capacity),
            children: Vec::with_capacity(capacity),
            depth: Vec::with_capacity(capacity),
            size: Vec::with_capacity(capacity),
        };
        visible.push(map, root, None, 0);

        // Depth-first, children pushed in reverse so they are popped in the user's
        // order — which is also the order they are appended to their parent's list,
        // and therefore the order the tidy pass reads them in.
        let mut stack: Vec<(NodeId, u32, u32)> =
            branches.iter().rev().map(|&child| (child, 0, 1)).collect();
        while let Some((id, parent, depth)) = stack.pop() {
            let index = visible.push(map, id, Some(parent), depth);
            for &child in map.visible_children(id).iter().rev() {
                stack.push((child, index, depth + 1));
            }
        }
        visible
    }

    fn push(&mut self, map: &MindMap, id: NodeId, parent: Option<u32>, depth: u32) -> u32 {
        let index = self.ids.len() as u32;
        self.ids.push(id);
        self.parent.push(parent);
        self.children.push(Vec::new());
        self.depth.push(depth);
        self.size.push(map.get(id).map_or(Size::ZERO, |n| n.size.sanitised()));
        if let Some(parent) = parent {
            self.children[parent as usize].push(index);
        }
        index
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// One more than the deepest depth, i.e. the number of levels. Never zero: a
    /// [`Visible`] always holds at least its own root.
    pub fn depth_count(&self) -> usize {
        self.depth.iter().max().map_or(1, |d| *d as usize + 1)
    }

    /// The largest value of `measure` at each depth.
    ///
    /// Levels are spaced by this rather than by a constant so that one very wide
    /// node does not overlap the next column, and so that a map of uniformly narrow
    /// nodes does not sit in a grid of empty space sized for the worst case
    /// anywhere in the tree.
    pub fn max_per_depth(&self, measure: impl Fn(Size) -> f64) -> Vec<f64> {
        let mut out = vec![0.0f64; self.depth_count()];
        for (i, &depth) in self.depth.iter().enumerate() {
            let slot = &mut out[depth as usize];
            *slot = slot.max(measure(self.size[i]));
        }
        out
    }

    /// Borrow the shape, own the extents. See [`TidyTree`].
    pub fn tidy_tree(&self, breadth: Vec<f64>) -> TidyTree<'_> {
        debug_assert_eq!(breadth.len(), self.len());
        TidyTree {
            parent: &self.parent,
            children: &self.children,
            depth: &self.depth,
            breadth,
        }
    }

    /// The parent handle of a compact index, in the map's own terms.
    pub fn parent_id(&self, index: usize) -> Option<NodeId> {
        self.parent[index].map(|p| self.ids[p as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Node;

    fn sample() -> (MindMap, Vec<NodeId>) {
        let mut map = MindMap::new("root");
        let root = map.root();
        let a = map.add_child(root, Node::new("a")).unwrap();
        let a1 = map.add_child(a, Node::new("a1")).unwrap();
        let b = map.add_child(root, Node::new("b")).unwrap();
        (map, vec![root, a, a1, b])
    }

    #[test]
    fn the_flattened_order_is_pre_order_with_the_branch_root_first() {
        let (map, ids) = sample();
        let v = Visible::whole(&map);
        assert_eq!(v.ids, ids);
        assert_eq!(v.parent, vec![None, Some(0), Some(1), Some(0)]);
        assert_eq!(v.children, vec![vec![1, 3], vec![2], vec![], vec![]]);
        assert_eq!(v.depth, vec![0, 1, 2, 1]);
        assert_eq!(v.depth_count(), 3);
    }

    #[test]
    fn a_collapsed_subtree_never_enters_the_arrays() {
        let (mut map, ids) = sample();
        map.set_collapsed(ids[1], true).unwrap();
        let v = Visible::whole(&map);
        assert_eq!(v.ids, vec![ids[0], ids[1], ids[3]], "the fold handle itself stays");
        assert_eq!(v.children[1], Vec::<u32>::new());
        assert_eq!(v.depth_count(), 2);
    }

    #[test]
    fn a_branch_becomes_a_tree_in_its_own_right() {
        let (map, ids) = sample();
        let v = Visible::rooted_at(&map, ids[1]);
        assert_eq!(v.ids, vec![ids[1], ids[2]]);
        assert_eq!(v.depth, vec![0, 1], "depth is measured from the branch, not the map");
    }

    #[test]
    fn a_subset_of_branches_keeps_the_root_and_drops_the_rest() {
        let (map, ids) = sample();
        let v = Visible::branches(&map, ids[0], &ids[3..4]);
        assert_eq!(v.ids, vec![ids[0], ids[3]]);
        assert_eq!(v.parent_id(1), Some(ids[0]));
    }

    #[test]
    fn level_extents_take_the_widest_node_on_each_level() {
        let (mut map, ids) = sample();
        map.get_mut(ids[3]).unwrap().size = Size::new(400.0, 10.0);
        let v = Visible::whole(&map);
        let widths = v.max_per_depth(|s| s.width);
        assert_eq!(widths[0], Size::default().width);
        assert_eq!(widths[1], 400.0, "the widest node on the level sets the column");
    }
}
