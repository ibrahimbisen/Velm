//! Turning a stroke into triangles.
//!
//! The output is an indexed triangle mesh in the stroke's own coordinate space:
//! positions and indices, nothing else. No GPU types, no buffer handles, no wgpu —
//! the renderer owns all of that, and keeping this side of the boundary pure is what
//! lets several hundred strokes be tessellated and checked by `cargo test` on a
//! machine with no graphics stack.
//!
//! # Round caps and round joins, not configurable
//!
//! Both are fixed. That is a deliberate restriction, not an omission: [`crate::hit`]
//! computes exact bounds and an exact signed distance by treating the stroke as the
//! centreline swept by a disc, and that identity only holds for round caps and round
//! joins. Offering butt caps as an option would silently make `bounds()` wrong at the
//! ends of every stroke and `hit_test` wrong near every corner. Ink is drawn with a
//! round nib in every application anyone has used, so the restriction costs nothing
//! real.
//!
//! Miter joins would be worse than merely inconsistent: a hand-drawn stroke doubling
//! back on itself produces near-180° turns, and a miter there shoots a spike out to
//! the miter limit.
//!
//! # Fixed width against variable width
//!
//! lyon has two stroking paths. The variable-width one interpolates a per-vertex
//! width attribute; the fixed-width one is the older, far more heavily exercised
//! code. Miro's `paint` widgets carry **no pressure at all** — every one of the 219
//! strokes on the reference board is uniform width — so imported ink takes the
//! fixed-width path, and the variable-width path only runs for Vellum's own stylus
//! input. Uniform pressure that is not 1.0 is folded into the line width instead of
//! being passed as an attribute, so "the pen was pressed evenly" still takes the
//! simpler route.
//!
//! # `u32` indices
//!
//! `u16` runs out at 65,535 vertices. No single stroke gets close — the largest on
//! the reference board is 128 points and costs about 1,000 vertices at maximum zoom —
//! but the board as a whole does: its 219 strokes tessellate to **103,297 vertices**
//! at zoom 64. The obvious optimisation for 219 small meshes is one buffer per draw
//! call rather than 219 of them, and at `u16` that batch overflows on real data
//! rather than hypothetical data. Overflow would not fail loudly either; it would
//! wrap and render as garbage. The extra two bytes per index are bought deliberately.

use lyon::math::point;
use lyon::path::{LineCap, LineJoin, Path, Winding};
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, FillVertexConstructor,
    StrokeOptions, StrokeTessellator, StrokeVertex, StrokeVertexConstructor, VertexBuffers,
};

use crate::InkError;
use crate::stroke::{Stroke, StrokePoint};

/// Index of the per-vertex width attribute on the variable-width path.
const WIDTH_ATTRIBUTE: lyon::path::AttributeIndex = 0;

/// An indexed triangle mesh, ready to become a vertex and an index buffer.
///
/// Positions are `f32` even though the rest of the crate is `f64`. They are in the
/// stroke's local space — a few hundred px across at most — so the cast is exact for
/// every value that occurs in practice, and the alternative would be converting on
/// every upload. World-space `f64` matters for *placing* a stroke on a 41,000 px
/// board; it does not matter for the offsets within one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 2]>,
    /// Triangle list. Three consecutive entries are one triangle.
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Whether every position is finite.
    ///
    /// Worth having as a public assertion rather than only a test helper: a single
    /// NaN in a vertex buffer does not fail, it rasterises as a triangle stretched
    /// across the whole viewport, and tracking that back to its stroke from a
    /// screenshot is expensive. A debug-build check here names the culprit instead.
    pub fn is_finite(&self) -> bool {
        self.positions.iter().all(|p| p[0].is_finite() && p[1].is_finite())
    }

    /// Whether every index addresses a real vertex.
    pub fn is_well_formed(&self) -> bool {
        self.indices.len().is_multiple_of(3)
            && self.indices.iter().all(|&i| (i as usize) < self.positions.len())
    }
}

impl From<VertexBuffers<[f32; 2], u32>> for Mesh {
    fn from(buffers: VertexBuffers<[f32; 2], u32>) -> Self {
        Self { positions: buffers.vertices, indices: buffers.indices }
    }
}

