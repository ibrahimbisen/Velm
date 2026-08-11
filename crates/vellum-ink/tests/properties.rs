//! Randomised property tests over the whole pipeline.
//!
//! Unit tests pin the cases someone thought of. These pin the ones nobody did, which
//! for ink is the interesting half: the failure modes here are a zero-length segment
//! producing a `0/0` normal, a near-180° hairpin, a duplicated point at a corner, and
//! a stroke long enough to matter — all things a random generator finds and a
//! hand-written example does not.
//!
//! The generator is a 15-line SplitMix64 rather than `proptest` or `quickcheck`. The
//! decisive reason is reproducibility: each case is a pure function of a seed, and a
//! failure prints the seed, so a failing case is reproduced by running the same test
//! rather than by reconstructing a shrunk value from a report. It also keeps a
//! geometry crate free of a test-only dependency tree. What is given up is automatic
//! shrinking, which matters less here than usual because the seed regenerates the
//! exact stroke and the shapes are already small.
//!
//! The two properties the brief names — no NaN in the tessellated output, and no
//! point moved further than the simplification tolerance — are
//! [`tessellation_never_produces_a_non_finite_vertex`] and
//! [`simplification_never_moves_a_point_further_than_the_tolerance`]. The rest are
//! the invariants the rest of the app will lean on.

use vellum_ink::{Lod, SmoothOptions, Stroke, StrokePoint, TessellationOptions};

/// Cases per property. High enough to reach the awkward shapes, low enough that the
/// whole file runs in well under a second in a debug build.
const CASES: u64 = 400;

// ---------------------------------------------------------------------------
// Generator
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_F00D)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// A random stroke drawn from the shape families that break things: degenerate
/// lengths, jitter, sharp corners, self-intersection, duplicates and sheer size.
fn arbitrary_stroke(rng: &mut Rng) -> Stroke {
    let width = rng.range(0.5, 24.0);
    let mut points: Vec<StrokePoint> = match rng.below(8) {
        // The degenerate lengths, kept common because they are where the real bugs
        // live and real Miro data contains them.
        0 => Vec::new(),
        1 => vec![StrokePoint::new(rng.range(-500.0, 500.0), rng.range(-500.0, 500.0))],
        2 => (0..2)
            .map(|_| StrokePoint::new(rng.range(-500.0, 500.0), rng.range(-500.0, 500.0)))
            .collect(),
        // A jittery random walk: what a slow, deliberate pen stroke looks like.
        3 => {
            let n = 3 + rng.below(200);
            let (mut x, mut y) = (0.0, 0.0);
            (0..n)
                .map(|_| {
                    x += rng.range(-8.0, 8.0);
                    y += rng.range(-8.0, 8.0);
                    StrokePoint::new(x, y)
                })
                .collect()
        }
        // A smooth arc, sampled sparsely or densely at random.
        4 => {
            let n = 3 + rng.below(150);
            let radius = rng.range(5.0, 800.0);
            let sweep = rng.range(0.2, 6.5);
            (0..n)
                .map(|i| {
                    let a = sweep * i as f64 / (n - 1).max(1) as f64;
                    StrokePoint::new(radius * a.cos(), radius * a.sin())
                })
                .collect()
        }
        // A polygon: deliberately sharp corners, sometimes a hairpin.
        5 => {
            let n = 3 + rng.below(20);
            (0..n)
                .map(|_| StrokePoint::new(rng.range(-300.0, 300.0), rng.range(-300.0, 300.0)))
                .collect()
        }
        // A figure-eight — self-intersecting, which handwriting is constantly.
        6 => {
            let n = 8 + rng.below(120);
            (0..n)
                .map(|i| {
                    let t = std::f64::consts::TAU * i as f64 / (n - 1) as f64;
                    StrokePoint::new(200.0 * t.sin(), 200.0 * (2.0 * t).sin())
                })
                .collect()
        }
        // Large: past the point where an O(n²) mistake or a u16 index would show.
        _ => {
            let n = 1_000 + rng.below(4_000);
            (0..n)
                .map(|i| {
                    let t = i as f64 * 0.01;
                    StrokePoint::new(t * 12.0 + (t * 3.0).sin() * 40.0, (t * 1.7).cos() * 300.0)
                })
                .collect()
        }
    };

    // Duplicate a point sometimes: the zero-length-segment case, which is the one
    // that turns into a NaN normal if it ever reaches the tessellator.
    if !points.is_empty() && rng.unit() < 0.25 {
        let i = rng.below(points.len());
        points.insert(i, points[i]);
    }
    // And vary pressure sometimes, to exercise the variable-width path.
    if rng.unit() < 0.3 {
        for p in &mut points {
            p.pressure = rng.range(0.05, 3.0);
        }
    }

    Stroke::new(points, width)
}

fn arbitrary_lod(rng: &mut Rng) -> Lod {
    // Spanning vellum-scene's whole 0.01..=64 zoom range.
    Lod::with_screen_tolerance(rng.range(0.01, 64.0), rng.range(0.05, 2.0))
}

