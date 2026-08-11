//! Regression tests against real ink from the reference board.
//!
//! Every other test in this crate uses a shape someone invented. This one uses the
//! actual bytes: the largest `paint` widget on **Reference Board**, recovered by
//! decoding `captures/reference-board.html` exactly as `docs/02-miro-formats.md`
//! describes — extract `data-meta`, unescape, strip the `(miro-data-v1)` markers,
//! base64-decode, add 197 to every byte, parse as JSON.
//!
//! That board's ink, measured the same way, is what the pipeline was tuned against:
//!
//! | | |
//! |---|---|
//! | `paint` widgets | 219 of 596 objects |
//! | Points, total | 5,376 |
//! | Points per stroke | min 1, median 22, p90 51, **max 128** |
//! | Single-point strokes | **12** |
//! | Strokes with ≤ 2 points | 14 |
//! | Segment length | min 0.17 px, median 3.03 px, p90 7.76 px, max 64 px |
//! | Zero-length segments | 0 |
//! | Distinct `t` values | 6.02 (×141), 8 (×29), 12.65 (×23), 2 (×22), 30, 16, 20.9 |
//!
//! Three of those shaped decisions elsewhere in the crate. Real strokes are **short**
//! — a median of 22 points, never more than 128 — so the per-stroke saving from
//! level-of-detail is modest and the win is in aggregate across 219 of them. Twelve
//! strokes are a single tap, which is why the dot path is a first-class case rather
//! than a guard. And `t` is frequently non-integer (6.0196…), so thickness is carried
//! as `f64` and never rounded.

use vellum_ink::{Lod, Stroke, TessellationOptions};

/// The largest `paint` widget on the reference board: 128 points, `t = 8`. A doodle
/// that doubles back on itself several times, with long perfectly-collinear runs
/// where the pen was dragged straight and tight reversals where it turned — which
/// makes it an unusually complete exercise for one fixture.
#[rustfmt::skip]
const LARGEST_STROKE: &[(f64, f64)] = &[
    (4.0006, 16.1133), (4.0006, 17.0109), (3.9223, 19.3958), (3.4791, 20.7294),
    (3.0359, 21.909), (2.8795, 23.8067), (2.5144, 25.6275), (2.4362, 26.3712),
    (2.4362, 26.961), (2.4362, 27.9868), (2.4362, 29.3716), (2.4362, 30.4743),
    (2.4362, 31.5001), (2.4362, 32.5259), (2.4362, 33.8594), (2.4362, 36.3213),
    (2.4362, 37.6548), (2.5388, 38.9883), (3.0944, 41.4502), (3.556, 42.7837),
    (3.7099, 43.8095), (4.2228, 44.9122), (5.0947, 46.297), (5.6845, 47.3228),
    (6.1974, 47.9126), (6.7873, 48.5024), (7.6592, 49.2974), (8.249, 49.4513),
    (8.685, 49.5282), (8.8388, 49.8872), (9.2748, 49.9642), (9.7877, 49.9642),
    (10.3006, 49.809), (10.8853, 48.4862), (11.6563, 45.3815), (11.805, 43.7483),
    (11.805, 41.99), (11.805, 40.8005), (11.805, 39.7662), (11.7268, 38.7306),
    (11.3618, 37.6902), (11.2054, 36.8098), (10.8404, 36.5764), (10.6057, 36.055),
    (9.7975, 35.1685), (9.1979, 34.5689), (8.6764, 34.1257), (8.2332, 33.9692),
    (8.0768, 33.6042), (7.6335, 33.526), (7.1903, 33.4478), (7.0339, 33.0828),
    (6.4868, 33.3892), (5.455, 35.4921), (4.7101, 37.4667), (4.2412, 38.6464),
    (3.9809, 39.9799), (3.053, 42.4418), (2.4277, 43.7753), (1.9845, 44.8011),
    (1.8024, 46.1346), (1.1356, 48.9042), (0.1821, 51.6738), (0.0, 52.9304),
    (0.0, 53.6741), (0.0, 54.905), (0.0, 55.6487), (0.0, 56.6745), (0.0769, 57.9824),
    (0.5898, 58.803), (1.4617, 60.1109), (1.9746, 61.1366), (2.1285, 61.7265),
    (2.5645, 62.3163), (3.0004, 63.1882), (3.1543, 63.855), (3.5902, 64.7269),
    (4.1031, 65.3167), (4.693, 65.8296), (5.6418, 66.3425), (7.1112, 67.0772),
    (10.4382, 68.7535), (13.0616, 70.0866), (14.6772, 70.6508), (15.78, 70.7277),
    (16.7288, 70.6495), (17.3186, 70.2063), (17.9085, 69.5297), (18.8829, 67.8983),
    (20.0233, 64.9), (21.0456, 61.0426), (22.1873, 57.0506), (24.1174, 51.2904),
    (26.372, 39.0468), (27.6303, 32.9956), (28.6048, 27.0465), (28.9587, 19.6779),
    (29.7845, 14.1725), (29.9614, 12.2496), (29.9614, 10.1882), (29.8832, 7.5579),
    (29.5182, 6.0607), (29.3617, 4.3083), (28.8403, 3.1201), (27.9539, 2.1627),
    (27.3542, 1.5643), (26.8328, 1.1211), (26.1562, 0.9647), (24.9107, 0.5214),
    (24.079, 0.0782), (22.2924, 0.0), (18.8599, 0.1026), (16.283, 1.0942),
    (13.4134, 3.7441), (11.9675, 5.2828), (10.7451, 7.3343), (9.8462, 10.4117),
    (8.6948, 14.0532), (8.1221, 16.2587), (8.0438, 17.5922), (7.9656, 19.4899),
    (7.6006, 21.5414), (7.5224, 23.4391), (7.5224, 24.6957), (7.5224, 26.1575),
    (7.5224, 27.5423), (7.5224, 28.209), (7.5224, 28.7219),
];