/// How finely the stroke's outline is approximated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TessellationOptions {
    /// Maximum distance, in world px, between the emitted outline and the true one.
    ///
    /// This is an *outline* budget: it controls how many facets a round cap or join
    /// is broken into. It is separate from the centreline budget that smoothing and
    /// simplification spend, and the two do not add up — one moves the edge of the
    /// stroke, the other moves its middle.
    pub tolerance: f64,
}

impl Default for TessellationOptions {
    fn default() -> Self {
        Self { tolerance: 0.25 }
    }
}

impl TessellationOptions {
    pub fn with_tolerance(tolerance: f64) -> Self {
        Self { tolerance }
    }

    /// lyon rejects a non-finite tolerance and spins on a tiny one, so the value is
    /// clamped here rather than trusted from a caller that computed it by dividing
    /// by a zoom.
    fn tolerance_f32(&self) -> f32 {
        let t = if self.tolerance.is_finite() { self.tolerance.max(1e-3) } else { 0.25 };
        t as f32
    }
}

/// Tessellates a stroke into triangles.
///
/// An empty stroke gives an empty mesh — there is nothing to draw, and that is not
/// an error. A single point gives a filled disc, because Miro's real data contains
/// `points: [{"x":0,"y":0}]` and a tap of the pen is ink the user expects to see.
///
/// Failure means either an internal lyon bug or a stroke so large its vertex count
/// overflows `u32`; both are worth surfacing rather than swallowing.
pub fn tessellate(stroke: &Stroke, options: &TessellationOptions) -> Result<Mesh, InkError> {
    match stroke.points() {
        [] => Ok(Mesh::default()),
        [only] => tessellate_dot(*only, stroke.width(), options),
        _ => tessellate_polyline(stroke, options),
    }
}

fn tessellate_dot(
    p: StrokePoint,
    width: f64,
    options: &TessellationOptions,
) -> Result<Mesh, InkError> {
    let radius = (width * p.pressure / 2.0) as f32;
    let mut builder = Path::builder();
    builder.add_circle(point(p.x as f32, p.y as f32), radius, Winding::Positive);
    let path = builder.build();

    let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    FillTessellator::new().tessellate_path(
        &path,
        &FillOptions::tolerance(options.tolerance_f32()),
        &mut BuffersBuilder::new(&mut buffers, Position),
    )?;
    Ok(buffers.into())
}

fn tessellate_polyline(
    stroke: &Stroke,
    options: &TessellationOptions,
) -> Result<Mesh, InkError> {
    let points = stroke.points();
    let base = StrokeOptions::tolerance(options.tolerance_f32())
        .with_line_cap(LineCap::Round)
        .with_line_join(LineJoin::Round);

    let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tessellator = StrokeTessellator::new();

    if stroke.pressure_varies() {
        let pressures = slope_limited_pressures(points, stroke.width());
        let mut builder = Path::builder_with_attributes(1);
        builder.begin(to_lyon(points[0]), &[pressures[0] as f32]);
        for (p, pressure) in points[1..].iter().zip(&pressures[1..]) {
            builder.line_to(to_lyon(*p), &[*pressure as f32]);
        }
        builder.end(false);

        tessellator.tessellate_path(
            &builder.build(),
            &base
                .with_line_width(stroke.width() as f32)
                .with_variable_line_width(WIDTH_ATTRIBUTE),
            &mut BuffersBuilder::new(&mut buffers, Position),
        )?;
    } else {
        let mut builder = Path::builder();
        builder.begin(to_lyon(points[0]));
        for p in &points[1..] {
            builder.line_to(to_lyon(*p));
        }
        builder.end(false);

        let width = stroke.width() * points[0].pressure;
        tessellator.tessellate_path(
            &builder.build(),
            &base.with_line_width(width as f32),
            &mut BuffersBuilder::new(&mut buffers, Position),
        )?;
    }

    Ok(buffers.into())
}