// ---------------------------------------------------------------------------
// Independent geometry, written separately from the implementation on purpose
// ---------------------------------------------------------------------------

fn distance_to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_squared = dx * dx + dy * dy;
    let t = if length_squared > 0.0 {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length_squared).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p.0 - (a.0 + dx * t)).hypot(p.1 - (a.1 + dy * t))
}

fn distance_to_polyline(p: (f64, f64), points: &[StrokePoint]) -> f64 {
    match points {
        [] => f64::INFINITY,
        [only] => (p.0 - only.x).hypot(p.1 - only.y),
        _ => points
            .windows(2)
            .map(|w| distance_to_segment(p, (w[0].x, w[0].y), (w[1].x, w[1].y)))
            .fold(f64::INFINITY, f64::min),
    }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// No NaN, no infinity, no dangling index — at any zoom, for any stroke.
///
/// A non-finite vertex does not crash. It rasterises as a triangle stretched across
/// the entire viewport, and finding which of 219 strokes produced it from a
/// screenshot is a bad afternoon.
#[test]
fn tessellation_never_produces_a_non_finite_vertex() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        let lod = arbitrary_lod(&mut rng);

        let mesh = stroke
            .render(lod)
            .unwrap_or_else(|e| panic!("seed {seed}: render failed: {e}"));

        assert!(mesh.is_finite(), "seed {seed}: non-finite vertex, lod {lod:?}");
        assert!(mesh.is_well_formed(), "seed {seed}: malformed index buffer");
        assert_eq!(mesh.indices.len() % 3, 0, "seed {seed}: ragged triangle list");

        // Also directly, skipping smoothing, so a failure localises to one stage.
        let raw = stroke
            .tessellate(&TessellationOptions::with_tolerance(lod.world_tolerance()))
            .unwrap_or_else(|e| panic!("seed {seed}: tessellation failed: {e}"));
        assert!(raw.is_finite() && raw.is_well_formed(), "seed {seed}: raw mesh unsound");
    }
}

/// RDP's contract: every point it discards is within the tolerance of what remains.
#[test]
fn simplification_never_moves_a_point_further_than_the_tolerance() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        let tolerance = rng.range(0.001, 50.0);
        let simplified = stroke.simplified(tolerance);

        for p in stroke.points() {
            let distance = distance_to_polyline((p.x, p.y), simplified.points());
            assert!(
                distance <= tolerance + 1e-6,
                "seed {seed}: tolerance {tolerance}, point {p:?} left {distance} away"
            );
        }
    }
}

/// Simplification only ever removes. It must never invent a point, move one, or
/// reorder them — an interpolated "simplified" point would silently change a shape
/// the user drew.
#[test]
fn simplification_is_a_subsequence_of_its_input() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        let simplified = stroke.simplified(rng.range(0.001, 50.0));

        let mut original = stroke.points().iter();
        for kept in simplified.points() {
            assert!(
                original.any(|p| p == kept),
                "seed {seed}: {kept:?} is not an input point, or is out of order"
            );
        }
        assert!(simplified.len() <= stroke.len(), "seed {seed}: simplification grew the stroke");
        if stroke.len() >= 2 {
            assert_eq!(simplified.points().first(), stroke.points().first(), "seed {seed}");
            assert_eq!(simplified.points().last(), stroke.points().last(), "seed {seed}");
        }
    }
}

/// Catmull-Rom interpolates, so every input point is still on the curve. Losing this
/// would mean rendering the user's stroke somewhere they did not draw it.
#[test]
fn smoothing_preserves_every_input_point_in_order() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        let smoothed = stroke.smoothed(&SmoothOptions::with_tolerance(rng.range(0.01, 4.0)));

        let mut produced = smoothed.points().iter();
        for p in stroke.points() {
            assert!(
                produced.any(|q| q.x == p.x && q.y == p.y),
                "seed {seed}: input point {p:?} is not on the smoothed curve"
            );
        }
        assert!(smoothed.points().iter().all(|p| p.x.is_finite() && p.y.is_finite()));
    }
}

/// Bounds are claimed to be exact for the ink itself, so they must contain both the
/// centreline grown by its width and every triangle lyon emits for it.
#[test]
fn bounds_contain_the_stroke_and_its_mesh() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        let Some(bounds) = stroke.bounds() else {
            assert!(stroke.is_empty(), "seed {seed}: a non-empty stroke reported no bounds");
            continue;
        };

        for p in stroke.points() {
            let r = stroke.width() * p.pressure / 2.0;
            assert!(
                bounds.contains((p.x - r, p.y - r)) && bounds.contains((p.x + r, p.y + r)),
                "seed {seed}: {p:?} with radius {r} is not inside {bounds:?}"
            );
        }

        let mesh = stroke.tessellate(&TessellationOptions::default()).unwrap();
        // Slack for the f64 -> f32 cast the mesh goes through, scaled to the
        // magnitude of the coordinates involved.
        let slack = 1e-3 + 1e-5 * bounds.width().max(bounds.height());
        for v in &mesh.positions {
            let (x, y) = (v[0] as f64, v[1] as f64);
            assert!(
                x >= bounds.min_x - slack
                    && x <= bounds.max_x + slack
                    && y >= bounds.min_y - slack
                    && y <= bounds.max_y + slack,
                "seed {seed}: vertex {v:?} escapes {bounds:?}"
            );
        }
    }
}

