//! The ways a structural edit can be refused, and the invariants a tree is checked
//! against.
//!
//! Two separate types, because they mean different things. [`MindMapError`] is a
//! *refusal*: the caller asked for something that is not a tree operation, the map
//! is untouched, and the app should say so. [`Invariant`] is a *bug*: the map is
//! already inconsistent, which can only happen if this crate has a defect, so it is
//! reported by [`MindMap::validate`](crate::MindMap::validate) and asserted in tests
//! rather than being something callers handle.
//!
//! The distinction matters most for reparenting. Dragging a node onto one of its own
//! descendants is the single easiest way to turn a tree into a ring, and a ring in a
//! mind map is not a wrong picture — it is an infinite loop in every traversal in
//! this crate. So the check happens **before any mutation**, and the operation is
//! all-or-nothing: a rejected reparent leaves a byte-identical map.

use thiserror::Error;

use crate::tree::NodeId;

/// A structural edit that was refused. In every case the map is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MindMapError {
    /// The id names no live node — it was never in this map, or it has been removed
    /// and the caller is holding a stale handle.
    ///
    /// Distinguishable from a valid id because [`NodeId`] carries a generation; see
    /// [`crate::tree`] for why a bare index would silently succeed here instead,
    /// editing whichever node happened to reuse the slot.
    #[error("no live node {0:?}")]
    NoSuchNode(NodeId),

    /// The root cannot be removed. A mind map with no root is not a mind map, and
    /// every layout, traversal and bounds query in this crate starts from one.
    #[error("the root {0:?} cannot be removed")]
    CannotRemoveRoot(NodeId),

    /// The root cannot be reparented — it has no parent to move it away from.
    /// Re-rooting a map is a different operation with different semantics and is not
    /// offered here.
    #[error("the root {0:?} cannot be reparented")]
    CannotReparentRoot(NodeId),

    /// The requested new parent is the node itself, or somewhere inside its own
    /// subtree. Accepting it would detach that subtree from the root and leave a
    /// cycle behind — see the module docs.
    #[error("{node:?} cannot be reparented under {new_parent:?}, which is inside its own subtree")]
    WouldCycle { node: NodeId, new_parent: NodeId },

    /// A child index past the end of the list it would be inserted into.
    ///
    /// Reported rather than clamped: an out-of-range index in a drag-to-reorder
    /// means the caller's model of the child list disagrees with this one, and
    /// quietly appending would hide that.
    #[error("child index {index} is out of range for {parent:?}, which has {len} children")]
    IndexOutOfRange { parent: NodeId, index: usize, len: usize },
}

/// A structural invariant that does not hold. Only ever produced by
/// [`MindMap::validate`](crate::MindMap::validate).
///
/// Every variant is a bug in this crate, not a caller error. They exist so that the
/// tests covering rejected edits can assert something far stronger than "an error
/// came back" — namely that the map is still a well-formed tree afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Invariant {
    #[error("the root {0:?} has a parent")]
    RootHasParent(NodeId),
    #[error("{node:?} names parent {parent:?}, which is not live")]
    DanglingParent { node: NodeId, parent: NodeId },
    #[error("{parent:?} lists child {child:?}, which is not live")]
    DanglingChild { parent: NodeId, child: NodeId },
    #[error("{child:?} names parent {parent:?} but is not among its children")]
    MissingBacklink { child: NodeId, parent: NodeId },
    #[error("{child:?} is listed under {parent:?} but names {actual:?} as its parent")]
    WrongBacklink { parent: NodeId, child: NodeId, actual: Option<NodeId> },
    #[error("{child:?} appears more than once among the children of {parent:?}")]
    DuplicateChild { parent: NodeId, child: NodeId },
    /// The decisive one: a cycle, or a fragment orphaned by a partial edit, shows up
    /// as live nodes the root cannot reach.
    #[error("{unreachable} live node(s) are not reachable from the root")]
    Unreachable { unreachable: usize },
}