/// Pressures with the radius gradient limited to what a moving nib can produce.
///
/// A nib's radius cannot grow faster than the nib travels. When it does — pressure
/// jumping thirteen-fold between two samples four pixels apart, which a random
/// generator produces readily and a real digitiser never does — the two discs stop
/// having a common tangent: one simply contains the other, and the swept shape is
/// just the larger disc. lyon's variable-width stroker offsets along that tangent, so
/// with no tangent to follow it extrapolates and puts vertices outside the stroke,
/// which was measured at 1.8px of overshoot on a 27px-wide stroke.
///
/// Rather than let that reach the mesh, the radius gradient is limited in both
/// directions. Two passes are exact for a chain: the forward pass makes each radius
/// no larger than its predecessor plus the distance travelled, the backward pass does
/// the same from the other end, and the result is the largest profile satisfying both
/// constraints. Radii only ever *decrease*, so the mesh stays inside the bounds
/// computed from the unclamped pressures — the exactness of [`crate::hit::bounds`] is
/// preserved rather than traded away.
fn slope_limited_pressures(points: &[StrokePoint], width: f64) -> Vec<f64> {
    // Strictly below 1 so that the tangent exists rather than being degenerate at
    // exactly the limiting angle.
    const MAX_SLOPE: f64 = 0.95;

    let to_radius = width / 2.0;
    let mut radii: Vec<f64> = points.iter().map(|p| p.pressure * to_radius).collect();

    for i in 1..radii.len() {
        let reach = points[i - 1].distance_to(points[i]) * MAX_SLOPE;
        radii[i] = radii[i].min(radii[i - 1] + reach);
    }
    for i in (0..radii.len() - 1).rev() {
        let reach = points[i].distance_to(points[i + 1]) * MAX_SLOPE;
        radii[i] = radii[i].min(radii[i + 1] + reach);
    }

    radii.iter().map(|r| r / to_radius).collect()
}

fn to_lyon(p: StrokePoint) -> lyon::math::Point {
    point(p.x as f32, p.y as f32)
}

/// Keeps only the position. Every other stroke attribute lyon can supply — normal,
/// advancement, side — is recoverable on the GPU or unused, and a fatter vertex is
/// bandwidth spent on all 219 strokes to serve none of them.
struct Position;

impl StrokeVertexConstructor<[f32; 2]> for Position {
    fn new_vertex(&mut self, vertex: StrokeVertex) -> [f32; 2] {
        vertex.position().to_array()
    }
}

impl FillVertexConstructor<[f32; 2]> for Position {
    fn new_vertex(&mut self, vertex: FillVertex) -> [f32; 2] {
        vertex.position().to_array()
    }
}

