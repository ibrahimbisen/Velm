//! Analytic shapes: everything `vellum_shapes::SdfParams` can describe, in one
//! pipeline.
//!
//! `docs/01-architecture.md` §3 puts SDF shapes on the critical path — a shape drawn
//! from a distance field is one instanced quad, resolution-independent at any zoom,
//! with the border falling out of the same distance value and no triangles at all.
//! `vellum-shapes` already decides *which* shapes qualify and documents why; this
//! module is the other half of that contract, evaluating the same four formulas on
//! the GPU. They are transcribed literally into `shaders/common.wgsl` and
//! `shaders/shape.wgsl` so the Rust reference implementation the shape crate tests
//! against is the same arithmetic the shader runs.
//!
//! ## One pipeline, four kinds
//!
//! Box, rounded box, ellipse and polygon share a pipeline and therefore a draw call.
//! The kind is an instance field and the fragment shader switches on it: the value is
//! constant across a primitive, so it costs a well-predicted branch, where four
//! pipelines would cost three state changes per frame and split the instance array
//! four ways.
//!
//! ## Polygons live in a storage buffer
//!
//! A polygon's vertex count varies, so its vertices cannot ride in the instance.
//! They are concatenated into one [`PolygonArena`] and addressed by offset and
//! length, which keeps all 47 shapes of the reference board in a single bind.

use crate::color::Rgba;
use crate::pipeline::{self, PipelineDescriptor};
use std::collections::HashMap;
use vellum_shapes::SdfParams;

/// Fill, border and per-item modifiers, shared by [`ShapeInstance`] and the image
/// pipeline's rounded-rect mask.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeStyle {
    pub fill: Rgba,
    pub border: Rgba,
    /// Drawn inside the edge, so a bordered shape stays within the bounds the scene
    /// layer culls and hit-tests against.
    pub border_width: f32,
    /// Radians, clockwise about the shape's centre.
    pub rotation: f32,
    pub opacity: f32,
}

impl Default for ShapeStyle {
    /// Nothing drawn. A style is almost always built from one of the constructors
    /// below, and defaulting to an opaque colour would make a forgotten field render
    /// as a surprise rectangle rather than as nothing.
    fn default() -> Self {
        Self {
            fill: Rgba::TRANSPARENT,
            border: Rgba::TRANSPARENT,
            border_width: 0.0,
            rotation: 0.0,
            opacity: 1.0,
        }
    }
}

impl ShapeStyle {
    pub fn filled(fill: Rgba) -> Self {
        Self { fill, ..Self::default() }
    }

    pub fn outlined(border: Rgba, border_width: f32) -> Self {
        Self { border, border_width, ..Self::default() }
    }

    pub fn with_border(mut self, border: Rgba, border_width: f32) -> Self {
        self.border = border;
        self.border_width = border_width;
        self
    }

    pub fn with_rotation(mut self, radians: f32) -> Self {
        self.rotation = radians;
        self
    }

    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    pub fn is_invisible(&self) -> bool {
        self.opacity <= 0.0
            || (self.fill.is_invisible() && (self.border.is_invisible() || self.border_width <= 0.0))
    }

    fn packed(&self) -> [f32; 4] {
        [self.border_width, self.rotation, self.opacity, 0.0]
    }
}

/// Which formula the fragment shader evaluates. Values are duplicated as the
/// `KIND_*` constants in `shaders/shape.wgsl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum Kind {
    Box = 0,
    RoundedBox = 1,
    Ellipse = 2,
    Polygon = 3,
}

/// One analytic shape. Mirrored by `ShapeInstance` in `shaders/shape.wgsl` and by
/// [`ATTRIBUTES`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ShapeInstance {
    /// The shape's centre — camera-relative world pixels, or physical pixels for an
    /// overlay. `vellum_shapes` puts the origin of item-local space at the centre,
    /// so this is where that origin lands.
    pub centre: [f32; 2],
    /// Half the rasterised quad, before the antialiasing margin the shader adds. Must
    /// enclose the shape or its edges are clipped.
    pub half_extent: [f32; 2],
    /// Rounded box: corner radii, CSS order. Ellipse: radii in `xy`. Unused by the
    /// box and polygon forms.
    pub params: [f32; 4],
    pub fill: Rgba,
    pub border: Rgba,
    /// `border_width, rotation, opacity, padding`.
    style: [f32; 4],
    /// `kind, first polygon vertex, polygon vertex count, padding`.
    shape: [u32; 4],
}

impl ShapeInstance {
    pub fn rectangle(centre: [f32; 2], half_extent: [f32; 2], style: &ShapeStyle) -> Self {
        Self::new(Kind::Box, centre, half_extent, [0.0; 4], [0; 2], style)
    }

    pub fn rounded_rectangle(
        centre: [f32; 2],
        half_extent: [f32; 2],
        corner_radii: [f32; 4],
        style: &ShapeStyle,
    ) -> Self {
        Self::new(Kind::RoundedBox, centre, half_extent, corner_radii, [0; 2], style)
    }

