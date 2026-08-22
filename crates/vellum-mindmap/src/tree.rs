//! The mind map itself: a rooted tree of nodes, and the four edits that can change
//! its shape — add, remove, reparent, collapse.
//!
//! # Why a generational arena rather than `Rc<RefCell<Node>>`
//!
//! Nodes are stored in a flat `Vec` and referred to by [`NodeId`]. Three things fall
//! out of that which a pointer graph does not give:
//!
//! 1. **Layout is a pass over contiguous memory.** The tidy algorithm in
//!    [`crate::tidy`] touches every node several times per pass and chases sibling
//!    and contour links constantly. Indices into a `Vec` keep that in cache; a graph
//!    of reference-counted cells does not.
//! 2. **Ids survive being handed out.** The app holds selection, the undo stack
//!    holds edits, and a connector holds two endpoints — all outliving any borrow of
//!    the map.
//! 3. **A stale id is caught, not silently honoured.** This is the reason for the
//!    generation counter. Removing a node frees its slot for reuse; a bare index
//!    handed out before the removal would then address whatever node landed there,
//!    so "delete a node, then undo something that still names it" would edit an
//!    unrelated node rather than failing. The generation makes that
//!    [`MindMapError::NoSuchNode`] instead.
//!
//! # Ordering
//!
//! A node's children are an ordered list, and that order is the layout order — first
//! child is topmost in a horizontal tree, first clockwise in a radial one. It is
//! the user's order, so no operation here ever reorders a list as a side effect.
//!
//! # Deserialisation is not trusted
//!
//! `serde` can produce a `MindMap` whose backlinks disagree or whose parent pointers
//! form a ring, because it bypasses every constructor here. Anything reading a map
//! from a file should run [`MindMap::validate`] before laying it out; the traversals
//! below are defensive enough not to hang on a corrupt map, but they will not
//! produce a sensible picture from one.

use serde::{Deserialize, Serialize};

use crate::error::{Invariant, MindMapError};
use crate::geometry::Size;
use crate::style::NodeStyle;

/// A handle to a node, stable across every edit that does not remove it.
///
/// The generation is what makes a stale handle detectable; see the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

impl NodeId {
    /// The slot this id addresses. Exposed because the app's own side tables —
    /// selection sets, animation state — want a dense key, and re-hashing a
    /// two-field struct for those is wasteful. Not a valid handle on its own.
    pub const fn slot(self) -> u32 {
        self.index
    }
}

impl std::fmt::Debug for NodeId {
    /// `#12v3` rather than `NodeId { index: 12, generation: 3 }`. Every error in
    /// this crate names at least one node and several name three; the derived form
    /// makes those messages unreadable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}v{}", self.index, self.generation)
    }
}

/// One node: its label, its appearance, its measured box and whether its children
/// are folded away.
///
/// Parent and child links are private and are only ever changed through [`MindMap`],
/// because every one of them has a matching backlink and the two must move together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// The label. Plain text here; rich spans live in `vellum-doc`, which owns the
    /// styled-span model that `docs/01-architecture.md` §4 requires from day one.
    /// A mind map node's label is a single short phrase, so this crate can lay out
    /// from a measured [`Size`] without knowing how the text is marked up.
    pub text: String,
    pub style: NodeStyle,
    /// The measured extent of the node's box. Written by the caller after
    /// `vellum-text` shapes the label; see [`Size`].
    pub size: Size,
    /// When true, this node's subtree is hidden: excluded from layout, from bounds
    /// and from hit-testing, but **not** removed. The subtree keeps its shape and
    /// its ids, so expanding restores exactly the previous picture.
    pub collapsed: bool,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
}

impl Node {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: NodeStyle::default(),
            size: Size::default(),
            collapsed: false,
            parent: None,
            children: Vec::new(),
        }
    }

    pub fn with_size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    pub fn with_style(mut self, style: NodeStyle) -> Self {
        self.style = style;
        self
    }

    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Slot {
    generation: u32,
    node: Option<Node>,
}

/// A rooted tree of [`Node`]s. Always has a root; never empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MindMap {
    slots: Vec<Slot>,
    free: Vec<u32>,
    root: NodeId,
    live: usize,
}