/// The signed distance is the basis of selection and erasing, so it is checked
/// against an independently written centreline distance: outside the ink it must
/// equal `centreline distance − half width` for a constant-width stroke, and its
/// sign must always be right.
#[test]
fn signed_distance_agrees_with_an_independent_centreline_distance() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        if stroke.is_empty() || stroke.pressure_varies() {
            continue;
        }
        let half_width = stroke.max_half_width();

        for _ in 0..8 {
            let q = (rng.range(-900.0, 900.0), rng.range(-900.0, 900.0));
            let signed = stroke.signed_distance(q).unwrap();
            let expected = distance_to_polyline(q, stroke.points()) - half_width;
            assert!(
                (signed - expected).abs() < 1e-6,
                "seed {seed}: at {q:?} got {signed}, expected {expected}"
            );
            assert_eq!(
                stroke.hit_test(q, 0.0),
                signed <= 0.0,
                "seed {seed}: hit_test disagrees with its own distance at {q:?}"
            );
        }
    }
}

/// Erasing must remove what it covers and keep what it does not: no surviving point
/// inside the eraser, and no ink invented outside the original stroke.
#[test]
fn erasing_removes_exactly_what_the_eraser_covers() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        if stroke.len() < 2 {
            continue;
        }

        // Aim the eraser at the stroke often enough for the interesting cases to
        // happen, rather than at empty space.
        let anchor = stroke.points()[rng.below(stroke.len())];
        let center = (anchor.x + rng.range(-40.0, 40.0), anchor.y + rng.range(-40.0, 40.0));
        let radius = rng.range(0.5, 120.0);

        let pieces = stroke.erase(center, radius);
        // "Still on the original centreline" is O(n) per point, so it is checked
        // exhaustively only on the smaller strokes. Nothing about the algorithm is
        // length-dependent, and the large strokes still get the cheap check.
        let check_provenance = stroke.len() <= 300;

        for piece in &pieces {
            assert!(piece.len() >= 2, "seed {seed}: a one-point fragment survived");
            for p in piece.points() {
                let d = (p.x - center.0).hypot(p.y - center.1);
                assert!(
                    d >= radius - 1e-6,
                    "seed {seed}: point {p:?} is {d} from the eraser of radius {radius}"
                );
                if check_provenance {
                    // Nothing was invented: every surviving point is still on the
                    // original centreline.
                    let off = distance_to_polyline((p.x, p.y), stroke.points());
                    assert!(off < 1e-6, "seed {seed}: erased result strayed {off} off the stroke");
                }
            }
        }
        let kept: f64 = pieces.iter().map(|p| p.length()).sum();
        assert!(
            kept <= stroke.length() + 1e-6,
            "seed {seed}: erasing produced more ink than it started with"
        );
    }
}

/// A split is a cut, not an edit: the two halves meet exactly, and together they are
/// still the same length of line as before.
#[test]
fn splitting_conserves_the_stroke() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        if stroke.len() < 3 {
            continue;
        }

        let anchor = stroke.points()[1 + rng.below(stroke.len() - 2)];
        let Some((head, tail)) = stroke.split_at((anchor.x, anchor.y), 1.0) else {
            continue;
        };

        assert_eq!(
            head.points().last(),
            tail.points().first(),
            "seed {seed}: the halves do not meet"
        );
        assert_eq!(head.width(), stroke.width(), "seed {seed}: the cut changed the width");
        let combined = head.length() + tail.length();
        assert!(
            (combined - stroke.length()).abs() < 1e-6,
            "seed {seed}: {combined} of line after cutting {}",
            stroke.length()
        );
    }
}

/// The whole point of the LOD stage: the zoomed-out view that has to draw every
/// stroke on the board never costs more than the zoomed-in view that draws one.
///
/// This is deliberately stated between the *extremes* rather than step by step,
/// because step-by-step monotonicity is not true and asserting it would be a lie the
/// test suite told. Two independent reasons: RDP's output is not nested across
/// tolerances, so a tighter tolerance can retain a different set of points rather
/// than a superset; and the adaptive sample count is computed from the geometry that
/// *results* from simplification, so retaining more points can shorten and flatten
/// every segment and end up adding fewer samples overall. Measured over 800 random
/// strokes, one zoom step can genuinely cut the vertex count to a third — always in
/// the direction of a closer view being cheaper, never of a distant one being
/// expensive. Across the full 0.01 to 64 range there were no inversions at all.
#[test]
fn the_most_distant_view_is_never_the_most_expensive() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let stroke = arbitrary_stroke(&mut rng);
        let distant = stroke.render(Lod::new(0.01)).unwrap().vertex_count();
        let close = stroke.render(Lod::new(64.0)).unwrap().vertex_count();
        assert!(
            distant <= close,
            "seed {seed}: {distant} vertices zoomed out against {close} zoomed in"
        );
    }
}
