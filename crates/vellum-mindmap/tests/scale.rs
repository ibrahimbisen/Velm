//! Large maps: does the layout still hold up, and does it still finish?
//!
//! The claim this crate makes is that a mind map arranges itself *correctly*, and
//! "correctly" is mostly one property: **no two node boxes overlap**. That is easy
//! to satisfy on the six-node map in a screenshot and easy to break on a real one,
//! so it is checked here on a thousand nodes at a time, in four tree shapes chosen
//! to attack the layout from different directions, in all three layout forms.
//!
//! The shapes are not arbitrary:
//!
//! - **Ragged** — a randomly branching tree with randomly sized nodes. The ordinary
//!   case, and the one where variable extents interact with variable fan-out.
//! - **Chain** — a thousand nodes deep and one wide. The case a recursive tidy
//!   implementation crashes on, and the case where every level holds one node so the
//!   contour threading in [`vellum_mindmap`]'s apportion pass never gets to rest.
//! - **Star** — one root, 999 leaves. One level holding almost the whole map, which
//!   is where a radial layout has to fit nearly a thousand boxes onto one ring.
//! - **Comb** — a spine with a bush hanging off every vertebra. Walker's original
//!   algorithm is quadratic on shapes like this; Buchheim's is not, and the timing
//!   assertion below is what would notice if that ever regressed.
//!
//! The timing bound is deliberately loose — this runs in a debug build on whatever
//! machine happens to be free — so it is not a benchmark. It is a tripwire for an
//! accidental `O(n²)`, which on these inputs would not be a slow test but a hung one.

use std::time::Instant;

use vellum_mindmap::{
    Axis, ConnectorOptions, ConnectorShape, Direction, Layout, LayoutKind, LayoutOptions,
    MindMap, Node, NodeId, Rect, Size,
};

/// Every layout form, so a property is never proved for one and assumed for the rest.
fn all_kinds() -> Vec<LayoutKind> {
    vec![
        LayoutKind::Tree { direction: Direction::Right },
        LayoutKind::Tree { direction: Direction::Down },
        LayoutKind::Balanced { axis: Axis::Horizontal },
        LayoutKind::Balanced { axis: Axis::Vertical },
        LayoutKind::Radial { start_angle: 0.0, sweep: std::f64::consts::TAU },
        LayoutKind::Radial { start_angle: -0.5, sweep: std::f64::consts::PI },
    ]
}

/// A deterministic 64-bit LCG. Deterministic on purpose: a layout test that fails
/// once a fortnight on a seed nobody recorded is worse than no test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn size(&mut self) -> Size {
        Size::new(60.0 + self.below(220) as f64, 28.0 + self.below(40) as f64)
    }
}

/// A randomly branching tree of `count` nodes with randomly sized boxes.
fn ragged(count: usize, seed: u64) -> MindMap {
    let mut rng = Rng(seed);
    let mut map = MindMap::with_root(Node::new("root").with_size(rng.size()));
    let mut open = vec![map.root()];
    while map.node_count() < count {
        // Bias towards the recently added, which produces a deep, irregular tree
        // rather than the wide shallow one a uniform pick gives.
        let pick = open.len() - 1 - (rng.below(6).min(open.len() as u64 - 1) as usize);
        let parent = open[pick];
        let size = rng.size();
        let child = map.add_child(parent, Node::new("n").with_size(size)).unwrap();
        open.push(child);
        if map.children(parent).len() > 4 {
            open.retain(|&id| id != parent);
        }
    }
    map
}

fn chain(count: usize) -> MindMap {
    let mut map = MindMap::with_root(Node::new("root").with_size(Size::new(120.0, 36.0)));
    let mut tip = map.root();
    for i in 1..count {
        tip = map
            .add_child(tip, Node::new(format!("n{i}")).with_size(Size::new(120.0, 36.0)))
            .unwrap();
    }
    map
}

fn star(count: usize) -> MindMap {
    let mut map = MindMap::with_root(Node::new("root").with_size(Size::new(160.0, 44.0)));
    let root = map.root();
    for i in 1..count {
        map.add_child(root, Node::new(format!("n{i}")).with_size(Size::new(100.0, 32.0)))
            .unwrap();
    }
    map
}

/// A spine of `count / 8` nodes, each carrying seven leaves.
fn comb(count: usize) -> MindMap {
    let mut map = MindMap::with_root(Node::new("root").with_size(Size::new(160.0, 44.0)));
    let mut spine = map.root();
    while map.node_count() + 8 <= count {
        let next = map.add_child(spine, Node::new("s").with_size(Size::new(140.0, 40.0))).unwrap();
        for _ in 0..7 {
            map.add_child(next, Node::new("l").with_size(Size::new(90.0, 30.0))).unwrap();
        }
        spine = next;
    }
    map
}

fn shapes(count: usize) -> Vec<(&'static str, MindMap)> {
    vec![
        ("ragged", ragged(count, 0x5EED)),
        ("chain", chain(count)),
        ("star", star(count)),
        ("comb", comb(count)),
    ]
}

