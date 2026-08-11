//! The tidy-tree core: Buchheim, Jünger and Leipert's linear-time refinement of
//! Walker's algorithm, generalised to variable node extents.
//!
//! This is the whole value of a mind map. A map you position by hand is just shapes,
//! and a map positioned by stacking siblings at `index × spacing` looks tidy for
//! about six nodes and then falls apart: sibling subtrees of different shapes either
//! collide or leave a ragged trench between them. What is wanted is
//! [Reingold–Tilford]'s definition of tidy — subtrees never overlap, identical
//! subtrees are drawn identically wherever they appear, a parent is centred over its
//! children, and the whole drawing is as narrow as those rules allow.
//!
//! Walker's 1990 algorithm gets that but degrades to `O(n²)` on trees whose
//! shape defeats its ancestor bookkeeping. Buchheim et al. (2002) fixed exactly that
//! with two devices, both of which are the parts of the code below that look
//! gratuitous and are not:
//!
//! - **Threads.** Walking the contour of a subtree means visiting its leftmost or
//!   rightmost node at each level. A leaf has no children to descend into, so the
//!   contour would dead-end; a *thread* is a temporary pointer from that leaf to
//!   wherever the contour continues. `Walker::next_left` and
//!   `Walker::next_right` are the only places the distinction shows.
//! - **Deferred shifts.** When subtree *k* has to move right to clear subtree *j*,
//!   the `k − j − 1` subtrees between them must be spread out too. Doing that
//!   immediately is what costs quadratic time. Instead `Walker::move_subtree`
//!   records the shift and its per-subtree share, and `Walker::execute_shifts`
//!   applies both in one right-to-left sweep over the children.
//!
//! # Variable node sizes
//!
//! The published algorithm assumes every node is the same width and separates
//! siblings by a constant. A mind map's nodes are text, so they are not. The
//! generalisation is one line — the constant becomes
//!
//! ```text
//! distance(a, b) = (breadth(a) + breadth(b)) / 2 + gap
//! ```
//!
//! — and it is sound because `Walker::distance` is only ever asked about two nodes
//! **at the same depth**: either two siblings, or two nodes on the facing contours
//! of two subtrees, which the apportion loop advances in lockstep one level at a
//! time. Both cases are asserted in debug builds. That property is also what lets
//! the radial layout hand this module *angular* extents that depend on the ring
//! radius: within one depth the radius is a constant, so the units are consistent.
//!
//! # One axis only
//!
//! This module computes a single coordinate per node — its position along the
//! **breadth** axis, the one siblings spread out along. Where the levels go is a
//! separate and much easier question, answered in [`crate::layout`]. Keeping them
//! apart is what lets one implementation serve a left-to-right tree, a top-down
//! tree, a two-sided map and a radial map without a transpose flag threaded through
//! the recursion.
//!
//! # No recursion
//!
//! Both walks are driven by an explicit stack. The published pseudocode recurses,
//! and on a mind map that is a real hazard: an imported outline can be a chain a few
//! thousand nodes deep, and a stack overflow in layout takes the whole app with it
//! rather than producing a bad picture. The explicit form also makes the interleaving
//! that the algorithm depends on — `firstWalk(child)` then `apportion(child)`, one
//! child at a time — visible in the code instead of implied by the call order.
//!
//! [Reingold–Tilford]: https://reingold.co/tidier-drawings.pdf

use crate::geometry::EPSILON;

/// The tree the walker operates on, flattened: node `0` is the root and every index
/// is a compact position, not a [`NodeId`](crate::NodeId).
///
/// Built by [`crate::layout`] from the *visible* part of a map, which is why
/// collapse needs no special handling anywhere in this module: a collapsed node's
/// children simply never enter the array.
///
/// The skeleton is borrowed and only the extents are owned. The balanced layout runs
/// two passes over one map and the radial layout runs two over the same skeleton
/// with different extents, so cloning a `Vec<Vec<u32>>` per pass would be a thousand
/// small allocations for nothing.
pub struct TidyTree<'a> {
    /// `None` only for the root.
    pub parent: &'a [Option<u32>],
    /// Children in the user's order.
    pub children: &'a [Vec<u32>],
    /// Depth from the root; the root is `0`.
    pub depth: &'a [u32],
    /// Each node's extent along the breadth axis, in whatever unit the caller is
    /// working in — world px for the linear layouts, radians for the radial one.
    pub breadth: Vec<f64>,
}