/// Miro's `t` for this widget, from its `style` string.
const WIDTH: f64 = 8.0;

fn largest() -> Stroke {
    Stroke::from_miro(LARGEST_STROKE, Some(WIDTH))
}

/// Real ink survives import unchanged: this board has no zero-length segments and no
/// non-finite coordinates, so sanitisation must be a no-op on it. If this ever fails,
/// the sanitiser has started eating real data.
#[test]
fn real_ink_passes_through_import_untouched() {
    let stroke = largest();
    assert_eq!(stroke.len(), LARGEST_STROKE.len());
    assert_eq!(stroke.width(), WIDTH);
    for (point, raw) in stroke.points().iter().zip(LARGEST_STROKE) {
        assert_eq!((point.x, point.y), *raw);
        assert_eq!(point.pressure, 1.0, "Miro carries no pressure");
    }
}

/// Bounds are the point extent grown by exactly half the width, on real geometry as
/// well as on constructed cases.
#[test]
fn real_bounds_are_the_extent_plus_the_nib() {
    let stroke = largest();
    let bounds = stroke.bounds().unwrap();
    let (min_x, max_x) = (0.0 - WIDTH / 2.0, 29.9614 + WIDTH / 2.0);
    let (min_y, max_y) = (0.0 - WIDTH / 2.0, 70.7277 + WIDTH / 2.0);

    assert!((bounds.min_x - min_x).abs() < 1e-9);
    assert!((bounds.max_x - max_x).abs() < 1e-9);
    assert!((bounds.min_y - min_y).abs() < 1e-9);
    assert!((bounds.max_y - max_y).abs() < 1e-9);
    assert!((bounds.width() - 37.9614).abs() < 1e-4);
    assert!((bounds.height() - 78.7277).abs() < 1e-4);
}

/// The pipeline's cost on real ink, across the zoom range Vellum allows. Pinned as
/// ranges so a lyon patch release can move a join without failing the build, but
/// tight enough that a regression in any stage shows up here.
#[test]
fn real_ink_has_the_expected_cost_at_every_zoom() {
    let stroke = largest();
    for (zoom, low, high) in [(0.01, 4, 14), (0.1, 30, 60), (1.0, 120, 200), (16.0, 850, 1200)] {
        let mesh = stroke.render(Lod::new(zoom)).unwrap();
        assert!(mesh.is_finite() && mesh.is_well_formed(), "zoom {zoom}");
        let n = mesh.vertex_count();
        assert!((low..=high).contains(&n), "zoom {zoom}: {n} vertices, expected {low}..{high}");
    }
}