impl Stroke {
    /// This stroke as triangles. See [`tessellate`].
    pub fn tessellate(&self, options: &TessellationOptions) -> Result<Mesh, InkError> {
        tessellate(self, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xy(points: &[(f64, f64)]) -> Stroke {
        Stroke::from_miro(points, Some(8.0))
    }

    fn mesh_of(stroke: &Stroke) -> Mesh {
        stroke.tessellate(&TessellationOptions::default()).expect("tessellation failed")
    }

    /// Every mesh in this crate has to satisfy these, so they are checked together
    /// on every shape the tests produce.
    fn assert_sane(mesh: &Mesh) {
        assert!(mesh.is_finite(), "mesh contains a non-finite position");
        assert!(mesh.is_well_formed(), "mesh has a dangling or ragged index");
    }

    /// Bounds containment with room for the f32 round-trip the mesh goes through.
    fn within(bounds: &crate::Bounds, p: [f32; 2]) -> bool {
        const EPSILON: f64 = 1e-3;
        let (x, y) = (p[0] as f64, p[1] as f64);
        x >= bounds.min_x - EPSILON
            && x <= bounds.max_x + EPSILON
            && y >= bounds.min_y - EPSILON
            && y <= bounds.max_y + EPSILON
    }

    #[test]
    fn an_empty_stroke_produces_an_empty_mesh() {
        let mesh = mesh_of(&xy(&[]));
        assert!(mesh.is_empty());
        assert_eq!(mesh.vertex_count(), 0);
        assert_sane(&mesh);
    }

    /// The case real Miro data contains: `points: [{"x":0,"y":0}]`.
    #[test]
    fn a_single_point_renders_as_a_dot() {
        let mesh = mesh_of(&xy(&[(0.0, 0.0)]));
        assert_sane(&mesh);
        assert!(mesh.triangle_count() >= 4, "a dot needs real geometry: {mesh:?}");

        // Every vertex sits on or inside the nib.
        let radius = 4.0_f32;
        for p in &mesh.positions {
            assert!(p[0].hypot(p[1]) <= radius + 1e-4, "vertex {p:?} outside the nib");
        }
        // And the disc is actually filled out, not collapsed.
        let extent = mesh.positions.iter().map(|p| p[0].hypot(p[1])).fold(0.0, f32::max);
        assert!(extent > radius * 0.95, "the dot is too small: {extent}");
    }

    #[test]
    fn two_points_produce_a_capsule() {
        let mesh = mesh_of(&xy(&[(0.0, 0.0), (100.0, 0.0)]));
        assert_sane(&mesh);
        assert!(mesh.triangle_count() >= 2);

        let (min_x, max_x, max_y) = mesh.positions.iter().fold(
            (f32::MAX, f32::MIN, f32::MIN),
            |(lo, hi, y), p| (lo.min(p[0]), hi.max(p[0]), y.max(p[1].abs())),
        );
        // Round caps reach half a width beyond each end, and no further.
        assert!((-4.01..=-3.9).contains(&min_x), "start cap at {min_x}");
        assert!((103.9..=104.01).contains(&max_x), "end cap at {max_x}");
        assert!(max_y <= 4.01, "stroke is wider than its width: {max_y}");
    }

    /// The mesh must stay inside the bounds [`crate::hit::bounds`] promises, or
    /// culling would clip visible ink.
    #[test]
    fn the_mesh_never_escapes_the_computed_bounds() {
        let strokes = [
            xy(&[(0.0, 0.0)]),
            xy(&[(0.0, 0.0), (50.0, 0.0)]),
            xy(&[(0.0, 0.0), (50.0, 60.0), (100.0, 0.0), (20.0, 40.0)]),
        ];
        for s in strokes {
            let bounds = s.bounds().unwrap();
            for p in &mesh_of(&s).positions {
                assert!(within(&bounds, *p), "vertex {p:?} escapes {bounds:?}");
            }
        }
    }

    #[test]
    fn duplicate_and_collinear_points_still_tessellate() {
        assert_sane(&mesh_of(&xy(&[(0.0, 0.0), (0.0, 0.0), (0.0, 0.0)])));
        assert_sane(&mesh_of(&xy(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0), (30.0, 0.0)])));
    }

    #[test]
    fn a_self_intersecting_stroke_tessellates() {
        let mesh = mesh_of(&xy(&[
            (0.0, 0.0),
            (100.0, 100.0),
            (100.0, 0.0),
            (0.0, 100.0),
            (50.0, 50.0),
        ]));
        assert_sane(&mesh);
        assert!(mesh.triangle_count() > 8);
    }

    /// A near-180° reversal is where a miter join would fire a spike out to the
    /// miter limit. Round joins must keep every vertex within the stroke's own
    /// bounds.
    #[test]
    fn a_hairpin_turn_does_not_spike() {
        let s = xy(&[(0.0, 0.0), (100.0, 0.0), (0.0, 0.1)]);
        let bounds = s.bounds().unwrap();
        for p in &mesh_of(&s).positions {
            assert!(within(&bounds, *p), "spike at {p:?}, outside {bounds:?}");
        }
    }

    #[test]
    fn variable_width_takes_the_variable_path_and_stays_bounded() {
        let s = Stroke::new(
            (0..40)
                .map(|i| {
                    let t = i as f64 / 39.0;
                    StrokePoint::with_pressure(i as f64 * 5.0, (t * 6.0).sin() * 20.0, 0.2 + t)
                })
                .collect::<Vec<_>>(),
            10.0,
        );
        assert!(s.pressure_varies());
        let mesh = mesh_of(&s);
        assert_sane(&mesh);

        let bounds = s.bounds().unwrap();
        for p in &mesh.positions {
            assert!(within(&bounds, *p), "vertex {p:?} escapes {bounds:?}");
        }
    }

