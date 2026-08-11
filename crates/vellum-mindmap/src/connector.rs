//! The lines between a node and its children, emitted as point lists.
//!
//! **Point lists, not meshes.** `vellum-connect` already owns stroke tessellation,
//! dash patterns, arrowheads and jump-overs; a mind map's links are ordinary
//! connectors that happen to be derived from a layout rather than bound to two
//! widgets, so they go through that crate rather than growing a second stroker here.
//! A [`ConnectorPath`] maps onto `vellum_connect::RoutedPath::from_points` with a
//! struct-literal conversion of each point, and this crate stays free of `lyon`.
//!
//! **Curves are flattened here rather than emitted as control points**, for the same
//! reason: `RoutedPath` would carry a cubic perfectly well, but then this crate
//! would be describing geometry in `vellum-connect`'s vocabulary and would have to
//! track it. A polyline is the one representation both sides already agree on, and
//! [`ConnectorOptions::curve_samples`] is a layout-time decision anyway — a mind map
//! link spans a fixed world distance, so the sample count that looks smooth does not
//! change with zoom the way a freehand stroke's does.
//!
//! # Where a link attaches
//!
//! Two rules, chosen by [`Layout::kind`], because one rule genuinely does not fit
//! both cases:
//!
//! - [`Anchor::Along`] — used by the tree and balanced layouts. The link leaves the
//!   parent's **branch face** and enters the child's opposite face, both at the
//!   node's mid-height. That is what a mind map looks like: in a left-to-right tree
//!   every link leaves a right edge and enters a left edge, so the map reads as an
//!   outline. The obvious alternative — where the line between the two centres
//!   crosses the box — is *geometrically* right and *visually* wrong here, because
//!   mind-map nodes are wide and short: a child a few hundred px down and across
//!   would be reached out of the parent's **bottom** edge, and a map of those reads
//!   as a web rather than a tree.
//! - [`Anchor::Facing`] — used by the radial layout, where there is no branch axis
//!   and a child can sit at any angle. Here the crossing point *is* the right
//!   answer, and it comes with the edge's outward normal.
//!
//! Both produce a point and a normal, and everything downstream — elbow, curve —
//! works from those alone. The normal is what makes those forms leave a node
//! perpendicular to its edge instead of at whatever angle the centres happen to
//! make.

use serde::{Deserialize, Serialize};

use crate::geometry::{Point, Rect, Vec2};
use crate::layout::{Direction, Layout, LayoutKind, Side};
use crate::tree::NodeId;

/// How a link decides which point on a node's border to attach to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anchor {
    /// Leave the face pointing this way, at mid-height, and enter the other node's
    /// opposite face. The rule for a branch that has a direction.
    Along(Direction),
    /// Leave wherever the line between the two centres crosses the border. The rule
    /// for a radial map, where a branch does not.
    Facing,
}

impl Anchor {
    /// Attachment point and outward normal on `rect`, for a link to `other`.
    fn resolve(self, rect: Rect, other: Rect, outward: bool) -> (Point, Vec2) {
        match self {
            Self::Facing => rect.boundary_towards(other.centre()),
            Self::Along(direction) => {
                let normal = if outward { direction.normal() } else { -direction.normal() };
                let centre = rect.centre();
                // Read off the rect's own edges rather than reconstructing them as
                // `centre ± half`, which is a rounding step away from landing
                // outside the very node the link is attaching to.
                let point = Point::new(
                    if normal.x > 0.0 {
                        rect.max.x
                    } else if normal.x < 0.0 {
                        rect.min.x
                    } else {
                        centre.x
                    },
                    if normal.y > 0.0 {
                        rect.max.y
                    } else if normal.y < 0.0 {
                        rect.min.y
                    } else {
                        centre.y
                    },
                );
                (point, normal)
            }
        }
    }
}

/// How a link is drawn between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ConnectorShape {
    /// Two points, edge to edge.
    Straight,
    /// Orthogonal: leave perpendicular, turn, arrive perpendicular. The form that
    /// makes a mind map read as an outline, because every link on a level turns on
    /// the same line.
    Elbow,
    /// A cubic Bézier leaving and arriving along the edge normals, flattened to a
    /// polyline. The default: it is what a hand-drawn mind map looks like, and it
    /// keeps the branch colour legible where a stack of elbows would overlap.
    #[default]
    Curve,
}

