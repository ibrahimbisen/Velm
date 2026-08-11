//! Triangle meshes: ink strokes, connectors, and the shapes no distance field can
//! express.
//!
//! Three crates produce indexed triangles and none of them knows about the GPU:
//! `vellum-ink` emits stroke-local positions, `vellum-connect` emits positions
//! relative to an `f64` world origin it carries alongside, and `vellum-shapes` emits
//! unit-box positions plus a stroke mesh whose vertices are a path point and an
//! offset normal. [`MeshBatch`] takes all four forms into **one** vertex buffer and
//! one index buffer, so a board's entire ink is a single draw call.
//!
//! # Why a vertex names a transform instead of carrying a world position
//!
//! Camera-relative coordinates change every time the camera does, so baking them
//! into vertices would mean rewriting the whole vertex buffer on every frame of a
//! pan — 1.6 MB for the reference board's ink at deep zoom, every frame, for a pan
//! that changed two numbers. Instead each vertex holds an index into a small
//! transform table, and a pan rewrites the table: 32 bytes per distinct item rather
//! than 16 per vertex.
//!
//! That is also what makes the `f64` → `f32` rebasing correct. A stroke's own
//! vertices stay in its local space, where they are a few hundred pixels at most and
//! `f32` is exact; the only value that carries the board's 41282 px extent is the
//! transform's translation, which the caller computes with
//! [`Camera::to_camera_relative`](vellum_scene::Camera::to_camera_relative) in `f64`.
//!
//! # `u32` indices
//!
//! `vellum-ink` already argues this: its 219 strokes tessellate to 103,297 vertices
//! at maximum zoom, so batching them into one buffer overflows `u16` on real data,
//! silently, as wrapped indices that rasterise into garbage.

use crate::color::Rgba;
use crate::pipeline::{self, PipelineDescriptor};

/// One vertex of a batched mesh. 16 bytes: position, packed colour, transform index.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MeshVertex {
    /// In the source mesh's own space. The named transform places it.
    pub position: [f32; 2],
    /// Straight RGBA, 8 bits per channel — see [`Rgba::pack`].
    pub color: [u8; 4],
    /// Index into the batch's transform table.
    pub transform: u32,
}

/// An affine placement: a 2 × 2 matrix and a translation in the view's space.
///
/// Matches `MeshTransform` in `shaders/mesh.wgsl`, including the padding that brings
/// it to the 16-byte alignment WGSL gives a struct containing a `vec4`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MeshTransform {
    /// Column-major: `(m00, m10, m01, m11)`.
    pub matrix: [f32; 4],
    /// Camera-relative world pixels for board content, physical pixels for overlays.
    pub translation: [f32; 2],
    padding: [f32; 2],
}

impl MeshTransform {
    pub const IDENTITY: Self = Self {
        matrix: [1.0, 0.0, 0.0, 1.0],
        translation: [0.0, 0.0],
        padding: [0.0, 0.0],
    };

    /// Places a mesh that is already in the right scale and orientation.
    pub fn at(translation: [f32; 2]) -> Self {
        Self { translation, ..Self::IDENTITY }
    }

    /// Uniform scale, then rotation, then translation — the order a placed item is
    /// built in, and the only order under which a rotated shape keeps its proportions.
    pub fn scale_rotate_at(scale: f32, radians: f32, translation: [f32; 2]) -> Self {
        let (sin, cos) = radians.sin_cos();
        Self {
            matrix: [cos * scale, sin * scale, -sin * scale, cos * scale],
            translation,
            padding: [0.0, 0.0],
        }
    }

    /// Maps `vellum-shapes`' unit box — `0..1` on both axes, y down — onto a rect
    /// given by its top-left corner and size, with an optional rotation about its
    /// centre.
    ///
    /// This is the transform for a tessellated shape, and it is separate from
    /// [`Self::scale_rotate_at`] because the unit box's origin is a *corner* while
    /// rotation is about the *centre*.
    pub fn unit_box(top_left: [f32; 2], size: [f32; 2], radians: f32) -> Self {
        let (sin, cos) = radians.sin_cos();
        let half = [size[0] * 0.5, size[1] * 0.5];
        // p_view = R * (S * p_unit - half) + centre, and centre = top_left + half, so
        // the translation is top_left + half - R * half.
        Self {
            matrix: [cos * size[0], sin * size[0], -sin * size[1], cos * size[1]],
            translation: [
                top_left[0] + half[0] - (cos * half[0] - sin * half[1]),
                top_left[1] + half[1] - (sin * half[0] + cos * half[1]),
            ],
            padding: [0.0, 0.0],
        }
    }

