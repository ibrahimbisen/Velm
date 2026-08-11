//! Instanced rounded rectangles — the pipeline that draws most of a board.
//!
//! A sticky, a frame, a card, a table cell, a selection rect and every piece of
//! chrome are all the same four vertices with different parameters, so they are
//! instances of one pipeline and cost one draw call between them however many there
//! are. There is no vertex buffer at all: the corners come from
//! `@builtin(vertex_index)`, so the only per-frame traffic is the instance array.
//!
//! This is a strict superset of the single-colour quad `vellum-app` shipped with —
//! colour, corner radius, border colour and width, opacity and rotation are all
//! per-instance — and a plain filled rectangle still costs exactly the same draw.
//!
//! # Why this exists next to [`crate::shape`]
//!
//! The SDF pipeline can draw a rounded box too. This one is kept because a rounded
//! box is what the *majority* of items on a real board are: its instance is 80 bytes
//! against 96, it needs no storage-buffer bind, and it takes no branch per fragment.
//! On the reference board that is 44 stickies and 12 frames against 47 shapes.

use crate::color::Rgba;
use crate::pipeline::{self, PipelineDescriptor};

/// One rounded rectangle. Mirrored by `QuadInstance` in `shaders/quad.wgsl` and by
/// [`ATTRIBUTES`]; all three change together.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct QuadInstance {
    /// Top-left corner. Camera-relative world pixels for board content, physical
    /// pixels for overlays — never absolute world coordinates, which do not survive
    /// the trip through `f32` (see `vellum_scene::camera`).
    pub origin: [f32; 2],
    /// May be negative on either axis; the shader normalises, so a rect dragged
    /// right-to-left still draws.
    pub size: [f32; 2],
    pub fill: Rgba,
    /// Only visible where `border_width > 0`.
    pub border: Rgba,
    /// CSS order: top-left, top-right, bottom-right, bottom-left. Clamped in the
    /// shader to the shorter half-extent.
    pub corner_radii: [f32; 4],
    /// Drawn *inside* the edge, as CSS `border-box` and Miro both do, so a bordered
    /// item never overflows the bounds the scene layer culls and hit-tests against.
    pub border_width: f32,
    /// Radians, clockwise about the rect's centre — `+y` is down, so a positive
    /// angle turns the same way it does in Miro's `rotation` field.
    pub rotation: f32,
    /// Scales both the fill's and the border's alpha.
    pub opacity: f32,
    /// Pads the instance to a `vec4` boundary so the WGSL `style` member can be one
    /// attribute rather than three.
    padding: f32,
}

impl QuadInstance {
    /// A flat fill with square corners and no border — the payload
    /// `vellum_scene::RenderPayload::SolidQuad` describes.
    pub fn solid(origin: [f32; 2], size: [f32; 2], fill: Rgba) -> Self {
        Self {
            origin,
            size,
            fill,
            border: Rgba::TRANSPARENT,
            corner_radii: [0.0; 4],
            border_width: 0.0,
            rotation: 0.0,
            opacity: 1.0,
            padding: 0.0,
        }
    }

    pub fn with_corner_radius(mut self, radius: f32) -> Self {
        self.corner_radii = [radius; 4];
        self
    }

    pub fn with_corner_radii(mut self, radii: [f32; 4]) -> Self {
        self.corner_radii = radii;
        self
    }

    pub fn with_border(mut self, color: Rgba, width: f32) -> Self {
        self.border = color;
        self.border_width = width;
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

    /// True when nothing this instance could draw would be visible. Filtering these
    /// out is cheaper than rasterising a fully transparent quad, and a board full of
    /// borderless transparent frames is a real shape for a Miro import to have.
    pub fn is_invisible(&self) -> bool {
        self.opacity <= 0.0
            || (self.fill.is_invisible() && (self.border.is_invisible() || self.border_width <= 0.0))
    }
}

const ATTRIBUTES: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x2,  // origin
    1 => Float32x2,  // size
    2 => Float32x4,  // fill
    3 => Float32x4,  // border
    4 => Float32x4,  // corner_radii
    5 => Float32x4,  // border_width, rotation, opacity, padding
];

pub(crate) fn layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<QuadInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

pub(crate) fn pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    view_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    pipeline::build(
        device,
        &PipelineDescriptor {
            label: "vellum-quad",
            source: pipeline::shader_source!("shaders/quad.wgsl"),
            vertex_entry: "vs_quad",
            fragment_entry: "fs_quad",
            format,
            buffers: &[layout()],
            bind_group_layouts: &[view_layout],
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            samples: crate::pipeline::BOARD_SAMPLES,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout is stated in three places — this struct, [`ATTRIBUTES`] and the
    /// WGSL. A mismatch produces garbled geometry rather than an error, so pin the
    /// size and every offset.
    #[test]
    fn the_instance_layout_matches_the_vertex_attributes() {
        assert_eq!(size_of::<QuadInstance>(), 80);
        let offsets: Vec<_> = ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 16, 32, 48, 64]);
        for (i, attribute) in ATTRIBUTES.iter().enumerate() {
            assert_eq!(attribute.shader_location, i as u32);
        }
        assert_eq!(layout().array_stride, 80);
    }

    #[test]
    fn a_solid_quad_has_no_border_and_full_opacity() {
        let quad = QuadInstance::solid([10.0, 20.0], [100.0, 50.0], Rgba::from_hex(0xff_f79e));
        assert_eq!(quad.corner_radii, [0.0; 4]);
        assert_eq!(quad.border_width, 0.0);
        assert_eq!(quad.opacity, 1.0);
        assert!(!quad.is_invisible());
    }

    #[test]
    fn the_builders_compose() {
        let quad = QuadInstance::solid([0.0, 0.0], [200.0, 100.0], Rgba::WHITE)
            .with_corner_radius(8.0)
            .with_border(Rgba::BLACK, 2.0)
            .with_rotation(0.5)
            .with_opacity(0.25);
        assert_eq!(quad.corner_radii, [8.0; 4]);
        assert_eq!((quad.border, quad.border_width), (Rgba::BLACK, 2.0));
        assert_eq!((quad.rotation, quad.opacity), (0.5, 0.25));
    }

    /// A transparent fill with a visible border is still worth drawing — that is
    /// exactly what a frame outline is — while a transparent fill with a zero-width
    /// border is not.
    #[test]
    fn invisibility_accounts_for_the_border() {
        let clear = QuadInstance::solid([0.0; 2], [10.0; 2], Rgba::TRANSPARENT);
        assert!(clear.is_invisible());
        assert!(!clear.with_border(Rgba::BLACK, 1.0).is_invisible());
        assert!(clear.with_border(Rgba::BLACK, 0.0).is_invisible());
        assert!(clear.with_border(Rgba::TRANSPARENT, 4.0).is_invisible());
        assert!(
            QuadInstance::solid([0.0; 2], [10.0; 2], Rgba::WHITE)
                .with_opacity(0.0)
                .is_invisible()
        );
    }
}