/// Options for emitting connector paths.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConnectorOptions {
    pub shape: ConnectorShape,
    /// How far a link travels perpendicular to a node's edge before it may turn.
    ///
    /// Also the minimum bow of a curve, so that a link between two nodes that are
    /// nearly on top of each other is still visibly a link rather than a dot.
    pub stub: f64,
    /// Segments per curve. 16 is smooth at any zoom a mind map is read at; the cost
    /// is one `Point` each, and these are regenerated per layout, not per frame.
    pub curve_samples: u32,
}

impl Default for ConnectorOptions {
    fn default() -> Self {
        Self { shape: ConnectorShape::default(), stub: 16.0, curve_samples: 16 }
    }
}

/// One parent → child link. `points` is a polyline in world space, from the parent's
/// edge to the child's, always with at least two points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorPath {
    pub parent: NodeId,
    pub child: NodeId,
    pub points: Vec<Point>,
}

impl ConnectorPath {
    pub fn start(&self) -> Point {
        self.points[0]
    }

    pub fn end(&self) -> Point {
        self.points[self.points.len() - 1]
    }

    /// The bounds of the polyline. Always inside the bounds of the two nodes it
    /// joins, which is why [`Layout::bounds`] needs only node rects.
    pub fn bounds(&self) -> Rect {
        let mut bounds = Rect::new(self.start(), self.start());
        for &p in &self.points {
            bounds = bounds.union(Rect::new(p, p));
        }
        bounds
    }
}

/// Every parent → child link in a layout, in placement order.
pub(crate) fn paths(layout: &Layout, options: &ConnectorOptions) -> Vec<ConnectorPath> {
    layout
        .placements()
        .iter()
        .filter_map(|placement| {
            let parent = placement.parent?;
            let parent_rect = layout.rect(parent)?;
            Some(ConnectorPath {
                parent,
                child: placement.node,
                points: between(
                    parent_rect,
                    placement.rect,
                    anchor_for(layout.kind(), placement.side),
                    options,
                ),
            })
        })
        .collect()
}

/// The attachment rule for a link arriving at a node on `side` of a `kind` map.
///
/// Keyed on the **child's** side, because that is the branch the link belongs to:
/// in a balanced map the root's links leave leftwards and rightwards at once, and it
/// is the child that says which.
pub fn anchor_for(kind: LayoutKind, side: Side) -> Anchor {
    match kind {
        LayoutKind::Tree { direction } => Anchor::Along(direction),
        LayoutKind::Balanced { axis } => {
            let (primary, secondary) = axis.directions();
            Anchor::Along(if side == Side::Secondary { secondary } else { primary })
        }
        LayoutKind::Radial { .. } => Anchor::Facing,
    }
}

/// The polyline from one node's border to another's.
pub fn between(
    parent: Rect,
    child: Rect,
    anchor: Anchor,
    options: &ConnectorOptions,
) -> Vec<Point> {
    let (from, from_normal) = anchor.resolve(parent, child, true);
    let (to, to_normal) = anchor.resolve(child, parent, false);
    let points = match options.shape {
        ConnectorShape::Straight => vec![from, to],
        ConnectorShape::Elbow => elbow(from, from_normal, to, to_normal, options.stub),
        ConnectorShape::Curve => {
            curve(from, from_normal, to, to_normal, options.stub, options.curve_samples)
        }
    };
    dedupe(points)
}

/// An orthogonal link: out along the exit normal, across, and in along the entry
/// normal.
///
/// When both ends face along the same axis — the usual case, a parent and child in
/// adjacent columns — the turn happens midway between them, so every link on a level
/// turns on one line. When the two ends face along different axes, as they do in a
/// radial map, a single corner is enough and a midway turn would draw a needless
/// zig-zag.
fn elbow(from: Point, from_normal: Vec2, to: Point, to_normal: Vec2, stub: f64) -> Vec<Point> {
    let exits_horizontally = from_normal.y == 0.0;
    let enters_horizontally = to_normal.y == 0.0;
    match (exits_horizontally, enters_horizontally) {
        // Already in line: the bracket would degenerate into three collinear points,
        // and a redundant vertex is a redundant join for the stroker to mitre.
        (true, true) if from.y == to.y => vec![from, to],
        (false, false) if from.x == to.x => vec![from, to],
        (true, true) => {
            let x = turn(from.x, to.x, from_normal.x, stub);
            vec![from, Point::new(x, from.y), Point::new(x, to.y), to]
        }
        (false, false) => {
            let y = turn(from.y, to.y, from_normal.y, stub);
            vec![from, Point::new(from.x, y), Point::new(to.x, y), to]
        }
        (true, false) => vec![from, Point::new(to.x, from.y), to],
        (false, true) => vec![from, Point::new(from.x, to.y), to],
    }
}