    pub fn ellipse(centre: [f32; 2], radii: [f32; 2], style: &ShapeStyle) -> Self {
        Self::new(
            Kind::Ellipse,
            centre,
            radii,
            [radii[0], radii[1], 0.0, 0.0],
            [0; 2],
            style,
        )
    }

    /// Builds the instance for whatever `params` describes, interning any polygon
    /// vertices into `arena`.
    ///
    /// This is the whole bridge from `vellum-shapes` to the GPU: 41 shapes, one call.
    pub fn from_params(
        params: &SdfParams,
        centre: [f32; 2],
        style: &ShapeStyle,
        arena: &mut PolygonArena,
    ) -> Self {
        match params {
            SdfParams::Box { half_extent } => Self::rectangle(centre, *half_extent, style),
            SdfParams::RoundedBox { half_extent, corner_radii } => {
                Self::rounded_rectangle(centre, *half_extent, *corner_radii, style)
            }
            SdfParams::Ellipse { radii } => Self::ellipse(centre, *radii, style),
            SdfParams::Polygon { vertices } => {
                let (first, count) = arena.intern(vertices);
                // The quad has to enclose the polygon, and a polygon's vertices are
                // already centred on the item, so its half extent is the largest
                // absolute coordinate on each axis. Deriving it rather than trusting
                // a caller-supplied size means a shape whose silhouette overhangs its
                // nominal box — which no catalogue shape does today — would still be
                // drawn whole rather than clipped.
                let half_extent = vertices.iter().fold([0.0f32, 0.0f32], |acc, v| {
                    [acc[0].max(v.x.abs()), acc[1].max(v.y.abs())]
                });
                Self::new(Kind::Polygon, centre, half_extent, [0.0; 4], [first, count], style)
            }
        }
    }

    fn new(
        kind: Kind,
        centre: [f32; 2],
        half_extent: [f32; 2],
        params: [f32; 4],
        polygon: [u32; 2],
        style: &ShapeStyle,
    ) -> Self {
        Self {
            centre,
            half_extent,
            params,
            fill: style.fill,
            border: style.border,
            style: style.packed(),
            shape: [kind as u32, polygon[0], polygon[1], 0],
        }
    }

    pub fn is_invisible(&self) -> bool {
        let style = ShapeStyle {
            fill: self.fill,
            border: self.border,
            border_width: self.style[0],
            rotation: self.style[1],
            opacity: self.style[2],
        };
        style.is_invisible()
    }
}

/// Every polygon a frame draws, concatenated into one storage buffer.
///
/// Identical vertex runs are shared. A board repeats a handful of shapes hundreds of
/// times — 47 shapes across ~12 distinct silhouettes on the reference board — so
/// interning turns the arena from "per item" into "per distinct shape and size",
/// which is small enough to upload whole every frame without thinking about it.
#[derive(Debug, Clone, Default)]
pub struct PolygonArena {
    vertices: Vec<[f32; 2]>,
    /// Keyed on the vertices' bit patterns rather than their values: two runs are
    /// interchangeable only if they are byte-identical, and `f32` is not `Hash`.
    runs: HashMap<Vec<[u32; 2]>, (u32, u32)>,
}

impl PolygonArena {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `vertices` and returns `(first, count)`, reusing an identical run that
    /// is already present. An empty polygon interns to a zero-length run, which the
    /// shader draws as nothing.
    pub fn intern(&mut self, vertices: &[vellum_shapes::Point]) -> (u32, u32) {
        if vertices.is_empty() {
            return (0, 0);
        }
        let key: Vec<[u32; 2]> = vertices.iter().map(|v| [v.x.to_bits(), v.y.to_bits()]).collect();
        if let Some(&run) = self.runs.get(&key) {
            return run;
        }
        let run = (self.vertices.len() as u32, vertices.len() as u32);
        self.vertices.extend(vertices.iter().map(|v| [v.x, v.y]));
        self.runs.insert(key, run);
        run
    }

    pub fn vertices(&self) -> &[[f32; 2]] {
        &self.vertices
    }

    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    pub fn clear(&mut self) {
        self.vertices.clear();
        self.runs.clear();
    }
}

const ATTRIBUTES: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x2,  // centre
    1 => Float32x2,  // half_extent
    2 => Float32x4,  // params
    3 => Float32x4,  // fill
    4 => Float32x4,  // border
    5 => Float32x4,  // style
    6 => Uint32x4,   // shape
];

pub(crate) fn layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<ShapeInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

