//! Freehand ink: everything between Miro's raw `points` array and a triangle mesh.
//!
//! Ink is the largest thing on a real Miro board and the only thing Miro will not
//! give you. On the reference board — Reference Board, `docs/02-miro-formats.md` —
//! **219 of 596 widgets are `paint`**, more than stickies, images and text combined,
//! and no Miro REST v2 endpoint and no Web SDK method exposes a drawing at all. The
//! clipboard decoder is the only route to them. That makes the quality of what this
//! crate renders the difference between an import that looks like the user's board
//! and one that obviously is not.
//!
//! Miro gives us very little to work with: a point array relative to the widget's
//! position, a colour, an opacity and a thickness. No pressure, no timestamps, no
//! curve fitting — just where the pen was, sampled at whatever rate the browser
//! managed. Everything that makes a stroke look drawn rather than plotted has to be
//! reconstructed here.
//!
//! # The pipeline
//!
//! ```text
//!   raw points ─► simplify ─► smooth ─► tessellate ─► indexed triangles
//!                    │           │           │
//!                    └── all three driven by one world-space tolerance,
//!                        derived from the camera's zoom (`Lod`)
//! ```
//!
//! [`Stroke::render`] runs all three. Each stage is public and independently usable,
//! because they have separate lifetimes in a real app: bounds and hit-testing want
//! the raw stroke, an export wants a fixed fine tolerance, and the renderer wants
//! whatever the current zoom implies.
//!
//! **Simplify comes before smooth.** The other order works and is tempting — smooth
//! the raw input, then thin the result — but it pays for samples the view cannot
//! resolve and then throws them away. It also feeds corner detection raw, jittery
//! input, where [Ramer-Douglas-Peucker](mod@simplify) has already reduced the stroke
//! to exactly its geometrically significant vertices. The cost is that the spline is
//! fitted through slightly different points at different zooms, so a stroke's shape
//! shifts imperceptibly as the LOD changes; a cached mesh per zoom band avoids
//! re-tessellating and hides it entirely.
//!
//! What that buys, measured on **all 219 `paint` widgets of the reference board**
//! (5,376 raw points) at the default [`Lod::BALANCED`] half-pixel budget:
//!
//! | zoom | world tolerance | points after LOD | vertices | triangles |
//! |---|---|---|---|---|
//! | 0.01 | 50 px     | 468   | 1,010   | 548     |
//! | 0.1  | 5 px      | 927   | 3,149   | 2,683   |
//! | 0.25 | 2 px      | 1,337 | 5,626   | 5,126   |
//! | 1.0  | 0.5 px    | 2,446 | 14,162  | 13,636  |
//! | 4.0  | 0.125 px  | 3,913 | 31,455  | 30,919  |
//! | 16.0 | 0.031 px  | 4,692 | 57,527  | 57,007  |
//! | 64.0 | 0.0078 px | 4,894 | 103,297 | 102,797 |
//!
//! Zoomed out to fit the board — the one view that genuinely has to draw all 219
//! strokes — the entire board's ink is 1,010 vertices, against 30,207 for
//! tessellating the same strokes at a fixed fine tolerance. Preparing all 219 from
//! raw points takes 1.2 ms at zoom 1 and 0.6 ms zoomed out, in a release build on an
//! M-series laptop, which is comfortably inside a frame even without caching.
//!
//! The cost is also bounded by the strokes' *shape* rather than by how they were
//! sampled: the same synthetic arc captured at 5,000, 20,000 or 50,000 points all
//! render to 199 vertices at zoom 1, because simplification removes exactly the
//! samples the view cannot resolve. Real Miro strokes are short — a median of 22
//! points and never more than 128 on this board — so that headroom is insurance
//! against a stylus that samples faster than a browser does, not something the
//! current importer needs.
//!
//! # What this crate does not do
//!
//! No GPU code, no colour, no opacity, no transform. [`Mesh`] is positions and
//! indices in the stroke's own space; the renderer places it, colours it and
//! uploads it. That boundary is what keeps every stage here testable without a
//! graphics stack — the tests in this crate exercise the full pipeline on a machine
//! with no GPU at all.
//!
//! # Example
//!
//! ```
//! use vellum_ink::{Lod, Stroke};
//!
//! // Exactly the shape `vellum_import`'s `Ink` carries: points and Miro's `t`.
//! let stroke = Stroke::from_miro(&[(0.0, 0.0), (40.0, 30.0), (90.0, 10.0)], Some(6.0));
//!
//! let bounds = stroke.bounds().expect("a stroke with points has bounds");
//! assert_eq!((bounds.min_x, bounds.min_y), (-3.0, -3.0));
//! assert!(stroke.hit_test((40.0, 31.0), 0.0));
//!
//! let mesh = stroke.render(Lod::new(1.0)).unwrap();
//! assert!(mesh.triangle_count() > 0 && mesh.is_finite());
//! ```

