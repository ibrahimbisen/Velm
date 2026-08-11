//! Tessellated output: the triangles a shape becomes when it cannot be drawn
//! analytically.
//!
//! Fill and stroke are separate meshes because they are separate draw calls with
//! separate materials, and because a shape is frequently one without the other — a
//! sticky is fill-only, a flowchart connector target is usually both.
//!
//! Stroke vertices carry the path position *and* the offset normal rather than the
//! final expanded position. The renderer computes `position + normal · width/2` in
//! the vertex shader, so a border can change width, or stay a constant number of
//! *pixels* wide while the board zooms, without re-tessellating anything.

use crate::error::ShapeError;
use crate::outline::Outline;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, FillVertexConstructor, StrokeOptions,
    StrokeTessellator, StrokeVertex as LyonStrokeVertex, StrokeVertexConstructor, VertexBuffers,
};

/// An indexed triangle mesh in unit-box space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// One vertex of a stroke outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeVertex {
    /// Position **on the path**, before the stroke is widened.
    pub position: [f32; 2],
    /// Offset direction, scaled so that `position + normal · w/2` is the vertex of
    /// a stroke of width `w`. At a miter join its length exceeds 1, which is what
    /// keeps the corner sharp at any width.
    pub normal: [f32; 2],
}

/// A stroked outline as triangles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StrokeMesh {
    pub vertices: Vec<StrokeVertex>,
    pub indices: Vec<u32>,
    /// The width the joins were computed for. Re-expanding to a very different
    /// width is fine for the vertices themselves but slides the miter geometry, so
    /// a renderer that supports a wide zoom range should re-tessellate on a big
    /// change rather than on every frame.
    pub line_width: f32,
}

/// Both meshes for one shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShapeMesh {
    pub fill: Mesh,
    pub stroke: StrokeMesh,
}

/// How finely to tessellate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TessellationOptions {
    /// Maximum distance between a curve and its flattened approximation, **in
    /// unit-box units**. `lyon`'s own default of `0.1` is meant for pixel-space
    /// paths and would turn a unit circle into a decagon, so this crate always sets
    /// it explicitly.
    pub tolerance: f32,
    /// Stroke width in unit-box units.
    pub line_width: f32,
}

impl TessellationOptions {
    /// Tolerance for a shape drawn at `size` units across, keeping the flattening
    /// error at a quarter of a unit — invisible, and the point at which spending
    /// more vertices stops buying anything.
    pub fn for_size(size: f32) -> Self {
        Self { tolerance: (0.25 / size.max(1.0)).min(0.01), ..Self::default() }
    }
}

impl Default for TessellationOptions {
    /// Tuned for a shape around 400 units across, which is the size band Miro's own
    /// default shapes land in.
    fn default() -> Self {
        Self { tolerance: 0.000_625, line_width: 0.01 }
    }
}

struct FillPositions;

impl FillVertexConstructor<[f32; 2]> for FillPositions {
    fn new_vertex(&mut self, vertex: FillVertex) -> [f32; 2] {
        let p = vertex.position();
        [p.x, p.y]
    }
}

struct StrokePositions;

impl StrokeVertexConstructor<StrokeVertex> for StrokePositions {
    fn new_vertex(&mut self, vertex: LyonStrokeVertex) -> StrokeVertex {
        let p = vertex.position_on_path();
        let n = vertex.normal();
        StrokeVertex { position: [p.x, p.y], normal: [n.x, n.y] }
    }
}

impl Outline {
    /// Triangulates the silhouette under the non-zero fill rule.
    pub fn tessellate_fill(&self, options: TessellationOptions) -> Result<Mesh, ShapeError> {
        let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
        FillTessellator::new().tessellate_path(
            &self.fill_path(),
            &FillOptions::non_zero().with_tolerance(options.tolerance),
            &mut BuffersBuilder::new(&mut buffers, FillPositions),
        )?;
        Ok(Mesh { vertices: buffers.vertices, indices: buffers.indices })
    }

    /// Triangulates the silhouette *and* the interior detail lines as a stroke.
    pub fn tessellate_stroke(
        &self,
        options: TessellationOptions,
    ) -> Result<StrokeMesh, ShapeError> {
        let mut buffers: VertexBuffers<StrokeVertex, u32> = VertexBuffers::new();
        StrokeTessellator::new().tessellate_path(
            &self.stroke_path(),
            &StrokeOptions::default()
                .with_tolerance(options.tolerance)
                .with_line_width(options.line_width),
            &mut BuffersBuilder::new(&mut buffers, StrokePositions),
        )?;
        Ok(StrokeMesh {
            vertices: buffers.vertices,
            indices: buffers.indices,
            line_width: options.line_width,
        })
    }

    pub fn tessellate(&self, options: TessellationOptions) -> Result<ShapeMesh, ShapeError> {
        Ok(ShapeMesh {
            fill: self.tessellate_fill(options)?,
            stroke: self.tessellate_stroke(options)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::{Point, p};

    fn square() -> Outline {
        Outline::polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)])
    }

    #[test]
    fn a_square_fills_as_two_triangles() {
        let mesh = square().tessellate_fill(TessellationOptions::default()).unwrap();
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.triangle_count(), 2);
        assert!(mesh.indices.iter().all(|&i| (i as usize) < mesh.vertices.len()));
    }

    #[test]
    fn stroke_vertices_expand_from_the_path_by_the_normal() {
        let options = TessellationOptions { line_width: 0.1, ..Default::default() };
        let mesh = square().tessellate_stroke(options).unwrap();
        assert!(!mesh.indices.is_empty());
        assert_eq!(mesh.line_width, 0.1);
        // Every stroke vertex sits on the square's boundary, and its normal points
        // off it: expanding by half the width must land inside the widened band.
        for v in &mesh.vertices {
            let position = Point::new(v.position[0], v.position[1]);
            assert!(square().nearest_point(position).distance(position) < 1e-4, "{v:?}");
            let normal = Point::new(v.normal[0], v.normal[1]);
            assert!(normal.length() >= 0.99, "a unit-ish normal is needed to re-expand: {v:?}");
        }
    }

    #[test]
    fn tolerance_scales_with_the_size_the_shape_is_drawn_at() {
        assert!(TessellationOptions::for_size(4000.0).tolerance < TessellationOptions::for_size(40.0).tolerance);
        assert!(TessellationOptions::for_size(0.0).tolerance.is_finite());
    }

    /// A curved shape must produce enough triangles to look round rather than
    /// faceted; `lyon`'s default tolerance in unit space would give about ten.
    #[test]
    fn a_circle_is_not_tessellated_as_a_decagon() {
        let circle = crate::Shape::Ellipse.outline(1.0);
        let mesh = circle.tessellate_fill(TessellationOptions::default()).unwrap();
        assert!(mesh.triangle_count() > 50, "{} triangles", mesh.triangle_count());
    }
}
