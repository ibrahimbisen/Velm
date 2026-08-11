//! Automatic layout: the part of a mind map that makes it a mind map.
//!
//! [`MindMap::layout`] takes a tree and [`LayoutOptions`] and returns a [`Layout`] —
//! a world rect for every visible node, the bounds of the whole map, a hit index,
//! and the connector paths between parents and children. Nothing here mutates the
//! map, so a layout is a *view* of a tree at a moment, cheap to recompute and safe
//! to keep the previous one of, which is what [`Layout::stabilise_against`] needs.
//!
//! # Three forms, one algorithm
//!
//! | [`LayoutKind`] | Shape | Implementation |
//! |---|---|---|
//! | [`Tree`](LayoutKind::Tree) | Root at one edge, branches running one way | one tidy pass |
//! | [`Balanced`](LayoutKind::Balanced) | Root in the middle, branches either side | two tidy passes over one root |
//! | [`Radial`](LayoutKind::Radial) | Root at the centre, levels as rings | the same pass, in polar coordinates |
//!
//! All three go through [`crate::tidy`], which is the point: the non-overlap and
//! compactness properties are proved once and inherited by all three, rather than
//! re-derived per form. The `linear` and `radial` submodules hold the per-form
//! reasoning; both are private, because neither is a second algorithm to choose
//! between — they are two mappings of one.
//!
//! # Keeping the user's place
//!
//! Re-laying out after an edit is the moment a mind map can lose its user. Two
//! separate things keep that from happening, and it is worth being precise about
//! which does what, because only one of them is a choice made here.
//!
//! **The tidy pass is local.** Adding a leaf disturbs only what is genuinely in its
//! way. In the linear forms nothing moves further than about one row — measured, and
//! asserted in `tests/editing.rs` — because the only thing that changed is one
//! sibling list's height and the re-centring of the ancestors above it. This is a
//! property of the algorithm, not of anything in this module.
//!
//! **The global offset is a free choice, and it is spent on the user.**
//! [`Layout::stabilise_against`] translates the new layout by the **componentwise
//! median** of how far each surviving node moved. The median rather than the mean:
//! the median minimises the *sum of absolute* displacements, and an L1 minimum is
//! attained at an actual displacement, so it tends to leave a large block of nodes
//! exactly where they were. A mean would nudge everything a little instead, which is
//! the failure this exists to prevent — the eye tracks one node that did not move
//! far better than a whole map that moved slightly.
//!
//! **Where it cannot help, and does not pretend to.** When a map grows symmetrically
//! about its root — an edit in the middle pushes the branches above it up and the
//! ones below it down — there is no translation that improves matters, and
//! [`Layout::stabilise_against`] correctly returns [`Vec2::ZERO`] rather than picking
//! a compromise that moves everything. In the radial form nothing is *ever* left
//! exactly in place, because changing how crowded a ring is changes every angle on
//! it; displacements stay small for small edits, but they are not zero. Both are
//! consequences of tidiness, not of the stabiliser, and the only way to remove them
//! would be to stop laying the map out tidily.

mod linear;
mod radial;
mod visible;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::connector::{ConnectorOptions, ConnectorPath};
use crate::geometry::{Point, Rect, Size, Vec2};
use crate::tree::{MindMap, NodeId};

/// Which way a one-sided tree grows, and which way each half of a balanced map does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Right,
    Left,
    Down,
    Up,
}

impl Direction {
    /// The extent of a node along the axis siblings spread out on.
    pub(crate) fn breadth(self, size: Size) -> f64 {
        match self {
            Self::Right | Self::Left => size.height,
            Self::Down | Self::Up => size.width,
        }
    }

    /// The extent of a node along the axis levels advance on.
    pub(crate) fn depth(self, size: Size) -> f64 {
        match self {
            Self::Right | Self::Left => size.width,
            Self::Down | Self::Up => size.height,
        }
    }