impl TidyTree<'_> {
    /// Number of nodes. All four arrays are this long; nothing here checks that,
    /// because the only builder is [`crate::layout`] and a mismatch would be a bug
    /// in this crate rather than a caller error.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    /// True for a tree with no nodes at all — not the same as a tree holding only a
    /// root, which has one.
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }
}

/// Separation *in addition to* the two nodes' own extents, indexed by depth.
///
/// Two gaps rather than one, because "these are two children of the same parent" and
/// "these are the facing edges of two different branches" want different spacing —
/// the second wants more, or a map reads as one undifferentiated column of text. The
/// per-depth indexing exists for the radial layout, where a fixed arc length is a
/// different angle on every ring.
pub struct Gaps {
    pub sibling: Vec<f64>,
    pub subtree: Vec<f64>,
}

impl Gaps {
    /// The same two gaps at every depth — what the linear layouts use.
    pub fn uniform(depth_count: usize, sibling: f64, subtree: f64) -> Self {
        Self { sibling: vec![sibling; depth_count], subtree: vec![subtree; depth_count] }
    }
}

/// Lay the tree out along the breadth axis. Returns one coordinate per node: the
/// **centre** of its extent, in the same units as [`TidyTree::breadth`].
///
/// The result is not normalised — the root can land anywhere — because every caller
/// immediately translates the whole thing to put some chosen node at the origin, and
/// normalising here would be one pass that all of them undo.
pub fn tidy(tree: &TidyTree<'_>, gaps: &Gaps) -> Vec<f64> {
    let n = tree.len();
    if n == 0 { return Vec::new() }

    let mut walker = Walker {
        tree,
        gaps,
        prelim: vec![0.0; n],
        modifier: vec![0.0; n],
        shift: vec![0.0; n],
        change: vec![0.0; n],
        thread: vec![None; n],
        ancestor: (0..n as u32).collect(),
        number: numbering(tree),
    };
    walker.first_walk();
    walker.second_walk()
}

/// Each node's index among its siblings. The algorithm needs it twice: to find a
/// node's left sibling in `O(1)`, and — in `Walker::move_subtree` — to count how
/// many subtrees lie between two that must be separated, which is the denominator
/// the deferred shift is shared out over.
fn numbering(tree: &TidyTree<'_>) -> Vec<u32> {
    let mut number = vec![0u32; tree.len()];
    for siblings in tree.children {
        for (i, &child) in siblings.iter().enumerate() {
            number[child as usize] = i as u32;
        }
    }
    number
}

struct Walker<'a> {
    tree: &'a TidyTree<'a>,
    gaps: &'a Gaps,
    /// Provisional breadth coordinate, relative to the node's parent.
    prelim: Vec<f64>,
    /// Accumulated offset applied to this node's whole subtree by the second walk.
    /// Deferring it is what makes moving a subtree `O(1)` instead of `O(size)`.
    modifier: Vec<f64>,
    /// A pending shift for this node's subtree, applied by `execute_shifts`.
    shift: Vec<f64>,
    /// The per-subtree share of pending shifts, accumulated right to left.
    change: Vec<f64>,
    /// Where a subtree's contour continues past a leaf. See the module docs.
    thread: Vec<Option<u32>>,
    /// The ancestor of a contour node within the current parent's children, used to
    /// decide which subtree a shift should be charged to.
    ancestor: Vec<u32>,
    number: Vec<u32>,
}