    /// Applies the transform on the CPU — the same arithmetic `vs_mesh` performs, so
    /// tests can check placement without a GPU.
    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        [
            self.matrix[0] * p[0] + self.matrix[2] * p[1] + self.translation[0],
            self.matrix[1] * p[0] + self.matrix[3] * p[1] + self.translation[1],
        ]
    }
}

/// Every triangle a frame draws, in one vertex buffer and one index buffer.
#[derive(Debug, Clone, Default)]
pub struct MeshBatch {
    vertices: Vec<MeshVertex>,
    indices: Vec<u32>,
    transforms: Vec<MeshTransform>,
}

impl MeshBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn vertices(&self) -> &[MeshVertex] {
        &self.vertices
    }

    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    pub fn transforms(&self) -> &[MeshTransform] {
        &self.transforms
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Empties the geometry *and* the transforms.
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
        self.transforms.clear();
    }

    /// Reserves a transform slot and returns its index.
    pub fn push_transform(&mut self, transform: MeshTransform) -> u32 {
        self.transforms.push(transform);
        (self.transforms.len() - 1) as u32
    }

    /// Rewrites one transform, leaving the geometry alone.
    ///
    /// This is the fast path a pan takes: the vertex and index buffers are untouched
    /// and only the transform table is re-uploaded.
    pub fn set_transform(&mut self, index: u32, transform: MeshTransform) -> bool {
        match self.transforms.get_mut(index as usize) {
            Some(slot) => {
                *slot = transform;
                true
            }
            None => false,
        }
    }

    /// Appends an indexed mesh, rebasing its indices onto the batch.
    ///
    /// Indices that address past `positions` are dropped rather than uploaded: a
    /// malformed index is a GPU-side out-of-bounds read, and the triangle it would
    /// have drawn is not worth that.
    pub fn push_indexed(
        &mut self,
        positions: &[[f32; 2]],
        indices: &[u32],
        color: Rgba,
        transform: u32,
    ) {
        if positions.is_empty() || indices.len() < 3 {
            return;
        }
        let base = self.vertices.len() as u32;
        let packed = color.pack();
        self.vertices.extend(positions.iter().map(|&position| MeshVertex {
            position,
            color: packed,
            transform,
        }));
        let limit = positions.len() as u32;
        for triangle in indices.chunks_exact(3) {
            if triangle.iter().all(|&i| i < limit) {
                self.indices.extend(triangle.iter().map(|&i| base + i));
            }
        }
    }

    /// An ink stroke from `vellum-ink`, whose positions are in the stroke's own
    /// space.
    pub fn push_ink(&mut self, mesh: &vellum_ink::Mesh, color: Rgba, transform: u32) {
        self.push_indexed(&mesh.positions, &mesh.indices, color, transform);
    }

    /// A connector from `vellum-connect`.
    ///
    /// Its vertices are relative to `Mesh::origin`, an `f64` world point the caller
    /// must fold into `transform`'s translation — in `f64`, before the cast, which is
    /// the whole reason the connector crate carries the origin separately.
    pub fn push_connector(&mut self, mesh: &vellum_connect::Mesh, color: Rgba, transform: u32) {
        self.push_indexed(&mesh.vertices, &mesh.indices, color, transform);
    }

    /// A tessellated shape fill from `vellum-shapes`, in unit-box space. Pair with
    /// [`MeshTransform::unit_box`].
    pub fn push_shape_fill(&mut self, mesh: &vellum_shapes::Mesh, color: Rgba, transform: u32) {
        self.push_indexed(&mesh.vertices, &mesh.indices, color, transform);
    }

    /// A tessellated shape outline from `vellum-shapes`, expanded to `line_width`.
    ///
    /// The stroke mesh stores the point *on the path* plus an offset normal rather
    /// than the widened position, precisely so the width can be chosen here — a
    /// border can change thickness, or hold a constant number of screen pixels while
    /// the board zooms, without re-tessellating. `line_width` is in the same unit-box
    /// space the mesh is in.
    pub fn push_shape_stroke(
        &mut self,
        mesh: &vellum_shapes::StrokeMesh,
        line_width: f32,
        color: Rgba,
        transform: u32,
    ) {
        let half = line_width * 0.5;
        let expanded: Vec<[f32; 2]> = mesh
            .vertices
            .iter()
            .map(|v| {
                [
                    v.position[0] + v.normal[0] * half,
                    v.position[1] + v.normal[1] * half,
                ]
            })
            .collect();
        self.push_indexed(&expanded, &mesh.indices, color, transform);
    }
}

const ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
    0 => Float32x2,  // position
    1 => Unorm8x4,   // color
    2 => Uint32,     // transform
];

pub(crate) fn layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<MeshVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &ATTRIBUTES,
    }
}

pub(crate) fn transform_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vellum-mesh-transform-layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    })
}

pub(crate) fn pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    view_layout: &wgpu::BindGroupLayout,
    transform_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    pipeline::build(
        device,
        &PipelineDescriptor {
            label: "vellum-mesh",
            source: pipeline::shader_source!("shaders/mesh.wgsl"),
            vertex_entry: "vs_mesh",
            fragment_entry: "fs_mesh",
            format,
            buffers: &[layout()],
            bind_group_layouts: &[view_layout, transform_layout],
            topology: wgpu::PrimitiveTopology::TriangleList,
            samples: crate::pipeline::BOARD_SAMPLES,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    #[test]
    fn the_vertex_layout_matches_the_attributes() {
        assert_eq!(size_of::<MeshVertex>(), 16);
        assert_eq!(size_of::<MeshTransform>(), 32);
        let offsets: Vec<_> = ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 12]);
        assert_eq!(layout().array_stride, 16);
    }

    #[test]
    fn the_identity_transform_leaves_a_point_alone() {
        assert_eq!(MeshTransform::IDENTITY.apply([3.0, -7.0]), [3.0, -7.0]);
        assert_eq!(MeshTransform::at([10.0, 20.0]).apply([1.0, 2.0]), [11.0, 22.0]);
    }

    #[test]
    fn a_quarter_turn_rotates_clockwise_in_a_y_down_space() {
        let t = MeshTransform::scale_rotate_at(1.0, std::f32::consts::FRAC_PI_2, [0.0, 0.0]);
        // +x turns to +y, which is downwards on a board.
        assert!(close(t.apply([1.0, 0.0]), [0.0, 1.0]), "{:?}", t.apply([1.0, 0.0]));
        assert!(close(t.apply([0.0, 1.0]), [-1.0, 0.0]));
    }

    #[test]
    fn scaling_and_rotation_compose_without_shear() {
        let t = MeshTransform::scale_rotate_at(3.0, 0.7, [5.0, -2.0]);
        let a = t.apply([1.0, 0.0]);
        let b = t.apply([0.0, 1.0]);
        let da = [a[0] - 5.0, a[1] + 2.0];
        let db = [b[0] - 5.0, b[1] + 2.0];
        assert!((da[0].hypot(da[1]) - 3.0).abs() < 1e-4);
        assert!((db[0].hypot(db[1]) - 3.0).abs() < 1e-4);
        assert!((da[0] * db[0] + da[1] * db[1]).abs() < 1e-4, "axes must stay perpendicular");
    }

    #[test]
    fn the_unit_box_maps_onto_the_item_rect() {
        let t = MeshTransform::unit_box([100.0, 50.0], [400.0, 200.0], 0.0);
        assert!(close(t.apply([0.0, 0.0]), [100.0, 50.0]));
        assert!(close(t.apply([1.0, 1.0]), [500.0, 250.0]));
        assert!(close(t.apply([0.5, 0.5]), [300.0, 150.0]));
    }

    /// A rotated shape must turn about its own centre, not its corner — otherwise a
    /// rotated widget drifts away from the box the scene layer culls it by.
    #[test]
    fn a_rotated_unit_box_turns_about_its_centre() {
        let t = MeshTransform::unit_box([100.0, 50.0], [400.0, 200.0], 0.9);
        assert!(close(t.apply([0.5, 0.5]), [300.0, 150.0]), "{:?}", t.apply([0.5, 0.5]));
        let corner = t.apply([0.0, 0.0]);
        let radius = (corner[0] - 300.0).hypot(corner[1] - 150.0);
        assert!((radius - 200.0f32.hypot(100.0)).abs() < 1e-3, "{radius}");
    }

    #[test]
    fn a_pushed_mesh_rebases_its_indices() {
        let mut batch = MeshBatch::new();
        let t = batch.push_transform(MeshTransform::IDENTITY);
        batch.push_indexed(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], &[0, 1, 2], Rgba::WHITE, t);
        batch.push_indexed(&[[2.0, 2.0], [3.0, 2.0], [2.0, 3.0]], &[0, 1, 2], Rgba::BLACK, t);
        assert_eq!(batch.indices(), &[0, 1, 2, 3, 4, 5]);
        assert_eq!(batch.vertices().len(), 6);
        assert_eq!(batch.triangle_count(), 2);
        assert_eq!(batch.vertices()[0].color, [255; 4]);
        assert_eq!(batch.vertices()[3].color, [0, 0, 0, 255]);
    }

    /// An index past the end of the vertex array is an out-of-bounds fetch on the
    /// GPU. Dropping the triangle is the only response that cannot corrupt anything.
    #[test]
    fn an_out_of_range_index_is_dropped_rather_than_uploaded() {
        let mut batch = MeshBatch::new();
        let t = batch.push_transform(MeshTransform::IDENTITY);
        batch.push_indexed(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], &[0, 1, 2, 0, 1, 9], Rgba::WHITE, t);
        assert_eq!(batch.indices(), &[0, 1, 2]);
    }

    #[test]
    fn degenerate_meshes_add_nothing() {
        let mut batch = MeshBatch::new();
        let t = batch.push_transform(MeshTransform::IDENTITY);
        batch.push_indexed(&[], &[0, 1, 2], Rgba::WHITE, t);
        batch.push_indexed(&[[0.0, 0.0]], &[0, 0], Rgba::WHITE, t);
        assert!(batch.is_empty());
        // A trailing partial triangle is ignored, not read past.
        batch.push_indexed(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], &[0, 1, 2, 0, 1], Rgba::WHITE, t);
        assert_eq!(batch.triangle_count(), 1);
    }

    #[test]
    fn a_transform_can_be_rewritten_without_touching_the_geometry() {
        let mut batch = MeshBatch::new();
        let t = batch.push_transform(MeshTransform::IDENTITY);
        batch.push_indexed(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], &[0, 1, 2], Rgba::WHITE, t);
        let before = batch.vertices().to_vec();

        assert!(batch.set_transform(t, MeshTransform::at([500.0, -900.0])));
        assert_eq!(batch.vertices(), before.as_slice());
        assert_eq!(batch.transforms()[t as usize].translation, [500.0, -900.0]);
        assert!(!batch.set_transform(7, MeshTransform::IDENTITY));
    }

    /// The whole ink pipeline, end to end: a real stroke through `vellum-ink` and
    /// into the batch, because "one vertex format" is only true if the real producers
    /// fit it.
    #[test]
    fn a_real_ink_stroke_batches() {
        let stroke = vellum_ink::Stroke::from_miro(
            &[(0.0, 0.0), (40.0, 30.0), (90.0, 10.0), (120.0, 60.0)],
            Some(6.0),
        );
        let mesh = stroke.render(vellum_ink::Lod::new(1.0)).unwrap();
        assert!(!mesh.is_empty());

        let mut batch = MeshBatch::new();
        let t = batch.push_transform(MeshTransform::at([1000.0, 2000.0]));
        batch.push_ink(&mesh, Rgba::from_hex(0x1a_1a1a), t);

        assert_eq!(batch.vertices().len(), mesh.positions.len());
        assert_eq!(batch.indices().len(), mesh.indices.len());
        assert!(batch.indices().iter().all(|&i| (i as usize) < batch.vertices().len()));
    }

    #[test]
    fn a_real_connector_batches() {
        use vellum_connect::{Anchor, Connector, ConnectorStyle, Endpoint, Point, Router, WidgetBounds, WidgetId};

        let widgets = vec![
            WidgetBounds::new(Point::new(0.0, 0.0), 200.0, 100.0, 0.0),
            WidgetBounds::new(Point::new(600.0, 0.0), 200.0, 100.0, 0.0),
        ];
        let connector = Connector::new(
            Endpoint::bound(WidgetId(0), Anchor::RIGHT),
            Endpoint::bound(WidgetId(1), Anchor::LEFT),
            ConnectorStyle::default(),
        );
        let path = Router::default().route(&connector, &widgets, &[]).unwrap();
        let mesh = vellum_connect::tessellate(
            &path,
            &connector.style,
            &vellum_connect::TessellationOptions::default(),
        )
        .unwrap();

        let mut batch = MeshBatch::new();
        // The connector's `f64` origin is folded into the translation by the caller,
        // which is where the camera subtraction belongs.
        let t = batch.push_transform(MeshTransform::at([mesh.origin.x as f32, mesh.origin.y as f32]));
        batch.push_connector(&mesh, Rgba::BLACK, t);
        assert_eq!(batch.triangle_count(), mesh.triangle_count());
    }

    /// A tessellated shape and its outline, from the real catalogue.
    #[test]
    fn a_tessellated_shape_and_its_outline_batch() {
        let outline = vellum_shapes::Shape::Cloud.outline(1.0);
        let options = vellum_shapes::TessellationOptions::for_size(400.0);
        let shape = outline.tessellate(options).unwrap();

        let mut batch = MeshBatch::new();
        let t = batch.push_transform(MeshTransform::unit_box([0.0, 0.0], [400.0, 300.0], 0.0));
        batch.push_shape_fill(&shape.fill, Rgba::WHITE, t);
        let fill_vertices = batch.vertices().len();
        assert_eq!(fill_vertices, shape.fill.vertices.len());

        batch.push_shape_stroke(&shape.stroke, 0.02, Rgba::BLACK, t);
        assert_eq!(batch.vertices().len(), fill_vertices + shape.stroke.vertices.len());
        assert!(batch.indices().iter().all(|&i| (i as usize) < batch.vertices().len()));
    }

    /// The stroke mesh's normals are what let the width be chosen at draw time; check
    /// that widening actually moves the vertices off the path.
    #[test]
    fn widening_a_stroke_moves_its_vertices_off_the_path() {
        let outline = vellum_shapes::Shape::Rectangle.outline(1.0);
        let stroke = outline
            .tessellate_stroke(vellum_shapes::TessellationOptions::default())
            .unwrap();

        let mut thin = MeshBatch::new();
        let t = thin.push_transform(MeshTransform::IDENTITY);
        thin.push_shape_stroke(&stroke, 0.01, Rgba::BLACK, t);

        let mut thick = MeshBatch::new();
        let t = thick.push_transform(MeshTransform::IDENTITY);
        thick.push_shape_stroke(&stroke, 0.2, Rgba::BLACK, t);

        assert_eq!(thin.vertices().len(), thick.vertices().len());
        let moved = thin
            .vertices()
            .iter()
            .zip(thick.vertices())
            .filter(|(a, b)| a.position != b.position)
            .count();
        assert!(moved > 0, "a wider stroke must place its vertices differently");
    }

    #[test]
    fn clearing_resets_the_transform_table_too() {
        let mut batch = MeshBatch::new();
        batch.push_transform(MeshTransform::at([1.0, 2.0]));
        batch.push_indexed(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], &[0, 1, 2], Rgba::WHITE, 0);
        batch.clear();
        assert!(batch.is_empty());
        assert!(batch.transforms().is_empty());
        assert_eq!(batch.push_transform(MeshTransform::IDENTITY), 0);
    }
}
