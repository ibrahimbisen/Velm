//! The radial layout: the root at the centre, each level a ring around it.
//!
//! The trick is that this is **not** a second layout algorithm. It is the same tidy
//! pass, run in polar coordinates: the breadth axis is the *angle*, the depth axis
//! is the *radius*. [`crate::tidy`] never learns about any of it, because its one
//! requirement — that two nodes it is asked to separate are at the same depth — is
//! exactly what makes an angle a legal unit here. Within one ring the radius is a
//! constant, so an arc length converts to an angle by a single division.
//!
//! # Node extents: why the diagonal
//!
//! A node's box stays axis-aligned (labels stay horizontal), so how much of the ring
//! it occupies depends on the angle it ends up at — which is not known until the
//! layout has run. Rather than iterate towards a fixed point, every node is treated
//! as a disc of its own **diagonal**: a box of any aspect at any angle fits inside
//! that circle. The cost is that a radial map of wide, flat labels is looser than it
//! strictly needs to be near the top and bottom of each ring. That is the honest
//! trade and it is taken deliberately, because the alternative is a layout whose
//! non-overlap property depends on a fixed point converging.
//!
//! # Why `arcsin`, and why that makes the guarantee exact
//!
//! Separating two nodes by the required *arc* is not enough: the distance that
//! decides whether two boxes overlap is the **chord**, and the chord is shorter than
//! the arc. On a crowded outer ring the difference is a fraction of a percent; on an
//! inner ring holding three big nodes it is tens of percent, which is a real overlap
//! rather than a rounding concern.
//!
//! So a node of diagonal `d` on a ring of radius `r` is given the angular width
//!
//! ```text
//! w = 2·asin(d / 2r)
//! ```
//!
//! — the angle whose chord is exactly `d`. Two nodes then get, from the tidy pass,
//! an angular separation of at least `w₁/2 + w₂/2 = asin(x₁) + asin(x₂)` where
//! `xᵢ = dᵢ/2r`. `asin` is convex on `[0, 1]`, so that is at least
//! `2·asin((x₁+x₂)/2)`, whose chord is `2r·(x₁+x₂)/2 = (d₁+d₂)/2` — precisely the
//! sum of the two nodes' circumscribed radii. **Two nodes on one ring therefore
//! cannot overlap**, and the argument holds for every pair, not only adjacent ones:
//!
//! - Between adjacent nodes the tidy pass guarantees the separation directly.
//! - Between non-adjacent nodes on a ring the separations sum, and since the
//!   requirement is `w₁/2 + w₂/2` — additive in half-widths — a sum of two or more
//!   adjacent requirements always covers it.
//! - Going the *short* way round, the worst case is a pair straddling the seam of a
//!   full circle. The fit below keeps the total angular extent within the sweep, and
//!   that constraint is exactly what leaves at least `w_first/2 + w_last/2` of
//!   unused angle at the seam.
//!
//! Rings never collide with each other because [`ring_radii`] separates consecutive
//! radii by half of each ring's thickest diagonal plus the level gap.
//!
//! # Fitting the circle in a fixed number of passes
//!
//! A map can want more than `2π` of angle. Scaling every radius by `k` shrinks every
//! angular width, because `asin` is convex through the origin and therefore
//! star-shaped: `asin(x/k) ≤ asin(x)/k` for `k ≥ 1`. The gap terms are exactly
//! `1/k`. So every extent at scale `k` is at most its value at scale 1 divided by
//! `k`, and since the tidy pass is homogeneous and monotone in its extents — both
//! properties are tested in [`crate::tidy`] — the resulting angular extent is at
//! most `E/k`. Choosing `k = E / sweep` therefore fits in **one** rescale, and the
//! implementation runs at most two tidy passes regardless of the tree.

use std::f64::consts::TAU;

use crate::geometry::{EPSILON, Point, Rect};
use crate::layout::visible::Visible;
use crate::layout::{Layout, LayoutKind, LayoutOptions, Placement, Side};
use crate::tidy::{Gaps, tidy};
use crate::tree::MindMap;

/// One pass's worth of angular geometry.
struct Fan {
    /// Angle of each node's centre, before the whole fan is rotated to `start_angle`.
    angle: Vec<f64>,
    /// The leading edge of the fan: `min(angle − width/2)`.
    lower: f64,
    /// Total angle occupied, edge to edge.
    extent: f64,
}