impl MindMap {
    /// A new map holding only a root with the given label.
    pub fn new(root_text: impl Into<String>) -> Self {
        Self::with_root(Node::new(root_text))
    }

    /// A new map whose root is the given node. Any parent or child links the node
    /// carries are dropped — a root has neither.
    pub fn with_root(mut root: Node) -> Self {
        root.parent = None;
        root.children.clear();
        Self {
            slots: vec![Slot { generation: 0, node: Some(root) }],
            free: Vec::new(),
            root: NodeId { index: 0, generation: 0 },
            live: 1,
        }
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    /// The number of live nodes, collapsed ones included.
    pub fn node_count(&self) -> usize {
        self.live
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.get(id).is_some()
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation { return None }
        slot.node.as_ref()
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation { return None }
        slot.node.as_mut()
    }

    /// [`Self::get`] as a `Result`, for the many call sites whose next step is to
    /// return [`MindMapError::NoSuchNode`] anyway.
    pub fn node(&self, id: NodeId) -> Result<&Node, MindMapError> {
        self.get(id).ok_or(MindMapError::NoSuchNode(id))
    }

    pub fn node_mut(&mut self, id: NodeId) -> Result<&mut Node, MindMapError> {
        self.get_mut(id).ok_or(MindMapError::NoSuchNode(id))
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.get(id).and_then(Node::parent)
    }

    /// A node's children in order. An unknown id has none, which lets traversals
    /// stay total instead of threading a `Result` through every step.
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.get(id).map_or(&[], Node::children)
    }

    /// The children layout will actually place: none, if this node is collapsed.
    ///
    /// The distinction between this and [`Self::children`] is the entire
    /// implementation of collapse. Layout, bounds and hit-testing all walk with
    /// this; edits and validation all walk with [`Self::children`].
    pub fn visible_children(&self, id: NodeId) -> &[NodeId] {
        match self.get(id) {
            Some(node) if !node.collapsed => &node.children,
            _ => &[],
        }
    }

    pub fn is_collapsed(&self, id: NodeId) -> bool {
        self.get(id).is_some_and(|n| n.collapsed)
    }

    /// Depth from the root: the root is `0`.
    pub fn depth(&self, id: NodeId) -> Result<usize, MindMapError> {
        if !self.contains(id) { return Err(MindMapError::NoSuchNode(id)) }
        Ok(self.ancestors(id).count())
    }