    /// The outward unit normal of the face a branch leaves through.
    pub fn normal(self) -> Vec2 {
        match self {
            Self::Right => Vec2::new(1.0, 0.0),
            Self::Left => Vec2::new(-1.0, 0.0),
            Self::Down => Vec2::new(0.0, 1.0),
            Self::Up => Vec2::new(0.0, -1.0),
        }
    }

    /// Turn a (depth, breadth) pair into a world point.
    ///
    /// Only the depth axis is mirrored for `Left` and `Up`. Mirroring the breadth
    /// axis too would reverse the child order on screen, so a map flipped to the
    /// left would read bottom-to-top — the user's order is the user's order whatever
    /// way the tree points.
    pub(crate) fn point(self, origin: Point, depth: f64, breadth: f64) -> Point {
        match self {
            Self::Right => Point::new(origin.x + depth, origin.y + breadth),
            Self::Left => Point::new(origin.x - depth, origin.y + breadth),
            Self::Down => Point::new(origin.x + breadth, origin.y + depth),
            Self::Up => Point::new(origin.x + breadth, origin.y - depth),
        }
    }
}

/// Which pair of opposed directions a balanced map splits along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Axis {
    #[default]
    Horizontal,
    Vertical,
}

impl Axis {
    pub(crate) fn directions(self) -> (Direction, Direction) {
        match self {
            Self::Horizontal => (Direction::Right, Direction::Left),
            Self::Vertical => (Direction::Down, Direction::Up),
        }
    }
}

/// The shape of the map.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LayoutKind {
    /// Root at one edge, every branch running the same way.
    Tree { direction: Direction },
    /// Root in the middle, branches split either side by weight.
    Balanced { axis: Axis },
    /// Root at the centre, levels as rings.
    ///
    /// `start_angle` is where the fan begins and `sweep` is how much of the circle it
    /// may use, both in radians; angles run **clockwise** on screen because `+y` is
    /// down. A `sweep` that is not a usable angle is treated as a whole turn rather
    /// than as an error — a map with a nonsense sweep should still draw.
    ///
    /// This is the same tidy pass as the other two, run in polar coordinates: the
    /// breadth axis is angle, the depth axis is radius. Two things are worth knowing
    /// because they are what make it correct rather than approximately correct:
    ///
    /// - **Nodes are separated by chord, not by arc.** A node of diagonal `d` on a
    ///   ring of radius `r` is given angular width `2·asin(d / 2r)` — the angle whose
    ///   chord is exactly `d`. Since `asin` is convex on `[0, 1]`, the sum of two
    ///   half-widths is at least `2·asin((d₁+d₂)/4r)`, whose chord is `(d₁+d₂)/2`:
    ///   exactly the sum of the two nodes' circumscribed radii, so their boxes cannot
    ///   overlap at any angle. Separating by arc instead — the obvious thing — is
    ///   wrong by tens of percent on a sparse inner ring.
    /// - **A node's extent is its diagonal**, because how much of a ring an
    ///   axis-aligned box occupies depends on the angle it ends up at, which is not
    ///   known until the layout has run. Conservative, and deliberately so: the
    ///   alternative is a non-overlap property that depends on a fixed point
    ///   converging. The cost is that radial maps of wide flat labels are looser than
    ///   they strictly need to be.
    Radial { start_angle: f64, sweep: f64 },
}

impl Default for LayoutKind {
    /// A right-growing tree.
    ///
    /// Chosen over the balanced form because its result is determined entirely by
    /// the child order: a user adding their first three nodes sees them stack in the
    /// order they typed them. The balanced form's side assignment depends on subtree
    /// weights the user cannot see, which is the right behaviour for a map that has
    /// grown large and a surprising one for a map with three nodes in it — so it is
    /// something to switch into deliberately.
    fn default() -> Self {
        Self::Tree { direction: Direction::Right }
    }
}

/// Spacing and placement. All distances are world px.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LayoutOptions {
    pub kind: LayoutKind,
    /// Where the root's **centre** goes, before any stabilisation.
    pub origin: Point,
    /// Between two children of the same parent.
    pub sibling_gap: f64,
    /// Between the facing edges of two different branches. Larger than
    /// `sibling_gap`, or the eye cannot tell which node belongs to which parent.
    pub subtree_gap: f64,
    /// Between one level and the next.
    pub level_gap: f64,
}