/// Where an elbow turns: midway between the two ends, unless that would be behind
/// the exit or closer to it than `stub`, in which case it is exactly `stub` out
/// along the normal.
///
/// The clamp is not cosmetic. Two nodes can be side by side with the child's near
/// edge *behind* the parent's — a wide parent and a short first child in a balanced
/// map — and an unclamped midpoint then draws the link backwards through the parent.
fn turn(from: f64, to: f64, normal: f64, stub: f64) -> f64 {
    let midpoint = from + 0.5 * (to - from);
    if (midpoint - from) * normal >= stub { midpoint } else { from + normal * stub }
}

/// A cubic Bézier leaving along `from_normal` and arriving along `to_normal`,
/// flattened to `samples` segments.
///
/// The control handles are half the distance the link actually travels *along each
/// normal*, floored at `stub`. Using the along-normal component rather than the
/// straight-line distance is what keeps a long, nearly-flat link from bulging: a
/// child directly to the right of its parent gets a gentle S, not a loop.
fn curve(
    from: Point,
    from_normal: Vec2,
    to: Point,
    to_normal: Vec2,
    stub: f64,
    samples: u32,
) -> Vec<Point> {
    let delta = to - from;
    let along = |normal: Vec2, v: Vec2| (v.x * normal.x + v.y * normal.y).abs() * 0.5;
    let first = from + from_normal * along(from_normal, delta).max(stub);
    let second = to + to_normal * along(to_normal, -delta).max(stub);

    // At least two segments: one would be a straight line and would silently drop
    // the shape the caller asked for.
    let steps = samples.clamp(2, 256);
    (0..=steps)
        .map(|i| {
            let t = f64::from(i) / f64::from(steps);
            cubic(from, first, second, to, t)
        })
        .collect()
}

/// De Casteljau at one parameter. Written out rather than looped: a cubic is three
/// lerps and the closed form is both faster and easier to read than a general
/// subdivision.
fn cubic(a: Point, b: Point, c: Point, d: Point, t: f64) -> Point {
    let (ab, bc, cd) = (a.lerp(b, t), b.lerp(c, t), c.lerp(d, t));
    ab.lerp(bc, t).lerp(bc.lerp(cd, t), t)
}