    /// This node's ancestors, nearest first, ending at the root.
    ///
    /// Bounded by the live node count so that a map corrupted by a bad
    /// deserialisation produces a short wrong answer rather than hanging the UI
    /// thread. A well-formed tree can never reach the bound.
    pub fn ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors { map: self, next: self.parent(id), remaining: self.live }
    }

    /// True when `ancestor` is strictly above `descendant`. A node is not its own
    /// ancestor, which is what makes the reparent check below read as written:
    /// a move is a cycle if the target *is* the node or is under it.
    pub fn is_ancestor_of(&self, ancestor: NodeId, descendant: NodeId) -> bool {
        self.ancestors(descendant).any(|a| a == ancestor)
    }

    /// True when this node is laid out — it exists and no ancestor is collapsed.
    pub fn is_visible(&self, id: NodeId) -> bool {
        self.contains(id) && !self.ancestors(id).any(|a| self.is_collapsed(a))
    }

    /// The node and everything under it, in pre-order. Collapsed subtrees are
    /// included: collapse hides, it does not remove.
    pub fn subtree(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        if !self.contains(id) { return out }
        let mut stack = vec![id];
        while let Some(v) = stack.pop() {
            out.push(v);
            // Pushed in reverse so the first child is popped first and `out` comes
            // back in the order the user sees.
            stack.extend(self.children(v).iter().rev().copied());
        }
        out
    }

    /// Number of nodes strictly below this one.
    pub fn descendant_count(&self, id: NodeId) -> usize {
        self.subtree(id).len().saturating_sub(1)
    }

    /// Append a child. Returns the new node's id.
    pub fn add_child(&mut self, parent: NodeId, node: Node) -> Result<NodeId, MindMapError> {
        let len = self.node(parent)?.children.len();
        self.insert_child(parent, len, node)
    }

    /// Insert a child at `index` among `parent`'s existing children.
    ///
    /// `index == children.len()` appends; anything beyond that is
    /// [`MindMapError::IndexOutOfRange`] rather than a silent clamp, because an
    /// index the caller did not expect means their model of the child list has
    /// diverged from this one.
    pub fn insert_child(
        &mut self,
        parent: NodeId,
        index: usize,
        mut node: Node,
    ) -> Result<NodeId, MindMapError> {
        let len = self.node(parent)?.children.len();
        if index > len {
            return Err(MindMapError::IndexOutOfRange { parent, index, len });
        }
        node.parent = Some(parent);
        node.children.clear();
        let id = self.allocate(node);
        self.slot_node_mut(parent).children.insert(index, id);
        Ok(id)
    }

    /// Remove a node and everything under it. Returns the removed ids in pre-order.
    ///
    /// This is what the Delete key does to a selected mind-map node in Miro, and it
    /// is the destructive one: the subtree is gone, and only undo brings it back.
    /// [`Self::remove_node_promoting_children`] is the non-destructive alternative.
    pub fn remove_subtree(&mut self, id: NodeId) -> Result<Vec<NodeId>, MindMapError> {
        if !self.contains(id) { return Err(MindMapError::NoSuchNode(id)) }
        if id == self.root { return Err(MindMapError::CannotRemoveRoot(id)) }
        let removed = self.subtree(id);
        self.detach(id);
        for &victim in &removed {
            self.free_slot(victim);
        }
        Ok(removed)
    }

    /// Remove one node, splicing its children into its parent's list at the position
    /// the node itself occupied.
    ///
    /// Order matters here and is the reason for the splice: deleting a middle branch
    /// of an outline should close the gap, not move that branch's contents to the
    /// bottom of the list.
    pub fn remove_node_promoting_children(&mut self, id: NodeId) -> Result<(), MindMapError> {
        if !self.contains(id) { return Err(MindMapError::NoSuchNode(id)) }
        if id == self.root { return Err(MindMapError::CannotRemoveRoot(id)) }
        let parent = self.parent(id).expect("a non-root node has a parent");
        let children = self.children(id).to_vec();
        let at = self.child_index(parent, id).expect("a child is listed by its parent");

        for &child in &children {
            self.slot_node_mut(child).parent = Some(parent);
        }
        let siblings = &mut self.slot_node_mut(parent).children;
        siblings.remove(at);
        siblings.splice(at..at, children);
        self.slot_node_mut(id).children.clear();
        self.free_slot(id);
        Ok(())
    }

    /// Move a node, with its whole subtree, under a new parent.
    ///
    /// `index` is a position in `new_parent`'s child list **as it will be after the
    /// node has been detached**, so re-ordering within one parent needs no special
    /// case at the call site: moving child 0 to `Some(2)` of the same parent puts it
    /// third among the remaining three, which is what dragging it there looks like.
    /// `None` appends.
    ///
    /// # Rejections
    ///
    /// Dragging a node onto its own descendant is the one move that would turn the
    /// tree into a ring, and a ring makes every traversal in this crate
    /// non-terminating. It is rejected as [`MindMapError::WouldCycle`], **before any
    /// mutation happens**, so a refused reparent leaves a byte-identical map — see
    /// [`crate::error`]. The root cannot be reparented at all.
    pub fn reparent(
        &mut self,
        node: NodeId,
        new_parent: NodeId,
        index: Option<usize>,
    ) -> Result<(), MindMapError> {
        // Everything is checked before anything is written. The order of these
        // checks is itself part of the contract: a stale id is reported as a stale
        // id even when it is also the root or also a cycle.
        if !self.contains(node) { return Err(MindMapError::NoSuchNode(node)) }
        if !self.contains(new_parent) { return Err(MindMapError::NoSuchNode(new_parent)) }
        if node == self.root { return Err(MindMapError::CannotReparentRoot(node)) }
        if node == new_parent || self.is_ancestor_of(node, new_parent) {
            return Err(MindMapError::WouldCycle { node, new_parent });
        }

        let old_parent = self.parent(node).expect("a non-root node has a parent");
        // The list `index` addresses is the one left after detaching, so a move
        // inside a single parent is measured against a list one shorter.
        let len_after =
            self.children(new_parent).len() - usize::from(old_parent == new_parent);
        let at = match index {
            None => len_after,
            Some(i) if i <= len_after => i,
            Some(i) => {
                return Err(MindMapError::IndexOutOfRange {
                    parent: new_parent,
                    index: i,
                    len: len_after,
                });
            }
        };

        self.detach(node);
        self.slot_node_mut(new_parent).children.insert(at, node);
        self.slot_node_mut(node).parent = Some(new_parent);
        Ok(())
    }

    pub fn set_collapsed(&mut self, id: NodeId, collapsed: bool) -> Result<(), MindMapError> {
        self.node_mut(id)?.collapsed = collapsed;
        Ok(())
    }

    /// Flip the fold state, returning the new one. The one-call form of what a
    /// click on a node's fold handle does.
    pub fn toggle_collapsed(&mut self, id: NodeId) -> Result<bool, MindMapError> {
        let node = self.node_mut(id)?;
        node.collapsed = !node.collapsed;
        Ok(node.collapsed)
    }

    /// Every node laid out, in the order layout walks them: pre-order, skipping the
    /// subtrees of collapsed nodes but keeping the collapsed nodes themselves.
    pub fn visible_nodes(&self) -> Vec<NodeId> {
        let mut out = Vec::with_capacity(self.live);
        let mut stack = vec![self.root];
        // ⚠ **A visited set, because this walk reads a tree that came off a disk.**
        //
        // A `MindMap` is `Deserialize`, and `children` round-trips — so a token whose root
        // lists *itself* as a child parses cleanly, and this loop then pushes the root for
        // ever while `out` grows without limit. `MindMap::validate` exists and is called from
        // no production site; `visible_children` filters only on `collapsed`, so nothing
        // upstream refuses it.
        //
        // The consequence is not a wrong drawing, it is the frame never returning: on the
        // desktop a hang with memory climbing, and in a browser tab the same with no console
        // anybody can open. A structurally valid but self-referential document is exactly the
        // shape `WidgetLayer::readable`'s "a token we do not understand degrades rather than
        // errors" contract does *not* catch, because serde was perfectly happy with it.
        //
        // Bounded by the node count rather than trusted: a cycle simply stops, and what has
        // been collected so far is drawn. That is the degradation this layer promises
        // everywhere else.
        let mut seen = std::collections::HashSet::with_capacity(self.live);
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            out.push(v);
            stack.extend(self.visible_children(v).iter().rev().copied());
        }
        out
    }

    /// The position of `child` in `parent`'s child list.
    pub fn child_index(&self, parent: NodeId, child: NodeId) -> Option<usize> {
        self.children(parent).iter().position(|&c| c == child)
    }

    /// Check every structural invariant. `Ok(())` for any map built through the
    /// operations above; anything else is a bug in this crate or a hand-edited file.
    ///
    /// Cheap enough (`O(n)`) to run after every edit in a test, and that is exactly
    /// what the reparent tests do — "the operation was refused" is a much weaker
    /// claim than "the operation was refused and the tree is still a tree".
    pub fn validate(&self) -> Result<(), Invariant> {
        if self.get(self.root).is_none_or(|r| r.parent.is_some()) {
            return Err(Invariant::RootHasParent(self.root));
        }
        let mut counted = 0usize;
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(node) = slot.node.as_ref() else { continue };
            counted += 1;
            let id = NodeId { index: index as u32, generation: slot.generation };

            if let Some(parent) = node.parent {
                if !self.contains(parent) {
                    return Err(Invariant::DanglingParent { node: id, parent });
                }
                if self.child_index(parent, id).is_none() {
                    return Err(Invariant::MissingBacklink { child: id, parent });
                }
            }
            for (i, &child) in node.children.iter().enumerate() {
                let Some(child_node) = self.get(child) else {
                    return Err(Invariant::DanglingChild { parent: id, child });
                };
                if child_node.parent != Some(id) {
                    return Err(Invariant::WrongBacklink {
                        parent: id,
                        child,
                        actual: child_node.parent,
                    });
                }
                if node.children[..i].contains(&child) {
                    return Err(Invariant::DuplicateChild { parent: id, child });
                }
            }
        }
        if counted != self.live {
            // `live` is a cached count; if it has drifted, every capacity hint and
            // traversal bound derived from it is wrong too.
            return Err(Invariant::Unreachable { unreachable: counted.abs_diff(self.live) });
        }
        // A cycle shows up here and nowhere above: every node in a ring has a valid
        // parent, a valid backlink and no duplicates. It is simply unreachable.
        let reachable = self.subtree(self.root).len();
        if reachable != self.live {
            return Err(Invariant::Unreachable { unreachable: self.live - reachable });
        }
        Ok(())
    }

    // --- internals -------------------------------------------------------------

    fn allocate(&mut self, node: Node) -> NodeId {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.node = Some(node);
            return NodeId { index, generation: slot.generation };
        }
        let index = self.slots.len() as u32;
        self.slots.push(Slot { generation: 0, node: Some(node) });
        NodeId { index, generation: 0 }
    }

    /// Empty a slot and offer it for reuse.
    ///
    /// The generation is bumped so that ids handed out before the removal stop
    /// resolving. A slot whose generation has saturated is **retired rather than
    /// reused**: at `u32::MAX` the next bump could not distinguish the new occupant
    /// from an id 4.3 billion removals old, and leaking one `Vec` entry is a better
    /// trade than a stale id that silently resolves. Reaching it would take a
    /// lifetime of editing one node.
    fn free_slot(&mut self, id: NodeId) {
        let Some(slot) = self.slots.get_mut(id.index as usize) else { return };
        if slot.generation != id.generation || slot.node.is_none() { return }
        slot.node = None;
        self.live -= 1;
        if slot.generation == u32::MAX { return }
        slot.generation += 1;
        self.free.push(id.index);
    }

    /// Unlink a node from its parent, leaving the node and its own subtree intact.
    fn detach(&mut self, id: NodeId) {
        let Some(parent) = self.parent(id) else { return };
        if let Some(at) = self.child_index(parent, id) {
            self.slot_node_mut(parent).children.remove(at);
        }
        self.slot_node_mut(id).parent = None;
    }

    /// Mutable access to a node that is known to be live. Panicking here is correct:
    /// every caller has already resolved the id, so a `None` would mean this crate
    /// has lost track of its own arena, which is not a condition to paper over.
    fn slot_node_mut(&mut self, id: NodeId) -> &mut Node {
        self.get_mut(id).expect("internal: node was resolved before being mutated")
    }
}