impl Default for LayoutOptions {
    /// Gaps on the 4px grid of `docs/05-design-language.md` §3, in the ratio a dense
    /// tool wants: siblings close enough to read as a list, branches far enough
    /// apart to read as separate, levels far enough for a connector to be legible.
    fn default() -> Self {
        Self {
            kind: LayoutKind::default(),
            origin: Point::ORIGIN,
            sibling_gap: 12.0,
            subtree_gap: 28.0,
            level_gap: 64.0,
        }
    }
}

/// Which part of the map a node belongs to.
///
/// The renderer needs this and layout is the only thing that knows it: a label on
/// the left half of a balanced map is right-aligned, and its fold handle is on its
/// left. Deriving it downstream from the node's position relative to the root would
/// work for the balanced form and be wrong for both others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// The root. Every kind has exactly one.
    Centre,
    /// The only side of a one-sided tree, the outward side of a radial map, and the
    /// first half of a balanced one.
    Primary,
    /// The second half of a balanced map. Never produced by the other kinds.
    Secondary,
}

/// One node's place in the world.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub node: NodeId,
    /// The node's parent, if it has one — carried here so that a caller drawing
    /// connectors or walking upwards needs the [`Layout`] alone and not the map it
    /// came from.
    pub parent: Option<NodeId>,
    pub rect: Rect,
    pub depth: u32,
    pub side: Side,
}

/// A laid-out map: where every visible node is, and everything derived from that.
///
/// Placements are in visible pre-order for the tree and radial kinds. The balanced
/// kind emits its primary side first and then its secondary, so the order is still
/// deterministic but is not a single pre-order — nothing should depend on it beyond
/// determinism.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    kind: LayoutKind,
    placements: Vec<Placement>,
    index: HashMap<NodeId, u32>,
    /// Placement indices sorted by `rect.min.x`. See [`Layout::hit_test`].
    by_left_edge: Vec<u32>,
    widest: f64,
    bounds: Option<Rect>,
}

impl Layout {
    pub(crate) fn new(kind: LayoutKind, placements: Vec<Placement>) -> Self {
        let index =
            placements.iter().enumerate().map(|(i, p)| (p.node, i as u32)).collect();
        let mut by_left_edge: Vec<u32> = (0..placements.len() as u32).collect();
        by_left_edge.sort_unstable_by(|&a, &b| {
            placements[a as usize].rect.min.x.total_cmp(&placements[b as usize].rect.min.x)
        });
        let widest =
            placements.iter().map(|p| p.rect.width()).fold(0.0f64, f64::max);
        let bounds = Rect::of(placements.iter().map(|p| p.rect));
        Self { kind, placements, index, by_left_edge, widest, bounds }
    }