    /// Uniform pressure that is not 1.0 must still change the width, even though it
    /// takes the fixed-width path.
    #[test]
    fn uniform_pressure_scales_the_width() {
        let half = Stroke::new(
            [
                StrokePoint::with_pressure(0.0, 0.0, 0.5),
                StrokePoint::with_pressure(100.0, 0.0, 0.5),
            ],
            8.0,
        );
        assert!(!half.pressure_varies());
        let max_y =
            mesh_of(&half).positions.iter().map(|p| p[1].abs()).fold(0.0_f32, f32::max);
        assert!((max_y - 2.0).abs() < 0.05, "half pressure should give a 2px half-width: {max_y}");
    }

    /// A pressure profile no nib could produce — a thirteen-fold jump over four
    /// pixels — is the case that pushed lyon's variable-width stroker 1.8px outside
    /// the stroke's own bounds. The limiter must bring the radius gradient back
    /// below 1 while only ever narrowing.
    #[test]
    fn an_impossible_width_gradient_is_slope_limited() {
        let points = [
            StrokePoint::with_pressure(0.0, 0.0, 0.2),
            StrokePoint::with_pressure(4.0, 0.0, 2.8),
            StrokePoint::with_pressure(8.0, 0.0, 2.6),
        ];
        let width = 18.0;
        let limited = slope_limited_pressures(&points, width);

        for (i, (before, after)) in points.iter().zip(&limited).enumerate() {
            assert!(*after <= before.pressure + 1e-12, "point {i} was widened, not narrowed");
        }
        for i in 1..points.len() {
            let travelled = points[i - 1].distance_to(points[i]);
            let change = ((limited[i] - limited[i - 1]) * width / 2.0).abs();
            assert!(change <= travelled + 1e-9, "radius moved {change} over {travelled} px");
        }
    }

    /// And the limiter must leave a physically plausible taper alone.
    #[test]
    fn a_plausible_taper_is_untouched() {
        let points: Vec<_> = (0..20)
            .map(|i| StrokePoint::with_pressure(i as f64 * 10.0, 0.0, 0.3 + i as f64 * 0.05))
            .collect();
        let limited = slope_limited_pressures(&points, 4.0);
        for (before, after) in points.iter().zip(&limited) {
            assert!((before.pressure - after).abs() < 1e-12, "a real taper was clamped");
        }
    }

    #[test]
    fn a_coarser_tolerance_produces_fewer_vertices() {
        let s = xy(&[(0.0, 0.0), (60.0, 80.0), (120.0, 0.0), (180.0, 80.0)]);
        let fine = s.tessellate(&TessellationOptions::with_tolerance(0.01)).unwrap();
        let coarse = s.tessellate(&TessellationOptions::with_tolerance(2.0)).unwrap();
        assert!(
            coarse.vertex_count() < fine.vertex_count(),
            "coarse {} vs fine {}",
            coarse.vertex_count(),
            fine.vertex_count()
        );
    }

    #[test]
    fn a_broken_tolerance_does_not_reach_lyon() {
        for tolerance in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let mesh = xy(&[(0.0, 0.0), (50.0, 20.0)])
                .tessellate(&TessellationOptions::with_tolerance(tolerance))
                .expect("a bad tolerance should be clamped, not passed through");
            assert_sane(&mesh);
        }
    }

    /// Past 65,535 vertices — the case `u16` indices would wrap on rather than
    /// reject. Long enough to also be a real workout for the tessellator.
    #[test]
    fn a_very_large_stroke_tessellates_past_the_u16_ceiling() {
        let points: Vec<_> = (0..40_000)
            .map(|i| {
                let t = i as f64 * 0.02;
                (t * 3.0, t.sin() * 120.0)
            })
            .collect();
        let mesh = mesh_of(&xy(&points));
        assert_sane(&mesh);
        assert!(
            mesh.vertex_count() > u16::MAX as usize,
            "{} vertices; this is the case u16 would lose",
            mesh.vertex_count()
        );
    }
}