pub(crate) fn radial(
    map: &MindMap,
    options: &LayoutOptions,
    start_angle: f64,
    sweep: f64,
) -> Layout {
    let visible = Visible::whole(map);
    // A sweep that is not a usable angle means "all the way round" rather than an
    // error: a mind map with a nonsense sweep should still draw.
    let sweep = if sweep.is_finite() && sweep > EPSILON { sweep.min(TAU) } else { TAU };

    let mut radius = ring_radii(&visible, options);
    let mut fan = fan_out(&visible, options, &radius);
    if fan.extent > sweep {
        // See the module docs: one rescale is provably enough.
        let scale = fan.extent / sweep;
        for r in &mut radius {
            *r *= scale;
        }
        fan = fan_out(&visible, options, &radius);
    }

    let placements = (0..visible.len())
        .map(|i| {
            let depth = visible.depth[i];
            let size = visible.size[i];
            let centre = if depth == 0 {
                options.origin
            } else {
                let theta = start_angle + (fan.angle[i] - fan.lower);
                let r = radius[depth as usize];
                Point::new(
                    options.origin.x + r * theta.cos(),
                    options.origin.y + r * theta.sin(),
                )
            };
            Placement {
                node: visible.ids[i],
                parent: visible.parent_id(i),
                rect: Rect::from_centre(centre, size),
                depth,
                side: if depth == 0 { Side::Centre } else { Side::Primary },
            }
        })
        .collect();

    Layout::new(LayoutKind::Radial { start_angle, sweep }, placements)
}

/// The centre radius of each ring.
///
/// Consecutive rings are separated by half of each one's thickest diagonal plus the
/// level gap, which is what keeps a node on ring `d` clear of every node on rings
/// `d ± 1` whatever angles they land at. The root is at radius zero, so ring 1
/// already clears the root's own box.
fn ring_radii(visible: &Visible, options: &LayoutOptions) -> Vec<f64> {
    let thickness = visible.max_per_depth(|s| s.diagonal());
    let mut radius = vec![0.0; thickness.len()];
    for d in 1..thickness.len() {
        radius[d] = radius[d - 1] + 0.5 * (thickness[d - 1] + thickness[d]) + options.level_gap;
    }
    radius
}