/// Drop consecutive coincident points, and guarantee at least two.
///
/// Zero-length segments are the classic way to make a stroke tessellator produce
/// `NaN` normals, and they arise here for entirely ordinary reasons: an elbow whose
/// two ends are already aligned needs no corner, and a curve between two touching
/// nodes collapses.
fn dedupe(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if out.last().is_none_or(|&last| !last.coincident_with(p)) {
            out.push(p);
        }
    }
    while out.len() < 2 {
        out.push(*out.last().unwrap_or(&Point::ORIGIN));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::layout::{Axis, LayoutOptions};
    use crate::tree::{MindMap, Node};

    fn rect(cx: f64, cy: f64, w: f64, h: f64) -> Rect {
        Rect::from_centre(Point::new(cx, cy), Size::new(w, h))
    }

    fn opts(shape: ConnectorShape) -> ConnectorOptions {
        ConnectorOptions { shape, ..ConnectorOptions::default() }
    }

    const RIGHT: Anchor = Anchor::Along(Direction::Right);

    #[test]
    fn an_along_anchor_uses_the_branch_face_not_the_nearest_edge() {
        // A wide, short parent with a child far below and to the right. The line
        // between the centres leaves through the *bottom*, which is why `Facing` is
        // not the rule a tree layout uses.
        let parent = rect(0.0, 0.0, 100.0, 40.0);
        let child = rect(400.0, 200.0, 100.0, 40.0);

        let along = between(parent, child, RIGHT, &opts(ConnectorShape::Straight));
        assert_eq!(along, vec![Point::new(50.0, 0.0), Point::new(350.0, 200.0)]);

        let facing = between(parent, child, Anchor::Facing, &opts(ConnectorShape::Straight));
        assert_eq!(facing[0], Point::new(40.0, 20.0), "the geometric rule exits downwards");
    }

    #[test]
    fn a_straight_link_runs_edge_to_edge_not_centre_to_centre() {
        let parent = rect(0.0, 0.0, 100.0, 40.0);
        let child = rect(400.0, 0.0, 100.0, 40.0);
        let points = between(parent, child, RIGHT, &opts(ConnectorShape::Straight));
        assert_eq!(points, vec![Point::new(50.0, 0.0), Point::new(350.0, 0.0)]);
    }

    #[test]
    fn an_elbow_between_facing_edges_turns_midway() {
        let parent = rect(0.0, 0.0, 100.0, 40.0);
        let child = rect(400.0, 200.0, 100.0, 40.0);
        let points = between(parent, child, RIGHT, &opts(ConnectorShape::Elbow));
        assert_eq!(points.len(), 4);
        assert_eq!(points[0], Point::new(50.0, 0.0));
        assert_eq!(points[1].x, points[2].x, "one vertical run");
        assert_eq!(points[1].x, 200.0, "midway between the two facing edges");
        assert_eq!(points[3], Point::new(350.0, 200.0));
    }

    #[test]
    fn every_link_on_a_level_turns_on_the_same_line() {
        // What the elbow form is for. Three children at different heights, one
        // parent: the vertical runs all share an x, so the map reads as an outline.
        let parent = rect(0.0, 0.0, 100.0, 40.0);
        // None of them level with the parent, because a link that is already in line
        // has no turn to compare — see the collapse case in `elbow`.
        let turns: Vec<f64> = [-300.0, 80.0, 260.0]
            .into_iter()
            .map(|y| between(parent, rect(400.0, y, 100.0, 40.0), RIGHT, &opts(ConnectorShape::Elbow))[1].x)
            .collect();
        assert!(turns.windows(2).all(|w| w[0] == w[1]), "{turns:?}");
    }

    #[test]
    fn an_elbow_never_turns_backwards_through_its_own_node() {
        // A wide parent and a child whose near edge is behind the parent's, which
        // happens whenever the level gap is small next to the widest node on a level.
        let parent = rect(0.0, 0.0, 400.0, 40.0);
        let child = rect(210.0, 300.0, 40.0, 40.0);
        let options = opts(ConnectorShape::Elbow);
        let points = between(parent, child, RIGHT, &options);
        let exit = points[0];
        assert_eq!(exit, Point::new(200.0, 0.0));
        assert!(
            points[1].x >= exit.x + options.stub - 1e-9,
            "turned at {} from an exit at {}",
            points[1].x,
            exit.x
        );
    }

    #[test]
    fn an_elbow_across_axes_needs_only_one_corner() {
        // The radial case: the parent is left through its side and the child is
        // entered from above, so a single corner is the whole path.
        let parent = rect(0.0, 0.0, 400.0, 40.0);
        let child = rect(300.0, 400.0, 40.0, 400.0);
        let points = between(parent, child, Anchor::Facing, &opts(ConnectorShape::Elbow));
        assert_eq!(points.len(), 3, "{points:?}");
        // The parent is left downwards and the child entered from its side, so the
        // corner keeps the exit's x and the entry's y.
        assert_eq!(points[1], Point::new(points[0].x, points[2].y));
    }

    #[test]
    fn an_elbow_collapses_cleanly_when_the_two_ends_already_line_up() {
        let parent = rect(0.0, 0.0, 100.0, 40.0);
        let child = rect(400.0, 0.0, 100.0, 40.0);
        let points = between(parent, child, RIGHT, &opts(ConnectorShape::Elbow));
        assert_eq!(points, vec![Point::new(50.0, 0.0), Point::new(350.0, 0.0)]);
    }

    #[test]
    fn a_curve_leaves_and_arrives_along_the_edge_normals() {
        let parent = rect(0.0, 0.0, 100.0, 40.0);
        let child = rect(400.0, 200.0, 100.0, 40.0);
        let points = between(parent, child, RIGHT, &opts(ConnectorShape::Curve));
        assert_eq!(points.len(), 17);
        assert_eq!(points[0], Point::new(50.0, 0.0));
        assert_eq!(*points.last().unwrap(), Point::new(350.0, 200.0));

        // The tangent at t=0 is the exit normal and at t=1 the entry normal, so a
        // finely sampled first and last step are horizontal to within a hair. Sampled
        // finely on purpose: the deviation of a chord from the tangent shrinks with
        // the step, so a loose tolerance on a coarse sample would prove nothing.
        let fine = ConnectorOptions { curve_samples: 256, ..opts(ConnectorShape::Curve) };
        let points = between(parent, child, RIGHT, &fine);
        let first = points[1] - points[0];
        assert!(first.x > 0.0 && first.y.abs() < 0.01 * first.x, "{first:?}");
        let last = points[256] - points[255];
        assert!(last.x > 0.0 && last.y.abs() < 0.01 * last.x, "{last:?}");
    }

    #[test]
    fn a_curve_stays_between_the_two_nodes() {
        let parent = rect(0.0, 0.0, 100.0, 40.0);
        let child = rect(400.0, 200.0, 100.0, 40.0);
        let root = MindMap::new("x").root();
        let path = ConnectorPath {
            parent: root,
            child: root,
            points: between(parent, child, RIGHT, &opts(ConnectorShape::Curve)),
        };
        let hull = parent.union(child);
        let bounds = path.bounds();
        assert!(hull.contains(bounds.min) && hull.contains(bounds.max));
        assert_eq!(path.start(), Point::new(50.0, 0.0));
        assert_eq!(path.end(), Point::new(350.0, 200.0));
    }

    #[test]
    fn coincident_nodes_still_produce_a_two_point_path() {
        let same = rect(0.0, 0.0, 100.0, 40.0);
        for anchor in [RIGHT, Anchor::Facing] {
            for shape in [ConnectorShape::Straight, ConnectorShape::Elbow, ConnectorShape::Curve] {
                let points = between(same, same, anchor, &opts(shape));
                assert!(points.len() >= 2, "{shape:?}/{anchor:?} produced {points:?}");
                assert!(points.iter().all(|p| p.is_finite()));
            }
        }
    }

    #[test]
    fn a_balanced_map_anchors_each_side_to_its_own_face() {
        let horizontal = LayoutKind::Balanced { axis: Axis::Horizontal };
        assert_eq!(anchor_for(horizontal, Side::Primary), Anchor::Along(Direction::Right));
        assert_eq!(anchor_for(horizontal, Side::Secondary), Anchor::Along(Direction::Left));

        let vertical = LayoutKind::Balanced { axis: Axis::Vertical };
        assert_eq!(anchor_for(vertical, Side::Secondary), Anchor::Along(Direction::Up));

        assert_eq!(
            anchor_for(LayoutKind::Tree { direction: Direction::Left }, Side::Primary),
            Anchor::Along(Direction::Left)
        );
        assert_eq!(
            anchor_for(LayoutKind::Radial { start_angle: 0.0, sweep: 1.0 }, Side::Primary),
            Anchor::Facing
        );
    }

    #[test]
    fn every_node_but_the_root_gets_exactly_one_link() {
        let mut map = MindMap::with_root(Node::new("root").with_size(Size::new(120.0, 40.0)));
        let root = map.root();
        for _ in 0..3 {
            let branch =
                map.add_child(root, Node::new("b").with_size(Size::new(100.0, 36.0))).unwrap();
            map.add_child(branch, Node::new("l").with_size(Size::new(90.0, 32.0))).unwrap();
        }
        let layout = map.layout(&LayoutOptions::default());
        let links = layout.connectors(&ConnectorOptions::default());
        assert_eq!(links.len(), map.node_count() - 1);
        assert!(links.iter().all(|l| l.parent != l.child));
        assert!(links.iter().all(|l| layout.get(l.parent).is_some()));

        // Every link starts on its parent's right edge and ends on its child's left.
        for link in &links {
            assert_eq!(link.start().x, layout.rect(link.parent).unwrap().max.x);
            assert_eq!(link.end().x, layout.rect(link.child).unwrap().min.x);
        }
    }

    #[test]
    fn a_collapsed_branch_emits_no_links_into_it() {
        let mut map = MindMap::with_root(Node::new("root"));
        let root = map.root();
        let branch = map.add_child(root, Node::new("b")).unwrap();
        let hidden = map.add_child(branch, Node::new("l")).unwrap();
        map.set_collapsed(branch, true).unwrap();

        let layout = map.layout(&LayoutOptions::default());
        let links = layout.connectors(&ConnectorOptions::default());
        assert_eq!(links.len(), 1);
        assert!(links.iter().all(|l| l.child != hidden));
    }
}