pub mod hit;
pub mod simplify;
pub mod smooth;
pub mod stroke;
pub mod tessellate;
mod vec2;

pub use hit::{Bounds, Hit, bounds};
pub use simplify::{Lod, simplify};
pub use smooth::{SmoothOptions, corner_indices, smooth};
pub use stroke::{DEFAULT_WIDTH, MAX_PRESSURE, MERGE_EPSILON, MIN_PRESSURE, Stroke, StrokePoint};
pub use tessellate::{Mesh, TessellationOptions, tessellate};

/// Everything that can go wrong turning a stroke into triangles.
///
/// Deliberately narrow. The interesting inputs — an empty point array, a single
/// point, duplicated points, a corrupt coordinate — are all *shapes a stroke can
/// legitimately have* and are handled by [`Stroke::new`] rather than reported here.
/// Reaching this type means lyon itself failed, which is either a bug in lyon or a
/// stroke whose vertex count overflows a `u32` index, and neither should be silently
/// swallowed into an empty mesh.
#[derive(Debug, thiserror::Error)]
pub enum InkError {
    #[error("lyon could not tessellate the stroke: {0}")]
    Tessellation(#[from] lyon::tessellation::TessellationError),
}

impl Stroke {
    /// Simplify, smooth and tessellate at the detail the current view can resolve.
    ///
    /// One tolerance drives all three stages, so zooming out costs less work at
    /// every step rather than only at the last one.
    pub fn render(&self, lod: Lod) -> Result<Mesh, InkError> {
        let tolerance = lod.world_tolerance();
        self.simplified(tolerance)
            .smoothed(&SmoothOptions::with_tolerance(tolerance))
            .tessellate(&TessellationOptions::with_tolerance(tolerance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plausible hand-drawn stroke: a wobbly arc, sampled unevenly the way a real
    /// pointer samples one.
    fn hand_drawn(n: usize) -> Stroke {
        let points: Vec<_> = (0..n)
            .map(|i| {
                let t = i as f64 / (n - 1) as f64;
                let angle = t * std::f64::consts::PI * 1.4;
                let wobble = (t * 37.0).sin() * 1.5;
                (
                    300.0 * angle.cos() + wobble,
                    300.0 * angle.sin() + (t * 23.0).cos() * 1.5,
                )
            })
            .collect();
        Stroke::from_miro(&points, Some(4.0))
    }

    #[test]
    fn the_whole_pipeline_runs_on_every_degenerate_shape() {
        let strokes = [
            Stroke::from_miro(&[], None),
            Stroke::from_miro(&[(0.0, 0.0)], None),
            Stroke::from_miro(&[(0.0, 0.0), (0.0, 0.0)], None),
            Stroke::from_miro(&[(0.0, 0.0), (1.0, 1.0)], None),
            Stroke::from_miro(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0)], None),
        ];
        for stroke in strokes {
            for zoom in [0.01, 0.1, 1.0, 8.0, 64.0] {
                let mesh = stroke.render(Lod::new(zoom)).expect("render failed");
                assert!(mesh.is_finite() && mesh.is_well_formed(), "{stroke:?} at {zoom}");
            }
        }
    }

    /// The point of the whole exercise: zooming out has to cost less. Measured
    /// end-to-end, because a saving in one stage that the next stage gives back is
    /// no saving at all.
    ///
    /// Monotone here for one representative stroke. It is *not* monotone in general
    /// — see `the_most_distant_view_is_never_the_most_expensive` in
    /// `tests/properties.rs` for why, and for the weaker claim that does hold
    /// everywhere.
    #[test]
    fn zooming_out_reduces_the_mesh_for_a_typical_stroke() {
        let stroke = hand_drawn(500);
        let mut previous = 0;
        for zoom in [0.01, 0.1, 0.25, 1.0, 4.0, 16.0] {
            let n = stroke.render(Lod::new(zoom)).unwrap().vertex_count();
            assert!(n >= previous, "zoom {zoom} gave {n} vertices after {previous}");
            previous = n;
        }
    }

    /// The pipeline's cost profile on a long stroke, pinned so that retuning any
    /// stage shows up here rather than as a frame-time regression later. Ranges
    /// rather than exact counts, because the last few vertices belong to lyon's
    /// round joins and a patch release of lyon may legitimately move them.
    ///
    /// Real Miro strokes are much shorter than this — see `tests/real_board.rs`,
    /// which pins the same profile against the actual reference board.
    #[test]
    fn a_typical_500_point_stroke_has_the_documented_lod_profile() {
        let stroke = hand_drawn(500);
        for (zoom, low, high) in [
            (0.01, 20, 35),
            (0.1, 45, 75),
            (1.0, 180, 260),
            (4.0, 560, 780),
            (16.0, 1200, 1650),
        ] {
            let n = stroke.render(Lod::new(zoom)).unwrap().vertex_count();
            assert!((low..=high).contains(&n), "zoom {zoom}: {n} vertices, expected {low}..{high}");
        }
    }

    /// The claim that matters for a board full of imported ink: cost follows the
    /// stroke's shape, not how fast the hand was moving when it was drawn.
    #[test]
    fn oversampling_a_stroke_does_not_make_it_more_expensive_to_draw() {
        let counts: Vec<_> = [5_000, 20_000, 50_000]
            .iter()
            .map(|&n| hand_drawn(n).render(Lod::new(1.0)).unwrap().vertex_count())
            .collect();
        assert!(
            counts.windows(2).all(|w| w[0] == w[1]),
            "the same arc should cost the same at any sample rate: {counts:?}"
        );
    }

    #[test]
    fn rendering_never_leaves_the_bounds() {
        let stroke = hand_drawn(200);
        let bounds = stroke.bounds().unwrap();
        for zoom in [0.1, 1.0, 16.0] {
            for p in &stroke.render(Lod::new(zoom)).unwrap().positions {
                // A tolerance-sized slack: smoothing may bulge slightly outside the
                // raw polyline's box, which is inherent to an interpolating spline.
                let slack = Lod::new(zoom).world_tolerance() + 2.0;
                assert!(
                    (p[0] as f64) >= bounds.min_x - slack
                        && (p[0] as f64) <= bounds.max_x + slack
                        && (p[1] as f64) >= bounds.min_y - slack
                        && (p[1] as f64) <= bounds.max_y + slack,
                    "zoom {zoom}: {p:?} far outside {bounds:?}"
                );
            }
        }
    }

    /// Cutting a stroke and re-rendering both halves has to work — a partial erase
    /// is not a special case that skips the pipeline.
    #[test]
    fn a_split_stroke_still_renders() {
        let stroke = hand_drawn(120);
        let midpoint = stroke.points()[60];
        let (head, tail) = stroke.split_at((midpoint.x, midpoint.y), 0.0).expect("split failed");
        for piece in [head, tail] {
            let mesh = piece.render(Lod::default()).unwrap();
            assert!(mesh.is_finite() && !mesh.is_empty());
        }
    }
}