/// Run the tidy pass in angle for one set of ring radii.
fn fan_out(visible: &Visible, options: &LayoutOptions, radius: &[f64]) -> Fan {
    let width: Vec<f64> = (0..visible.len())
        .map(|i| {
            let depth = visible.depth[i] as usize;
            if depth == 0 {
                // The root is at the centre; it has no angular position to defend,
                // and its physical size is accounted for by ring 1's radius.
                return 0.0;
            }
            // `radius[depth] ≥ thickness[depth]/2 ≥ diagonal/2`, so the ratio is
            // never above 1 and the `asin` is always defined. The clamp is there for
            // the degenerate all-zero-size map, where the ratio is `0/0`.
            let half_chord = visible.size[i].diagonal() / (2.0 * radius[depth].max(EPSILON));
            2.0 * half_chord.clamp(0.0, 1.0).asin()
        })
        .collect();

    // Gaps are given in world px and converted per ring, which is the whole reason
    // `Gaps` is indexed by depth.
    let angular = |gap: f64| -> Vec<f64> {
        radius
            .iter()
            .enumerate()
            .map(|(d, &r)| if d == 0 { 0.0 } else { gap / r.max(EPSILON) })
            .collect()
    };
    let gaps = Gaps {
        sibling: angular(options.sibling_gap),
        subtree: angular(options.subtree_gap),
    };

    let angle = tidy(&visible.tidy_tree(width.clone()), &gaps);
    let (lower, upper) = angle.iter().zip(&width).fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(lo, hi), (&a, &w)| (lo.min(a - 0.5 * w), hi.max(a + 0.5 * w)),
    );
    let extent = if upper >= lower { upper - lower } else { 0.0 };
    Fan { angle, lower, extent }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::layout::LayoutOptions;
    use crate::tree::{MindMap, Node, NodeId};

    fn node(w: f64, h: f64) -> Node {
        Node::new("n").with_size(Size::new(w, h))
    }

    /// A root with `branches` children, each carrying `leaves` leaves.
    fn bushy(branches: usize, leaves: usize, size: Size) -> (MindMap, Vec<NodeId>) {
        let mut map = MindMap::with_root(Node::new("root").with_size(size));
        let root = map.root();
        let mut ids = vec![root];
        for _ in 0..branches {
            let b = map.add_child(root, Node::new("b").with_size(size)).unwrap();
            ids.push(b);
            for _ in 0..leaves {
                ids.push(map.add_child(b, Node::new("l").with_size(size)).unwrap());
            }
        }
        (map, ids)
    }

    fn options() -> LayoutOptions {
        LayoutOptions {
            kind: LayoutKind::Radial { start_angle: 0.0, sweep: TAU },
            ..LayoutOptions::default()
        }
    }

    /// The property the module docs prove: on one ring, the distance between two
    /// centres is at least the sum of the two circumscribed radii.
    fn assert_rings_are_chord_separated(layout: &Layout) {
        let places = layout.placements();
        for (i, a) in places.iter().enumerate() {
            for b in &places[i + 1..] {
                if a.depth != b.depth {
                    continue;
                }
                let required = 0.5 * (a.rect.size().diagonal() + b.rect.size().diagonal());
                let actual = a.rect.centre().distance_to(b.rect.centre());
                assert!(
                    actual >= required - 1e-6,
                    "depth {} centres are {actual} apart, need {required}",
                    a.depth
                );
            }
        }
    }

    #[test]
    fn the_root_is_at_the_centre_and_every_level_is_a_ring() {
        let (map, ids) = bushy(5, 3, Size::new(120.0, 40.0));
        let layout = radial(&map, &options(), 0.0, TAU);
        assert_eq!(layout.rect(ids[0]).unwrap().centre(), Point::ORIGIN);

        let mut radii: Vec<(u32, f64)> = layout
            .placements()
            .iter()
            .map(|p| (p.depth, p.rect.centre().distance_to(Point::ORIGIN)))
            .collect();
        radii.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        // Every node at one depth shares a radius, and radii increase with depth.
        for window in radii.windows(2) {
            let ((d0, r0), (d1, r1)) = (window[0], window[1]);
            if d0 == d1 {
                assert!((r0 - r1).abs() < 1e-6, "depth {d0} is not a ring: {r0} vs {r1}");
            } else {
                assert!(r1 > r0, "depth {d1} is not outside depth {d0}");
            }
        }
    }

    #[test]
    fn nodes_on_a_ring_are_chord_separated_even_when_the_ring_is_sparse() {
        // Three big nodes on the innermost ring is the case an arc-length separation
        // gets wrong: 120° apart, the chord is 83% of the arc.
        let (map, _) = bushy(3, 0, Size::new(200.0, 120.0));
        let layout = radial(&map, &options(), 0.0, TAU);
        assert_rings_are_chord_separated(&layout);
    }

    #[test]
    fn a_crowded_map_is_pushed_outwards_until_it_fits_the_circle() {
        let big = Size::new(200.0, 80.0);
        let (map, ids) = bushy(12, 6, big);
        let layout = radial(&map, &options(), 0.0, TAU);
        assert_rings_are_chord_separated(&layout);

        // Every angle really did land inside one turn, which is what the seam
        // argument in the module docs depends on.
        let angles: Vec<f64> = layout
            .placements()
            .iter()
            .filter(|p| p.depth > 0)
            .map(|p| {
                let c = p.rect.centre();
                c.y.atan2(c.x).rem_euclid(TAU)
            })
            .collect();
        assert!(angles.iter().all(|a| (0.0..TAU).contains(a)));

        // ...and the outer ring is well clear of the inner one.
        let ring1 = layout
            .placements()
            .iter()
            .find(|p| p.depth == 1)
            .map(|p| p.rect.centre().distance_to(Point::ORIGIN))
            .unwrap();
        let ring2 = layout
            .placements()
            .iter()
            .find(|p| p.depth == 2)
            .map(|p| p.rect.centre().distance_to(Point::ORIGIN))
            .unwrap();
        assert!(ring2 > ring1 + 0.5 * big.diagonal(), "{ring2} is not clear of {ring1}");
        assert_eq!(ids.len(), layout.len());
    }

    #[test]
    fn a_partial_sweep_fans_from_the_start_angle() {
        let (map, _) = bushy(4, 2, Size::new(100.0, 40.0));
        let start = 0.75;
        let sweep = TAU / 4.0;
        let layout = radial(&map, &options(), start, sweep);
        assert_rings_are_chord_separated(&layout);

        for placement in layout.placements().iter().filter(|p| p.depth > 0) {
            let c = placement.rect.centre();
            let theta = c.y.atan2(c.x);
            assert!(
                theta >= start - 1e-6 && theta <= start + sweep + 1e-6,
                "{theta} escaped the {sweep} sweep from {start}"
            );
        }
    }

    #[test]
    fn a_nonsense_sweep_draws_a_whole_circle_rather_than_failing() {
        let (map, _) = bushy(6, 2, Size::new(100.0, 40.0));
        for sweep in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let layout = radial(&map, &options(), 0.0, sweep);
            assert!(layout.placements().iter().all(|p| p.rect.centre().is_finite()));
            assert_rings_are_chord_separated(&layout);
        }
    }

    #[test]
    fn a_lone_root_is_a_single_point_at_the_origin() {
        let map = MindMap::with_root(node(100.0, 40.0));
        let layout = radial(&map, &options(), 0.0, TAU);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.bounds().unwrap(), Rect::from_centre(Point::ORIGIN, Size::new(100.0, 40.0)));
    }

    #[test]
    fn zero_sized_nodes_produce_finite_positions() {
        // The degenerate map: no measurements have arrived yet, so every box is
        // empty and every ratio in `fan_out` is 0/0 without the clamp.
        let mut map = MindMap::with_root(node(0.0, 0.0));
        let root = map.root();
        for _ in 0..4 {
            let b = map.add_child(root, node(0.0, 0.0)).unwrap();
            map.add_child(b, node(0.0, 0.0)).unwrap();
        }
        let layout = radial(&map, &options(), 0.0, TAU);
        assert!(layout.placements().iter().all(|p| p.rect.centre().is_finite()));
    }
}