/// Iterator over a node's ancestors, nearest first. See [`MindMap::ancestors`].
pub struct Ancestors<'a> {
    map: &'a MindMap,
    next: Option<NodeId>,
    remaining: usize,
}

impl Iterator for Ancestors<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.remaining == 0 { return None }
        self.remaining -= 1;
        let current = self.next?;
        self.next = self.map.parent(current);
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `root → a{a1, a2}, b` — small enough to reason about, deep enough that a
    /// reparent has somewhere wrong to go.
    fn sample() -> (MindMap, [NodeId; 5]) {
        let mut map = MindMap::new("root");
        let root = map.root();
        let a = map.add_child(root, Node::new("a")).unwrap();
        let a1 = map.add_child(a, Node::new("a1")).unwrap();
        let a2 = map.add_child(a, Node::new("a2")).unwrap();
        let b = map.add_child(root, Node::new("b")).unwrap();
        (map, [root, a, a1, a2, b])
    }


    /// ⚠ **The module header already promised this, and `visible_nodes` did not keep it.**
    ///
    /// *"the traversals below are defensive enough not to hang on a corrupt map"* — true of
    /// the others and false of that one, which walked a stack with no visited set. `children`
    /// round-trips through serde, so a map whose root lists **itself** as a child parses
    /// cleanly, `validate` is called from no production site, and `visible_children` filters
    /// only on `collapsed`. The loop then pushed the root for ever with `out` growing without
    /// limit: on the desktop a hang with memory climbing, and in a browser tab the same with
    /// no console anybody can open.
    ///
    /// The test builds the cycle the way the bug arrives — through `serde`, bypassing every
    /// constructor — rather than by reaching into private fields, because that is the only
    /// route by which it can happen.
    #[test]
    fn a_map_whose_root_is_its_own_child_does_not_hang_the_walk() {
        let map = MindMap::new("root");
        let mut json: serde_json::Value = serde_json::to_value(&map).unwrap();
        let root = serde_json::to_value(map.root()).unwrap();
        json["slots"][0]["node"]["children"] = serde_json::Value::Array(vec![root]);
        let cycle: MindMap = serde_json::from_value(json).expect("a cycle deserialises cleanly");

        // Terminates, and hands back what it managed to collect rather than nothing.
        let visible = cycle.visible_nodes();
        assert_eq!(visible, vec![cycle.root()], "the root should be reported exactly once");
    }

    #[test]
    fn a_new_map_is_a_valid_one_node_tree() {
        let map = MindMap::new("root");
        assert_eq!(map.node_count(), 1);
        assert_eq!(map.parent(map.root()), None);
        assert!(map.children(map.root()).is_empty());
        map.validate().unwrap();
    }

    #[test]
    fn children_keep_insertion_order_and_insert_respects_the_index() {
        let (mut map, [root, a, .., b]) = sample();
        let c = map.insert_child(root, 1, Node::new("c")).unwrap();
        assert_eq!(map.children(root), &[a, c, b]);
        map.validate().unwrap();
    }

    #[test]
    fn an_index_past_the_end_is_refused_rather_than_clamped() {
        let (mut map, [root, ..]) = sample();
        let err = map.insert_child(root, 3, Node::new("c")).unwrap_err();
        assert_eq!(err, MindMapError::IndexOutOfRange { parent: root, index: 3, len: 2 });
        assert_eq!(map.node_count(), 5, "a refused insert allocates nothing");
    }

    #[test]
    fn subtree_is_pre_order_and_depth_counts_from_the_root() {
        let (map, [root, a, a1, a2, b]) = sample();
        assert_eq!(map.subtree(root), vec![root, a, a1, a2, b]);
        assert_eq!(map.depth(root).unwrap(), 0);
        assert_eq!(map.depth(a1).unwrap(), 2);
        assert_eq!(map.descendant_count(a), 2);
        assert_eq!(map.ancestors(a1).collect::<Vec<_>>(), vec![a, root]);
    }

    #[test]
    fn removing_a_subtree_takes_its_descendants_with_it() {
        let (mut map, [root, a, a1, a2, b]) = sample();
        let removed = map.remove_subtree(a).unwrap();
        assert_eq!(removed, vec![a, a1, a2]);
        assert_eq!(map.children(root), &[b]);
        assert_eq!(map.node_count(), 2);
        assert!(!map.contains(a1));
        map.validate().unwrap();
    }

    #[test]
    fn promoting_children_splices_them_where_the_node_was() {
        let (mut map, [root, a, a1, a2, b]) = sample();
        map.remove_node_promoting_children(a).unwrap();
        assert_eq!(map.children(root), &[a1, a2, b], "the gap closes in place");
        assert_eq!(map.parent(a1), Some(root));
        assert!(!map.contains(a));
        map.validate().unwrap();
    }

    #[test]
    fn the_root_can_be_neither_removed_nor_reparented() {
        let (mut map, [root, a, ..]) = sample();
        assert_eq!(map.remove_subtree(root), Err(MindMapError::CannotRemoveRoot(root)));
        assert_eq!(
            map.remove_node_promoting_children(root),
            Err(MindMapError::CannotRemoveRoot(root))
        );
        assert_eq!(map.reparent(root, a, None), Err(MindMapError::CannotReparentRoot(root)));
        map.validate().unwrap();
    }

    #[test]
    fn a_stale_id_is_detected_even_after_its_slot_is_reused() {
        let (mut map, [root, a, ..]) = sample();
        map.remove_subtree(a).unwrap();
        // Three allocations reuse the three freed slots, one of which is `a`'s.
        let reused: Vec<_> =
            (0..3).map(|i| map.add_child(root, Node::new(format!("n{i}"))).unwrap()).collect();
        assert!(reused.iter().any(|r| r.slot() == a.slot()), "the slot really was reused");
        assert!(!map.contains(a), "but the old handle no longer resolves");
        assert_eq!(map.node(a), Err(MindMapError::NoSuchNode(a)));
        map.validate().unwrap();
    }

    #[test]
    fn reparenting_moves_the_whole_subtree() {
        let (mut map, [root, a, a1, a2, b]) = sample();
        map.reparent(a, b, None).unwrap();
        assert_eq!(map.children(root), &[b]);
        assert_eq!(map.children(b), &[a]);
        assert_eq!(map.children(a), &[a1, a2], "the subtree came along");
        assert_eq!(map.depth(a1).unwrap(), 3);
        map.validate().unwrap();
    }

    #[test]
    fn reordering_within_one_parent_indexes_the_list_after_detaching() {
        let mut map = MindMap::new("root");
        let root = map.root();
        let ids: Vec<_> = ["0", "1", "2", "3"]
            .iter()
            .map(|t| map.add_child(root, Node::new(*t)).unwrap())
            .collect();

        // Dragging the first child to the third slot: the list it lands in is the
        // three-long one left after it is lifted out.
        map.reparent(ids[0], root, Some(2)).unwrap();
        assert_eq!(map.children(root), &[ids[1], ids[2], ids[0], ids[3]]);

        // The end of that shorter list is a legal index; one past it is not.
        map.reparent(ids[0], root, Some(3)).unwrap();
        assert_eq!(map.children(root), &[ids[1], ids[2], ids[3], ids[0]]);
        assert_eq!(
            map.reparent(ids[0], root, Some(4)),
            Err(MindMapError::IndexOutOfRange { parent: root, index: 4, len: 3 })
        );
        map.validate().unwrap();
    }

    #[test]
    fn reparenting_into_a_descendant_is_refused_and_changes_nothing() {
        let (mut map, [_, a, a1, ..]) = sample();
        let before = map.clone();

        assert_eq!(
            map.reparent(a, a1, None),
            Err(MindMapError::WouldCycle { node: a, new_parent: a1 })
        );
        assert_eq!(
            map.reparent(a, a, None),
            Err(MindMapError::WouldCycle { node: a, new_parent: a })
        );

        assert_eq!(map, before, "a refused reparent leaves the map byte-identical");
        map.validate().unwrap();
    }

    #[test]
    fn a_reparent_naming_a_removed_node_is_refused_before_anything_moves() {
        let (mut map, [root, a, a1, _, b]) = sample();
        map.remove_subtree(a).unwrap();
        let before = map.clone();
        assert_eq!(map.reparent(b, a1, None), Err(MindMapError::NoSuchNode(a1)));
        assert_eq!(map.reparent(a, root, None), Err(MindMapError::NoSuchNode(a)));
        assert_eq!(map, before);
    }

    #[test]
    fn collapse_hides_children_without_removing_them() {
        let (mut map, [root, a, a1, a2, b]) = sample();
        assert!(map.toggle_collapsed(a).unwrap());

        assert_eq!(map.visible_children(a), &[] as &[NodeId]);
        assert_eq!(map.children(a), &[a1, a2], "still there, just folded");
        assert_eq!(map.visible_nodes(), vec![root, a, b]);
        assert!(!map.is_visible(a1));
        assert!(map.is_visible(a), "the collapsed node itself is still drawn");
        assert_eq!(map.node_count(), 5);

        map.set_collapsed(a, false).unwrap();
        assert_eq!(map.visible_nodes(), vec![root, a, a1, a2, b]);
    }

    #[test]
    fn a_map_round_trips_through_serde() {
        let (map, _) = sample();
        let json = serde_json::to_string(&map).unwrap();
        let back: MindMap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, map);
        back.validate().unwrap();
    }

    #[test]
    fn validate_catches_a_ring_that_serde_could_produce() {
        // Hand-built to look exactly like a file whose parent pointers were edited:
        // every backlink agrees, so only reachability from the root can catch it.
        let (mut map, [root, a, a1, ..]) = sample();
        map.slot_node_mut(root).children.retain(|&c| c != a);
        map.slot_node_mut(a).parent = Some(a1);
        map.slot_node_mut(a1).children.push(a);
        assert_eq!(map.validate(), Err(Invariant::Unreachable { unreachable: 3 }));
    }
}