/// The long collinear runs in this stroke — the pen dragged straight down at a fixed
/// x — are exactly what simplification exists to collapse. Real ink turns out to be
/// far more redundant than a synthetic curve: 17 of the 128 points are *exactly*
/// collinear with their neighbours and carry no information at all, and a
/// quarter-pixel budget removes well over half.
#[test]
fn real_collinear_runs_collapse() {
    let stroke = largest();
    assert!(
        stroke.simplified(0.25).len() < stroke.len() / 2,
        "a stroke this redundant should more than halve at a quarter-pixel tolerance"
    );
    // Even an unusably small budget still drops the exactly-collinear points, since
    // their distance from the retained line is zero rather than merely small.
    let lossless = stroke.simplified(1e-12);
    assert_eq!(lossless.len(), stroke.len() - 17);
    for p in stroke.points() {
        let distance = distance_to_polyline((p.x, p.y), lossless.points());
        assert!(distance < 1e-9, "point {p:?} moved {distance} at a zero budget");
    }
}

/// Distance from a point to a polyline. Written here rather than borrowed from the
/// crate, so that the check is genuinely independent of the code it is checking.
fn distance_to_polyline(p: (f64, f64), points: &[vellum_ink::StrokePoint]) -> f64 {
    points
        .windows(2)
        .map(|w| {
            let (dx, dy) = (w[1].x - w[0].x, w[1].y - w[0].y);
            let length_squared = dx * dx + dy * dy;
            let t = if length_squared > 0.0 {
                (((p.0 - w[0].x) * dx + (p.1 - w[0].y) * dy) / length_squared).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (p.0 - (w[0].x + dx * t)).hypot(p.1 - (w[0].y + dy * t))
        })
        .fold(f64::INFINITY, f64::min)
}

/// Selection on real ink: a point on the stroke hits, a point a nib's width off it
/// does not, and the whole thing survives being cut.
#[test]
fn real_ink_can_be_selected_split_and_erased() {
    let stroke = largest();
    let midpoint = stroke.points()[64];

    assert!(stroke.hit_test((midpoint.x, midpoint.y), 0.0));
    assert!(!stroke.hit_test((midpoint.x - 12.0, midpoint.y), 0.0));

    let (head, tail) = stroke.split_at((midpoint.x, midpoint.y), 0.0).expect("split missed");
    assert_eq!(head.len() + tail.len(), stroke.len() + 1, "the cut point belongs to both");
    assert!((head.length() + tail.length() - stroke.length()).abs() < 1e-9);

    let pieces = stroke.erase((midpoint.x, midpoint.y), 6.0);
    assert!(!pieces.is_empty(), "a small eraser should not delete the whole stroke");
    assert!(pieces.iter().map(Stroke::length).sum::<f64>() < stroke.length());
    for piece in &pieces {
        for p in piece.points() {
            let d = (p.x - midpoint.x).hypot(p.y - midpoint.y);
            assert!(d >= 6.0 - 1e-9, "erased region survived at {p:?}");
        }
    }
}

/// A `t` of 6.0196… is the most common thickness on the reference board. Rounding it
/// would be invisible on one stroke and wrong across 141 of them, so it has to
/// survive as an `f64` all the way to the mesh.
#[test]
fn a_non_integer_miro_thickness_survives_to_the_mesh() {
    let t = 6.019_630_853_383_819;
    let stroke = Stroke::from_miro(&[(0.0, 0.0), (100.0, 0.0)], Some(t));
    assert_eq!(stroke.width(), t);

    let mesh = stroke.tessellate(&TessellationOptions::with_tolerance(0.01)).unwrap();
    let half = mesh.positions.iter().map(|p| p[1].abs()).fold(0.0_f32, f32::max);
    assert!((f64::from(half) - t / 2.0).abs() < 1e-3, "half-width came out {half}");
}
