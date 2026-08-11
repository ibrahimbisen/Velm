//! Vellum's mind maps: a tree of nodes, and the automatic layout that makes it one.
//!
//! `docs/features/README.md` §2 lists mind maps as "node tree + automatic
//! radial/tree layout, collapse/expand", and the emphasis belongs on *automatic*. A
//! mind map you have to position by hand is a pile of stickies with lines between
//! them — Vellum already has those. What a mind map tool owes the user is that they
//! type, and the picture rearranges itself correctly, every time, instantly.
//!
//! So the centre of this crate is [`tidy`]: Buchheim, Jünger and Leipert's
//! linear-time refinement of Walker's algorithm, which gives the
//! Reingold–Tilford properties — siblings never overlap, subtrees stay as compact as
//! that allows, identical subtrees are drawn identically, and a parent is centred
//! over its children. The naive alternative, stacking siblings at
//! `index × spacing`, looks correct for about six nodes and then collides.
//!
//! Pure logic: no GPU, no document, no window. That is what lets the properties
//! above be *tested* rather than eyeballed — every overlap claim in this crate is an
//! assertion over a real tree, on a machine with no graphics stack.
//!
//! ```
//! use vellum_mindmap::{
//!     Axis, ConnectorOptions, LayoutKind, LayoutOptions, MindMap, Node, Point, Size,
//! };
//!
//! let mut map = MindMap::new("Engine bay");
//! let root = map.root();
//! let cooling = map.add_child(root, Node::new("Cooling")).unwrap();
//! let radiator = map.add_child(cooling, Node::new("Radiator")).unwrap();
//! let intake = map.add_child(root, Node::new("Intake")).unwrap();
//!
//! // Root in the middle, branches balanced either side of it.
//! let options = LayoutOptions {
//!     kind: LayoutKind::Balanced { axis: Axis::Horizontal },
//!     ..LayoutOptions::default()
//! };
//! let layout = map.layout(&options);
//!
//! // Every visible node has a world rect, and the map knows its own bounds.
//! assert_eq!(layout.len(), 4);
//! assert!(layout.bounds().unwrap().contains(layout.rect(radiator).unwrap().centre()));
//!
//! // Hit-testing needs no spatial index: the boxes cannot overlap.
//! assert_eq!(layout.hit_test(layout.rect(intake).unwrap().centre()), Some(intake));
//!
//! // Links come out as point lists for `vellum-connect` to tessellate.
//! assert_eq!(layout.connectors(&ConnectorOptions::default()).len(), 3);
//!
//! // Folding a branch reflows the rest; the folded nodes keep their ids.
//! map.set_collapsed(cooling, true).unwrap();
//! let folded = map.layout_stable(&options, Some(&layout));
//! assert_eq!(folded.len(), 3);
//! assert!(folded.get(radiator).is_none());
//! assert!(map.contains(radiator));
//! ```
//!
//! # Layout
//!
//! - [`mod@tree`] — [`MindMap`], [`Node`], and the four edits: add, remove,
//!   reparent, collapse. Ids are generational, so a handle to a removed node is
//!   *detected* rather than silently resolving to whatever reused its slot.
//! - [`layout`] — the three layout forms, the [`Layout`] they produce, its bounds
//!   and hit index, and the stabilisation that keeps a user's place across an edit.
//! - [`tidy`] — the algorithm, in one axis, with no knowledge of mind maps.
//! - [`connector`] — parent → child paths as point lists.
//! - [`style`], [`geometry`], [`error`] — the supporting vocabulary.
//!
//! # What this crate does not do
//!
//! - **It does not measure text.** [`Node::size`] is written by the caller after
//!   `vellum-text` shapes the label. Layout is a geometry problem given sizes, and
//!   pulling a font stack into it would make every test here need one.
//! - **It does not own the document.** A mind map on a real board lives in
//!   `vellum-doc`'s Loro tree, where undo, history and the item hierarchy already
//!   are. This crate is the shape of one, and the arithmetic that arranges it.
//! - **It does not tessellate.** Connector paths come out as points, for
//!   `vellum-connect` to stroke.
//!
//! # Honest limits
//!
//! - The radial form treats every node as a disc of its own diagonal, because a
//!   box's occupancy of a ring depends on the angle it ends up at, which is not
//!   known until the layout has run. The non-overlap guarantee is therefore exact,
//!   and radial maps of wide flat labels are looser than they strictly need to be.
//!   [`LayoutKind::Radial`] states the guarantee and the proof of it.
//! - Stabilisation spends one degree of freedom — where the drawing sits — and
//!   spends it optimally for a rigid translation, which is the only freedom a tidy
//!   layout leaves. It cannot stop a subtree from moving when the tree above it
//!   genuinely changed shape. In the linear forms adding a leaf moves nothing
//!   further than about one row; in the radial form every angle on a ring changes
//!   when that ring's occupancy does, so nothing is ever left *exactly* in place.
//!   [`layout`] measures both and states where each bites.

pub mod connector;
pub mod error;
pub mod geometry;
pub mod layout;
pub mod style;
pub mod tidy;
pub mod tree;

pub use connector::{Anchor, ConnectorOptions, ConnectorPath, ConnectorShape};
pub use error::{Invariant, MindMapError};
pub use geometry::{EPSILON, Point, Rect, Size, Vec2};
pub use layout::{Axis, Direction, Layout, LayoutKind, LayoutOptions, Placement, Side};
pub use style::{Color, NodeStyle};
pub use tree::{Ancestors, MindMap, Node, NodeId};