/// Assert that no two node boxes overlap, by a sweep over the y axis.
///
/// A sweep rather than the `n²` pairwise scan, so that this stays usable as maps get
/// bigger: node boxes in every layout here are short and separated vertically, so
/// the active set at any point in the sweep is a handful of nodes, not a level.
///
/// Deliberately written from the *placements alone*, with no knowledge of how they
/// were produced. A checker that assumed levels cannot collide would be assuming
/// exactly the thing under test.
fn assert_no_overlap(layout: &Layout, what: &str) {
    let mut boxes: Vec<(Rect, NodeId)> =
        layout.placements().iter().map(|p| (p.rect, p.node)).collect();
    boxes.sort_by(|a, b| a.0.min.y.total_cmp(&b.0.min.y));

    let mut active: Vec<(Rect, NodeId)> = Vec::new();
    for &(rect, node) in &boxes {
        active.retain(|(other, _)| other.max.y > rect.min.y);
        for &(other, other_node) in &active {
            assert!(
                !rect.intersects(other),
                "{what}: {node:?} at {rect:?} overlaps {other_node:?} at {other:?}"
            );
        }
        active.push((rect, node));
    }
}

#[test]
fn a_thousand_nodes_lay_out_without_overlap_in_every_form() {
    let options = LayoutOptions::default();
    let started = Instant::now();
    let mut laid_out = 0usize;

    for (name, map) in shapes(1000) {
        assert!(map.node_count() >= 990, "{name} built {} nodes", map.node_count());
        map.validate().unwrap();
        for kind in all_kinds() {
            let layout = map.layout(&LayoutOptions { kind, ..options });
            assert_eq!(layout.len(), map.node_count(), "{name} {kind:?} lost nodes");
            assert_no_overlap(&layout, &format!("{name} {kind:?}"));
            assert!(layout.bounds().is_some());
            laid_out += layout.len();
        }
    }

    let elapsed = started.elapsed();
    // 24 layouts of ~1000 nodes. A linear implementation does this in milliseconds;
    // the bound is loose enough to survive a loaded CI machine and tight enough that
    // an accidental quadratic pass — which would be minutes here — cannot pass it.
    assert!(
        elapsed.as_secs_f64() < 20.0,
        "{laid_out} placements took {elapsed:?}, which is not linear-time behaviour"
    );
}

#[test]
fn bounds_and_hit_testing_hold_on_a_thousand_nodes() {
    for (name, map) in shapes(1000) {
        for kind in all_kinds() {
            let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
            let bounds = layout.bounds().unwrap();
            for placement in layout.placements() {
                assert!(
                    bounds.contains(placement.rect.min) && bounds.contains(placement.rect.max),
                    "{name} {kind:?}: {:?} escaped the bounds",
                    placement.node
                );
                // Non-overlap is what makes this unambiguous: the centre of any node
                // is inside exactly one box.
                assert_eq!(
                    layout.hit_test(placement.rect.centre()),
                    Some(placement.node),
                    "{name} {kind:?}"
                );
            }
            assert_eq!(layout.hit_test(bounds.min - vellum_mindmap::Vec2::new(1.0, 1.0)), None);
        }
    }
}

#[test]
fn every_connector_joins_the_two_nodes_it_names() {
    let map = ragged(1000, 0xC0FFEE);
    for kind in all_kinds() {
        for shape in [ConnectorShape::Straight, ConnectorShape::Elbow, ConnectorShape::Curve] {
            let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
            let links = layout.connectors(&ConnectorOptions { shape, ..Default::default() });
            assert_eq!(links.len(), map.node_count() - 1, "{kind:?} {shape:?}");

            for link in &links {
                assert_eq!(map.parent(link.child), Some(link.parent));
                assert!(link.points.len() >= 2);
                assert!(link.points.iter().all(|p| p.is_finite()));

                // Each end sits on the border of the node it attaches to, and the
                // whole path stays inside the hull of the two — the property
                // `Layout::bounds` relies on to need node rects alone.
                let (from, to) =
                    (layout.rect(link.parent).unwrap(), layout.rect(link.child).unwrap());
                assert!(from.contains(link.start()), "{kind:?} {shape:?}: start left {from:?}");
                assert!(to.contains(link.end()), "{kind:?} {shape:?}: end left {to:?}");
                let hull = from.union(to);
                let bounds = link.bounds();
                assert!(
                    hull.contains(bounds.min) && hull.contains(bounds.max),
                    "{kind:?} {shape:?}: path escaped the two nodes it joins"
                );
            }
        }
    }
}

#[test]
fn a_thousand_node_chain_neither_overflows_the_stack_nor_wanders() {
    // A thousand levels is a plausible imported outline and an implausible stack
    // depth. Both walks of the tidy pass, and every traversal in `MindMap`, are
    // iterative for this reason.
    let map = chain(1000);
    for kind in all_kinds() {
        let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
        assert_eq!(layout.len(), 1000);
        assert!(layout.placements().iter().all(|p| p.rect.centre().is_finite()));
        assert_no_overlap(&layout, "chain");
    }

    // In a one-sided tree a chain is a straight line: every node shares one centre
    // line, because a lone child is centred under its parent at every level.
    let layout = map.layout(&LayoutOptions {
        kind: LayoutKind::Tree { direction: Direction::Right },
        ..LayoutOptions::default()
    });
    assert!(
        layout.placements().iter().all(|p| p.rect.centre().y.abs() < 1e-9),
        "a chain should not drift off its own axis"
    );
}

#[test]
fn four_thousand_nodes_still_finish_and_still_do_not_overlap() {
    // Four times the required size, to show the cost curve rather than one point on
    // it. If this passes and the thousand-node test passes in a comparable time per
    // node, the implementation is linear in practice as well as on paper.
    let map = ragged(4000, 0xBEEF);
    let started = Instant::now();
    for kind in all_kinds() {
        let layout = map.layout(&LayoutOptions { kind, ..LayoutOptions::default() });
        assert_eq!(layout.len(), map.node_count());
        assert_no_overlap(&layout, "ragged 4000");
    }
    assert!(started.elapsed().as_secs_f64() < 40.0, "took {:?}", started.elapsed());
}