pub(crate) fn polygon_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vellum-polygon-layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
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
    polygon_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    pipeline::build(
        device,
        &PipelineDescriptor {
            label: "vellum-shape",
            source: pipeline::shader_source!("shaders/shape.wgsl"),
            vertex_entry: "vs_shape",
            fragment_entry: "fs_shape",
            format,
            buffers: &[layout()],
            bind_group_layouts: &[view_layout, polygon_layout],
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            samples: crate::pipeline::BOARD_SAMPLES,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_shapes::{Shape, Size, p};

    #[test]
    fn the_instance_layout_matches_the_vertex_attributes() {
        assert_eq!(size_of::<ShapeInstance>(), 96);
        let offsets: Vec<_> = ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 16, 32, 48, 64, 80]);
        assert_eq!(layout().array_stride, 96);
    }

    /// The kind values are duplicated in WGSL, where nothing can check them. Pin them
    /// here so a reordering of the enum has to be a deliberate edit in both places.
    #[test]
    fn the_kind_codes_match_the_shader_constants() {
        assert_eq!(Kind::Box as u32, 0);
        assert_eq!(Kind::RoundedBox as u32, 1);
        assert_eq!(Kind::Ellipse as u32, 2);
        assert_eq!(Kind::Polygon as u32, 3);
    }

    #[test]
    fn every_analytic_shape_in_the_catalogue_becomes_an_instance() {
        let mut arena = PolygonArena::new();
        let style = ShapeStyle::filled(Rgba::WHITE);
        let mut analytic = 0;
        for shape in vellum_shapes::CATALOGUE {
            let Some(params) = shape.sdf_params(Size::new(200.0, 120.0)) else {
                continue;
            };
            analytic += 1;
            let instance = ShapeInstance::from_params(&params, [0.0, 0.0], &style, &mut arena);
            assert!(instance.half_extent[0] > 0.0 && instance.half_extent[1] > 0.0, "{shape:?}");
            if instance.shape[0] == Kind::Polygon as u32 {
                assert!(instance.shape[2] >= 3, "a polygon needs three vertices: {shape:?}");
                let end = (instance.shape[1] + instance.shape[2]) as usize;
                assert!(end <= arena.vertices().len(), "{shape:?} addresses past the arena");
            }
        }
        // The catalogue is 41 shapes and most of them are straight-edged.
        assert!(analytic > 20, "only {analytic} analytic shapes");
    }

    #[test]
    fn a_polygons_quad_encloses_its_vertices() {
        let params = Shape::Diamond.sdf_params(Size::new(400.0, 100.0)).unwrap();
        let mut arena = PolygonArena::new();
        let instance =
            ShapeInstance::from_params(&params, [0.0; 2], &ShapeStyle::filled(Rgba::WHITE), &mut arena);
        assert_eq!(instance.half_extent, [200.0, 50.0]);
        for v in arena.vertices() {
            assert!(v[0].abs() <= instance.half_extent[0] + 1e-3);
            assert!(v[1].abs() <= instance.half_extent[1] + 1e-3);
        }
    }

    #[test]
    fn an_ellipse_carries_its_radii_in_both_places() {
        let params = Shape::Ellipse.sdf_params(Size::new(300.0, 200.0)).unwrap();
        let mut arena = PolygonArena::new();
        let instance =
            ShapeInstance::from_params(&params, [5.0, 6.0], &ShapeStyle::filled(Rgba::WHITE), &mut arena);
        assert_eq!(instance.shape[0], Kind::Ellipse as u32);
        assert_eq!(instance.half_extent, [150.0, 100.0]);
        assert_eq!(instance.params, [150.0, 100.0, 0.0, 0.0]);
        assert!(arena.is_empty(), "an ellipse needs no polygon storage");
    }

    /// The reason the arena interns: 400 diamonds of the same size are one run of
    /// four vertices, not 1600 vertices.
    #[test]
    fn identical_polygons_share_one_run() {
        let mut arena = PolygonArena::new();
        let diamond = [p(0.0, -1.0), p(1.0, 0.0), p(0.0, 1.0), p(-1.0, 0.0)];
        let first = arena.intern(&diamond);
        let second = arena.intern(&diamond);
        assert_eq!(first, second);
        assert_eq!(arena.vertices().len(), 4);

        let other = [p(0.0, -2.0), p(2.0, 0.0), p(0.0, 2.0), p(-2.0, 0.0)];
        let (offset, count) = arena.intern(&other);
        assert_eq!((offset, count), (4, 4));
        assert_eq!(arena.vertices().len(), 8);
    }

    #[test]
    fn clearing_the_arena_resets_the_offsets() {
        let mut arena = PolygonArena::new();
        arena.intern(&[p(0.0, 0.0), p(1.0, 0.0), p(0.0, 1.0)]);
        arena.clear();
        assert!(arena.is_empty());
        assert_eq!(arena.intern(&[p(0.0, 0.0), p(1.0, 0.0), p(0.0, 1.0)]), (0, 3));
    }

    #[test]
    fn an_invisible_style_is_recognised_through_the_instance() {
        let clear = ShapeStyle::default();
        assert!(ShapeInstance::rectangle([0.0; 2], [10.0; 2], &clear).is_invisible());
        assert!(!ShapeInstance::rectangle([0.0; 2], [10.0; 2], &ShapeStyle::outlined(Rgba::BLACK, 1.0))
            .is_invisible());
        assert!(
            ShapeInstance::rectangle([0.0; 2], [10.0; 2], &ShapeStyle::filled(Rgba::WHITE).with_opacity(0.0))
                .is_invisible()
        );
    }
}