    /// The form this layout was produced in.
    ///
    /// Kept because a connector's attachment rule depends on it — a link in a
    /// left-to-right tree always leaves its parent's right face, whereas a link in a
    /// radial map leaves wherever the line to the child crosses the box. See
    /// [`crate::connector`].
    pub fn kind(&self) -> LayoutKind {
        self.kind
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub fn len(&self) -> usize {
        self.placements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    pub fn get(&self, node: NodeId) -> Option<&Placement> {
        self.index.get(&node).map(|&i| &self.placements[i as usize])
    }

    pub fn rect(&self, node: NodeId) -> Option<Rect> {
        self.get(node).map(|p| p.rect)
    }

    /// The bounds of every visible node, or `None` for an empty layout.
    ///
    /// **Node boxes only.** Connector paths are normally inside them too — every
    /// elbow corner and Bézier control point is derived from the two endpoints,
    /// which are on the two nodes' borders — with one exception worth stating: both
    /// forms enforce a minimum departure of [`ConnectorOptions::stub`] perpendicular
    /// to the node's edge, so where two nodes are closer than that, a path can bulge
    /// up to `stub` beyond their hull. With the default options that cannot happen,
    /// because `stub` is a quarter of `level_gap` and levels are at least that far
    /// apart; a caller who has shortened `level_gap` and wants exact ink bounds
    /// should union [`ConnectorPath::bounds`] over the links.
    pub fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    /// The node under a point, or `None`.
    ///
    /// Sublinear without a spatial index, which this crate has no business carrying:
    /// placements are kept sorted by left edge, so a hit can only be among those
    /// whose left edge lies in `[x − widest, x]`, and that range is found by binary
    /// search. `widest` is the widest single node, so the range is one node wide plus
    /// however many boxes start within it — a handful, for any map with more than one
    /// column.
    ///
    /// No z-order tie-break, because layout guarantees the boxes do not overlap: at
    /// most one node can contain a point.
    pub fn hit_test(&self, point: Point) -> Option<NodeId> {
        let lower = point.x - self.widest;
        let start = self.by_left_edge.partition_point(|&i| {
            self.placements[i as usize].rect.min.x < lower
        });
        for &i in &self.by_left_edge[start..] {
            let placement = &self.placements[i as usize];
            if placement.rect.min.x > point.x {
                break;
            }
            if placement.rect.contains(point) {
                return Some(placement.node);
            }
        }
        None
    }

    /// The parent → child connector paths, as point lists for `vellum-connect` to
    /// tessellate. See [`crate::connector`].
    pub fn connectors(&self, options: &ConnectorOptions) -> Vec<ConnectorPath> {
        crate::connector::paths(self, options)
    }

    /// Move the whole layout.
    pub fn translate(&mut self, by: Vec2) {
        if by == Vec2::ZERO {
            return;
        }
        for placement in &mut self.placements {
            placement.rect = placement.rect.translated(by);
        }
        self.bounds = self.bounds.map(|b| b.translated(by));
        // `by_left_edge` stays sorted: every left edge moved by the same amount.
    }

    /// Slide this layout so that the nodes it shares with `previous` move as little
    /// as possible, and return the offset applied.
    ///
    /// The offset is the componentwise median of each surviving node's displacement,
    /// which minimises the total L1 movement; see the module docs for why L1. Nodes
    /// that are new in this layout, and nodes that vanished from `previous` because
    /// they were removed or collapsed away, take no part in the vote.
    ///
    /// With nothing in common — a fresh map, or every node replaced — nothing moves
    /// and [`Vec2::ZERO`] comes back.
    pub fn stabilise_against(&mut self, previous: &Layout) -> Vec2 {
        let mut dx = Vec::with_capacity(self.placements.len());
        let mut dy = Vec::with_capacity(self.placements.len());
        for placement in &self.placements {
            if let Some(old) = previous.get(placement.node) {
                let moved = old.rect.centre() - placement.rect.centre();
                dx.push(moved.x);
                dy.push(moved.y);
            }
        }
        let Some(offset) = median(&mut dx).zip(median(&mut dy)).map(|(x, y)| Vec2::new(x, y))
        else {
            return Vec2::ZERO;
        };
        self.translate(offset);
        offset
    }

    /// Total L1 distance every shared node moved between the two layouts — the
    /// quantity [`Self::stabilise_against`] minimises, exposed so that a test can
    /// check the minimum really is one, and so that the app can decide whether a
    /// change is worth animating.
    pub fn displacement_from(&self, previous: &Layout) -> f64 {
        self.placements
            .iter()
            .filter_map(|p| previous.get(p.node).map(|old| (old, p)))
            .map(|(old, new)| (old.rect.centre() - new.rect.centre()).manhattan_length())
            .sum()
    }
}

/// The **lower** median of a sample, or `None` for an empty one.
///
/// For an even sample every value between the two central ones is an equally good L1
/// minimum, so the choice is free — and the textbook midpoint is the worst member of
/// that set for what this is used for. Two nodes that did not move and two that
/// moved 44px cost the same total whichever offset in `0..=44` is picked, but the
/// midpoint, 22, moves all four of them, while 0 leaves two exactly where they were.
/// Taking an actual observed displacement is therefore strictly better here and
/// never worse.
fn median(sample: &mut [f64]) -> Option<f64> {
    if sample.is_empty() {
        return None;
    }
    sample.sort_unstable_by(f64::total_cmp);
    Some(sample[(sample.len() - 1) / 2])
}

impl MindMap {
    /// Lay the map out.
    ///
    /// `O(n)` in visible nodes for every kind — the tidy pass is linear, the level
    /// extents are one sweep, and the balanced form's branch weighing visits each
    /// node once across all branches. The hit index adds one `O(n log n)` sort.
    pub fn layout(&self, options: &LayoutOptions) -> Layout {
        match options.kind {
            LayoutKind::Tree { direction } => linear::tree(self, options, direction),
            LayoutKind::Balanced { axis } => linear::balanced(self, options, axis),
            LayoutKind::Radial { start_angle, sweep } => {
                radial::radial(self, options, start_angle, sweep)
            }
        }
    }

    /// Lay the map out and slide the result to keep the user's place.
    ///
    /// This is the call an editor should make after every structural edit, passing
    /// the layout it is currently showing. With `None` it is exactly
    /// [`Self::layout`].
    pub fn layout_stable(&self, options: &LayoutOptions, previous: Option<&Layout>) -> Layout {
        let mut layout = self.layout(options);
        if let Some(previous) = previous {
            layout.stabilise_against(previous);
        }
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Node;

    fn node(w: f64, h: f64) -> Node {
        Node::new("n").with_size(Size::new(w, h))
    }

    fn sample() -> (MindMap, Vec<NodeId>) {
        let mut map = MindMap::with_root(node(120.0, 40.0));
        let root = map.root();
        let mut ids = vec![root];
        for _ in 0..3 {
            let branch = map.add_child(root, node(100.0, 36.0)).unwrap();
            ids.push(branch);
            for _ in 0..2 {
                ids.push(map.add_child(branch, node(90.0, 32.0)).unwrap());
            }
        }
        (map, ids)
    }

    #[test]
    fn every_visible_node_is_placed_exactly_once() {
        let (map, ids) = sample();
        for kind in [
            LayoutKind::Tree { direction: Direction::Right },
            LayoutKind::Balanced { axis: Axis::Horizontal },
            LayoutKind::Radial { start_angle: 0.0, sweep: std::f64::consts::TAU },
        ] {
            let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
            assert_eq!(layout.len(), ids.len(), "{kind:?}");
            for &id in &ids {
                assert!(layout.get(id).is_some(), "{kind:?} lost {id:?}");
            }
        }
    }

    #[test]
    fn bounds_cover_every_node_and_nothing_else() {
        let (map, _) = sample();
        let layout = map.layout(&LayoutOptions::default());
        let bounds = layout.bounds().unwrap();
        let union = Rect::of(layout.placements().iter().map(|p| p.rect)).unwrap();
        assert_eq!(bounds, union);
        assert!(layout.placements().iter().all(|p| bounds.contains(p.rect.min)));
    }

    #[test]
    fn hit_testing_finds_the_node_under_a_point_and_nothing_between_them() {
        let (map, ids) = sample();
        let layout = map.layout(&LayoutOptions::default());
        for &id in &ids {
            let rect = layout.rect(id).unwrap();
            assert_eq!(layout.hit_test(rect.centre()), Some(id));
            assert_eq!(layout.hit_test(rect.min), Some(id), "a click on the border counts");
        }
        let bounds = layout.bounds().unwrap();
        assert_eq!(layout.hit_test(Point::new(bounds.min.x - 10.0, bounds.min.y - 10.0)), None);
    }

    #[test]
    fn hit_testing_agrees_with_a_brute_force_scan_everywhere() {
        let (map, _) = sample();
        let layout = map.layout(&LayoutOptions::default());
        let bounds = layout.bounds().unwrap();
        let brute = |p: Point| {
            layout.placements().iter().find(|pl| pl.rect.contains(p)).map(|pl| pl.node)
        };
        for i in 0..60 {
            for j in 0..60 {
                let p = Point::new(
                    bounds.min.x - 20.0 + (bounds.width() + 40.0) * f64::from(i) / 59.0,
                    bounds.min.y - 20.0 + (bounds.height() + 40.0) * f64::from(j) / 59.0,
                );
                assert_eq!(layout.hit_test(p), brute(p), "at {p:?}");
            }
        }
    }

    #[test]
    fn translating_a_layout_moves_everything_and_keeps_the_hit_index_valid() {
        let (map, ids) = sample();
        let mut layout = map.layout(&LayoutOptions::default());
        let before = layout.rect(ids[0]).unwrap();
        layout.translate(Vec2::new(1000.0, -250.0));
        assert_eq!(layout.rect(ids[0]).unwrap(), before.translated(Vec2::new(1000.0, -250.0)));
        assert_eq!(layout.hit_test(layout.rect(ids[4]).unwrap().centre()), Some(ids[4]));
    }

    #[test]
    fn the_origin_option_places_the_root_and_nothing_else_decides_it() {
        let (map, ids) = sample();
        let origin = Point::new(-4000.0, 900.0);
        let layout =
            map.layout(&LayoutOptions { origin, ..LayoutOptions::default() });
        assert_eq!(layout.rect(ids[0]).unwrap().centre(), origin);
    }

    #[test]
    fn the_median_offset_really_is_the_l1_minimum() {
        let (mut map, ids) = sample();
        let options = LayoutOptions::default();
        let before = map.layout(&options);

        // An edit near the top of the map, which pushes most of it downwards.
        map.add_child(ids[1], node(90.0, 32.0)).unwrap();
        let unstabilised = map.layout(&options);
        let mut stabilised = unstabilised.clone();
        let offset = stabilised.stabilise_against(&before);

        let cost = |candidate: Vec2| {
            let mut trial = unstabilised.clone();
            trial.translate(candidate);
            trial.displacement_from(&before)
        };
        let best = cost(offset);
        // Sweep a grid around the chosen offset: nothing beats the median.
        for i in -12i32..=12 {
            for j in -12i32..=12 {
                let trial =
                    offset + Vec2::new(f64::from(i) * 3.5, f64::from(j) * 3.5);
                assert!(cost(trial) >= best - 1e-6, "{trial:?} beat the median {offset:?}");
            }
        }
        assert!(best <= cost(Vec2::ZERO), "stabilising never costs more than not");
    }

    #[test]
    fn stabilising_against_a_layout_with_nothing_in_common_does_nothing() {
        let (map, _) = sample();
        let other = MindMap::with_root(node(10.0, 10.0)).layout(&LayoutOptions::default());
        let mut layout = map.layout(&LayoutOptions::default());
        let unchanged = layout.clone();
        assert_eq!(layout.stabilise_against(&other), Vec2::ZERO);
        assert_eq!(layout, unchanged);
    }

    #[test]
    fn layout_is_deterministic() {
        let (map, _) = sample();
        let options = LayoutOptions::default();
        assert_eq!(map.layout(&options), map.layout(&options));
    }

    #[test]
    fn the_median_is_always_an_observed_value() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [3.0]), Some(3.0));
        assert_eq!(median(&mut [1.0, 3.0, 2.0]), Some(2.0));
        // The even case: 2.5 would also minimise the total, and would move all four.
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [44.0, 0.0, 0.0, 44.0]), Some(0.0));
    }

    #[test]
    fn an_even_split_settles_on_a_side_rather_than_between_them() {
        // Two nodes that did not move and two that did: the L1 cost is the same for
        // any offset between, and this is the one that leaves two of them alone.
        let (mut map, ids) = sample();
        let options = LayoutOptions::default();
        let before = map.layout(&options);
        map.remove_subtree(ids[3]).unwrap();

        let mut after = map.layout(&options);
        after.stabilise_against(&before);
        let unmoved = after
            .placements()
            .iter()
            .filter(|p| before.rect(p.node).map(|r| r.centre()) == Some(p.rect.centre()))
            .count();
        assert!(unmoved > 0, "the offset chosen left nothing exactly in place");
    }
}