impl Walker<'_> {
    /// Minimum distance between the centres of two nodes at the same depth.
    fn distance(&self, a: u32, b: u32) -> f64 {
        let (ai, bi) = (a as usize, b as usize);
        debug_assert_eq!(
            self.tree.depth[ai], self.tree.depth[bi],
            "distance is only meaningful between nodes at one depth — see the module docs"
        );
        let level = self.tree.depth[ai] as usize;
        let gap = if self.tree.parent[ai] == self.tree.parent[bi] {
            self.gaps.sibling[level]
        } else {
            self.gaps.subtree[level]
        };
        0.5 * (self.tree.breadth[ai] + self.tree.breadth[bi]) + gap
    }

    fn left_sibling(&self, v: u32) -> Option<u32> {
        let parent = self.tree.parent[v as usize]?;
        let n = self.number[v as usize] as usize;
        (n > 0).then(|| self.tree.children[parent as usize][n - 1])
    }

    /// The first of `v`'s siblings, or `None` when `v` is already first (or is the
    /// root). Only ever called for a `v` that has a left sibling, so the `Some` case
    /// is the one that matters.
    fn leftmost_sibling(&self, v: u32) -> Option<u32> {
        let parent = self.tree.parent[v as usize]?;
        (self.number[v as usize] > 0).then(|| self.tree.children[parent as usize][0])
    }

    /// Next node down the *left* contour: the first child, or the thread standing in
    /// for it when there are no children.
    fn next_left(&self, v: u32) -> Option<u32> {
        match self.tree.children[v as usize].first() {
            Some(&first) => Some(first),
            None => self.thread[v as usize],
        }
    }

    /// Next node down the *right* contour.
    fn next_right(&self, v: u32) -> Option<u32> {
        match self.tree.children[v as usize].last() {
            Some(&last) => Some(last),
            None => self.thread[v as usize],
        }
    }

    /// Post-order pass: give every node a provisional position and push sibling
    /// subtrees apart until they clear each other.
    fn first_walk(&mut self) {
        /// One suspended `firstWalk` call. `descended` records that the child at
        /// `next_child` has just finished and is owed its `apportion` — the
        /// interleaving the algorithm depends on and the one thing an explicit
        /// stack makes easy to get wrong.
        struct Frame {
            node: u32,
            next_child: usize,
            default_ancestor: u32,
            descended: bool,
        }

        let mut stack =
            vec![Frame { node: 0, next_child: 0, default_ancestor: 0, descended: false }];

        while !stack.is_empty() {
            let top = stack.len() - 1;
            let node = stack[top].node;
            let child_count = self.tree.children[node as usize].len();

            if child_count == 0 {
                // A leaf sits one `distance` right of its left sibling, or at zero
                // if it is the first child. Everything else is built from this.
                self.prelim[node as usize] = match self.left_sibling(node) {
                    Some(w) => self.prelim[w as usize] + self.distance(w, node),
                    None => 0.0,
                };
                stack.pop();
                continue;
            }

            if stack[top].descended {
                let child = self.tree.children[node as usize][stack[top].next_child];
                let default_ancestor = stack[top].default_ancestor;
                stack[top].default_ancestor = self.apportion(child, default_ancestor);
                stack[top].next_child += 1;
                stack[top].descended = false;
                continue;
            }

            if stack[top].next_child < child_count {
                let index = stack[top].next_child;
                let child = self.tree.children[node as usize][index];
                if index == 0 {
                    // The paper's `defaultAncestor` starts at the first child and is
                    // only advanced when a thread is laid; it is the subtree a shift
                    // is charged to when the contour node's own ancestor is not a
                    // sibling of the node being placed.
                    stack[top].default_ancestor = child;
                }
                stack[top].descended = true;
                stack.push(Frame {
                    node: child,
                    next_child: 0,
                    default_ancestor: child,
                    descended: false,
                });
                continue;
            }

            // Every child is placed and every collision resolved; pay out the
            // deferred shifts and centre this node over the result.
            self.execute_shifts(node);
            let children = &self.tree.children[node as usize];
            let (first, last) = (children[0] as usize, children[children.len() - 1] as usize);
            let midpoint = 0.5 * (self.prelim[first] + self.prelim[last]);
            match self.left_sibling(node) {
                // Placing this node beside its own left sibling would pull it off
                // the centre of its children, so the difference is banked in
                // `modifier` and applied to the whole subtree by the second walk.
                Some(w) => {
                    self.prelim[node as usize] =
                        self.prelim[w as usize] + self.distance(w, node);
                    self.modifier[node as usize] = self.prelim[node as usize] - midpoint;
                }
                None => self.prelim[node as usize] = midpoint,
            }
            stack.pop();
        }
    }

    /// Separate the subtree rooted at `v` from its left siblings' subtrees by walking
    /// the two facing contours down in lockstep.
    ///
    /// Returns the possibly-updated `default_ancestor`.
    fn apportion(&mut self, v: u32, default_ancestor: u32) -> u32 {
        let Some(left) = self.left_sibling(v) else { return default_ancestor };
        let mut default_ancestor = default_ancestor;

        // Four contour walkers. `inner_*` face each other across the gap being
        // closed; `outer_*` track the far sides, and exist only so that a thread can
        // be laid at the end from whichever contour ran out first.
        let (mut inner_right, mut outer_right) = (v, v);
        let mut inner_left = left;
        let mut outer_left = self.leftmost_sibling(v).unwrap_or(left);

        let mut s_inner_right = self.modifier[inner_right as usize];
        let mut s_outer_right = self.modifier[outer_right as usize];
        let mut s_inner_left = self.modifier[inner_left as usize];
        let mut s_outer_left = self.modifier[outer_left as usize];

        while let (Some(next_inner_left), Some(next_inner_right)) =
            (self.next_right(inner_left), self.next_left(inner_right))
        {
            inner_left = next_inner_left;
            inner_right = next_inner_right;
            // For a well-formed tree the outer contours are never shorter than the
            // inner ones, so these are always `Some`. Breaking rather than
            // unwrapping keeps a malformed input to a slightly wrong picture instead
            // of a panic inside layout.
            let (Some(next_outer_left), Some(next_outer_right)) =
                (self.next_left(outer_left), self.next_right(outer_right))
            else {
                break;
            };
            outer_left = next_outer_left;
            outer_right = next_outer_right;

            self.ancestor[outer_right as usize] = v;

            let overlap = (self.prelim[inner_left as usize] + s_inner_left)
                - (self.prelim[inner_right as usize] + s_inner_right)
                + self.distance(inner_left, inner_right);
            if overlap > 0.0 {
                let charged = self.ancestor_within(inner_left, v, default_ancestor);
                self.move_subtree(charged, v, overlap);
                s_inner_right += overlap;
                s_outer_right += overlap;
            }

            s_inner_left += self.modifier[inner_left as usize];
            s_inner_right += self.modifier[inner_right as usize];
            s_outer_left += self.modifier[outer_left as usize];
            s_outer_right += self.modifier[outer_right as usize];
        }

        // Whichever contour is still going gets threaded onto the one that ran out,
        // so a later sibling sees this pair of subtrees as a single silhouette.
        if self.next_right(inner_left).is_some() && self.next_right(outer_right).is_none() {
            self.thread[outer_right as usize] = self.next_right(inner_left);
            self.modifier[outer_right as usize] += s_inner_left - s_outer_right;
        }
        if self.next_left(inner_right).is_some() && self.next_left(outer_left).is_none() {
            self.thread[outer_left as usize] = self.next_left(inner_right);
            self.modifier[outer_left as usize] += s_inner_right - s_outer_left;
            default_ancestor = v;
        }
        default_ancestor
    }

    /// Which of the current parent's children a shift found on the left contour
    /// should be charged to.
    ///
    /// A contour node reached through a thread belongs to some earlier subtree; if
    /// the ancestor recorded for it is a sibling of `v` then it names that subtree
    /// directly, and otherwise the algorithm falls back on `default_ancestor`. This
    /// two-line test is the whole of Buchheim's fix to Walker's quadratic case.
    fn ancestor_within(&self, contour: u32, v: u32, default_ancestor: u32) -> u32 {
        let candidate = self.ancestor[contour as usize];
        if self.tree.parent[candidate as usize] == self.tree.parent[v as usize] {
            candidate
        } else {
            default_ancestor
        }
    }

    /// Record that the subtree at `right` must move `shift` further along, and that
    /// the subtrees between `left` and `right` must be spread to match.
    ///
    /// Nothing is moved here except `right` itself. The intermediate subtrees are
    /// dealt with in one sweep by [`Self::execute_shifts`], which is what keeps the
    /// whole pass linear.
    fn move_subtree(&mut self, left: u32, right: u32, shift: f64) {
        let spanned = f64::from(self.number[right as usize]) - f64::from(self.number[left as usize]);
        if spanned.abs() < EPSILON {
            // `left` and `right` are the same child, so there is nothing between
            // them to spread and the division below would be undefined.
            return;
        }
        let share = shift / spanned;
        self.change[right as usize] -= share;
        self.shift[right as usize] += shift;
        self.change[left as usize] += share;
        self.prelim[right as usize] += shift;
        self.modifier[right as usize] += shift;
    }

    /// Pay out every deferred shift over one node's children, right to left.
    fn execute_shifts(&mut self, node: u32) {
        let count = self.tree.children[node as usize].len();
        let mut shift = 0.0;
        let mut change = 0.0;
        for i in (0..count).rev() {
            let child = self.tree.children[node as usize][i] as usize;
            self.prelim[child] += shift;
            self.modifier[child] += shift;
            change += self.change[child];
            shift += self.shift[child] + change;
        }
    }

    /// Pre-order pass: turn provisional, parent-relative positions into absolute
    /// ones by carrying each node's accumulated modifier down its subtree.
    fn second_walk(&self) -> Vec<f64> {
        let mut out = vec![0.0; self.tree.len()];
        let mut stack = vec![(0u32, 0.0f64)];
        while let Some((node, carried)) = stack.pop() {
            out[node as usize] = self.prelim[node as usize] + carried;
            let onward = carried + self.modifier[node as usize];
            for &child in &self.tree.children[node as usize] {
                stack.push((child, onward));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree's shape without its extents, owned so that [`TidyTree`] can borrow it.
    /// `parents[i]` is the parent of node `i`; node 0 is the root and must be `None`.
    struct Skeleton {
        parent: Vec<Option<u32>>,
        children: Vec<Vec<u32>>,
        depth: Vec<u32>,
    }

    impl Skeleton {
        fn new(parents: &[Option<u32>]) -> Self {
            let n = parents.len();
            let mut children = vec![Vec::new(); n];
            let mut depth = vec![0u32; n];
            for (i, &p) in parents.iter().enumerate() {
                if let Some(p) = p {
                    children[p as usize].push(i as u32);
                    depth[i] = depth[p as usize] + 1;
                }
            }
            Self { parent: parents.to_vec(), children, depth }
        }

        /// The same extent for every node — the uniform case the published
        /// algorithm assumes.
        fn tree(&self, breadth: f64) -> TidyTree<'_> {
            TidyTree {
                parent: &self.parent,
                children: &self.children,
                depth: &self.depth,
                breadth: vec![breadth; self.parent.len()],
            }
        }

        fn depth_count(&self) -> usize {
            self.depth.iter().max().map_or(1, |d| *d as usize + 1)
        }
    }

    /// No two nodes at the same depth may be closer than their required separation.
    fn assert_no_overlap(tree: &TidyTree<'_>, gaps: &Gaps, out: &[f64]) {
        for a in 0..tree.len() {
            for b in (a + 1)..tree.len() {
                if tree.depth[a] != tree.depth[b] { continue }
                let required = 0.5 * (tree.breadth[a] + tree.breadth[b])
                    + if tree.parent[a] == tree.parent[b] {
                        gaps.sibling[tree.depth[a] as usize]
                    } else {
                        gaps.subtree[tree.depth[a] as usize]
                    };
                let actual = (out[a] - out[b]).abs();
                assert!(
                    actual >= required - 1e-6,
                    "nodes {a} and {b} at depth {} are {actual} apart, need {required}",
                    tree.depth[a]
                );
            }
        }
    }


    #[test]
    fn a_lone_root_sits_at_the_origin() {
        let s = Skeleton::new(&[None]);
        assert_eq!(tidy(&s.tree(10.0), &Gaps::uniform(1, 2.0, 4.0)), vec![0.0]);
    }

    #[test]
    fn siblings_are_separated_by_their_extents_plus_the_gap() {
        //     0
        //   1 2 3
        let s = Skeleton::new(&[None, Some(0), Some(0), Some(0)]);
        let out = tidy(&s.tree(10.0), &Gaps::uniform(2, 2.0, 4.0));
        assert_eq!(out[2] - out[1], 12.0);
        assert_eq!(out[3] - out[2], 12.0);
        assert_eq!(out[0], out[2], "the parent is centred over its children");
    }

    #[test]
    fn a_parent_with_two_children_lands_between_them() {
        let s = Skeleton::new(&[None, Some(0), Some(0)]);
        let out = tidy(&s.tree(10.0), &Gaps::uniform(2, 2.0, 4.0));
        assert!((out[0] - 0.5 * (out[1] + out[2])).abs() < 1e-9);
    }

    #[test]
    fn wide_subtrees_push_their_neighbours_aside_without_overlapping() {
        // 0 ─┬─ 1 ─┬─ 3
        //    │     └─ 4
        //    └─ 2 ─┬─ 5
        //          ├─ 6
        //          └─ 7
        let s = Skeleton::new(&[
            None,
            Some(0),
            Some(0),
            Some(1),
            Some(1),
            Some(2),
            Some(2),
            Some(2),
        ]);
        let t = s.tree(10.0);
        let gaps = Gaps::uniform(s.depth_count(), 2.0, 20.0);
        let out = tidy(&t, &gaps);
        assert_no_overlap(&t, &gaps, &out);
        // The two branches keep the wider `subtree` gap between their facing edges.
        assert!(out[5] - out[4] >= 30.0 - 1e-9);
    }

    #[test]
    fn identical_subtrees_are_drawn_identically_wherever_they_appear() {
        // Reingold–Tilford's second aesthetic rule, and the one a naive
        // "shift everything right until it fits" layout breaks first.
        //   0 ─┬─ 1 ─┬─ 3 ─── 7
        //      │     └─ 4
        //      └─ 2 ─┬─ 5 ─── 8
        //            └─ 6
        let s = Skeleton::new(&[
            None,
            Some(0),
            Some(0),
            Some(1),
            Some(1),
            Some(2),
            Some(2),
            Some(3),
            Some(5),
        ]);
        let out = tidy(&s.tree(10.0), &Gaps::uniform(s.depth_count(), 2.0, 4.0));

        let shape = |root: usize, kids: [usize; 2], grandchild: (usize, usize)| {
            [
                out[kids[0]] - out[root],
                out[kids[1]] - out[root],
                out[grandchild.1] - out[grandchild.0],
            ]
        };
        assert_eq!(shape(1, [3, 4], (3, 7)), shape(2, [5, 6], (5, 8)));
    }

    #[test]
    fn a_deep_thin_subtree_beside_a_bushy_one_stays_compact() {
        // The case Walker's original apportion handles quadratically: a long chain
        // whose contour must be threaded past a wide neighbour.
        let mut parents = vec![None];
        for i in 1..40u32 {
            parents.push(Some(i - 1)); // one 40-deep chain
        }
        let chain_end = parents.len() - 1;
        parents.push(Some(0)); // a second child of the root...
        let bushy = parents.len() as u32 - 1;
        for _ in 0..8 {
            parents.push(Some(bushy)); // ...with eight leaves
        }
        let s = Skeleton::new(&parents);
        let t = s.tree(10.0);
        let gaps = Gaps::uniform(s.depth_count(), 2.0, 4.0);
        let out = tidy(&t, &gaps);
        assert_no_overlap(&t, &gaps, &out);

        // Compactness: the chain and the bushy branch are adjacent, so the whole
        // drawing is only as wide as the bushy branch plus one chain column.
        let (lo, hi) =
            out.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        assert!(hi - lo < 8.0 * 12.0 + 20.0, "layout is {} wide", hi - lo);
        assert!(out[chain_end].is_finite());
    }

    #[test]
    fn variable_extents_separate_by_the_mean_of_the_two() {
        let s = Skeleton::new(&[None, Some(0), Some(0)]);
        let mut t = s.tree(10.0);
        t.breadth[1] = 100.0;
        t.breadth[2] = 20.0;
        let out = tidy(&t, &Gaps::uniform(2, 5.0, 5.0));
        assert!((out[2] - out[1] - (60.0 + 5.0)).abs() < 1e-9);
    }

    #[test]
    fn scaling_every_extent_scales_the_whole_layout() {
        // The property the radial layout relies on to fit a map into a circle in a
        // fixed number of passes instead of iterating: prelim values are sums and
        // means of `distance` terms, so they are linear and homogeneous in the
        // extents, and the `overlap > 0` branch is sign-invariant under a positive
        // scale.
        let s = Skeleton::new(&[None, Some(0), Some(0), Some(1), Some(1), Some(2), Some(2), Some(2)]);
        let a = tidy(&s.tree(10.0), &Gaps::uniform(3, 2.0, 6.0));
        let b = tidy(&s.tree(30.0), &Gaps::uniform(3, 6.0, 18.0));
        for (x, y) in a.iter().zip(&b) {
            assert!((x * 3.0 - y).abs() < 1e-9, "{x} × 3 ≠ {y}");
        }
    }

    #[test]
    fn shrinking_extents_never_widens_the_drawing() {
        // Monotonicity, the second half of the radial fit's argument: relaxing every
        // separation constraint cannot make the result wider.
        let s = Skeleton::new(&[None, Some(0), Some(0), Some(0), Some(1), Some(1), Some(3), Some(6)]);
        let span = |t: &TidyTree<'_>, gaps: &Gaps| {
            let out = tidy(t, gaps);
            let (lo, hi) = out
                .iter()
                .zip(&t.breadth)
                .fold((f64::MAX, f64::MIN), |(lo, hi), (&v, &b)| {
                    (lo.min(v - 0.5 * b), hi.max(v + 0.5 * b))
                });
            hi - lo
        };
        let wide = span(&s.tree(40.0), &Gaps::uniform(3, 8.0, 16.0));
        let narrow = span(&s.tree(9.0), &Gaps::uniform(3, 1.0, 2.0));
        assert!(narrow <= wide + 1e-9, "narrow {narrow} > wide {wide}");
    }

    #[test]
    fn a_deep_chain_does_not_overflow_the_stack() {
        // The reason both walks use an explicit stack. A recursive `firstWalk` on a
        // chain this deep is a crash in a debug build, not a slow layout.
        let parents: Vec<Option<u32>> =
            std::iter::once(None).chain((1..5000u32).map(|i| Some(i - 1))).collect();
        let s = Skeleton::new(&parents);
        let out = tidy(&s.tree(10.0), &Gaps::uniform(s.depth_count(), 2.0, 4.0));
        assert_eq!(out.len(), 5000);
        assert!(out.iter().all(|v| v.abs() < 1e-9), "a chain is a straight line");
    }
}
