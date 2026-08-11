//! Textured quads.
//!
//! One instance draws one image, or one *region* of one image: the UV sub-rect is
//! part of the instance, so a crop is four floats and no upload at all. That is the
//! implementation the feature catalogue promises for images — "crop is a UV-rect, so
//! it stays free" — and it is also what will let an icon sheet or a video frame
//! atlas share a texture without any new pipeline.
//!
//! A batch is one texture. The renderer therefore splits image draws on texture
//! change and nothing else, which is why [`crate::DrawList`] keeps the caller's
//! ordering: images that share a texture and sit next to each other in z coalesce.

use crate::color::Rgba;
use crate::pipeline::{self, PipelineDescriptor};
use crate::texture::UvRect;

/// One textured quad. Mirrored by `ImageInstance` in `shaders/image.wgsl` and by
/// [`ATTRIBUTES`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ImageInstance {
    /// Top-left corner, in the view's space.
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// `uv_min.xy, uv_max.xy`.
    uv: [f32; 4],
    /// Multiplies the sampled colour. White leaves the image alone; a colour here is
    /// how a shape-masked or duotoned image is drawn without a second pipeline.
    pub tint: Rgba,
    /// CSS order. Rounds the quad, which is how an image in a shape mask or a
    /// rounded card avatar is drawn.
    pub corner_radii: [f32; 4],
    /// `rotation, opacity, padding, padding`.
    style: [f32; 4],
}

impl ImageInstance {
    pub fn new(origin: [f32; 2], size: [f32; 2], crop: UvRect) -> Self {
        Self {
            origin,
            size,
            uv: crop.packed(),
            tint: Rgba::WHITE,
            corner_radii: [0.0; 4],
            style: [0.0, 1.0, 0.0, 0.0],
        }
    }

    pub fn crop(&self) -> UvRect {
        UvRect::new([self.uv[0], self.uv[1]], [self.uv[2], self.uv[3]])
    }

    pub fn with_crop(mut self, crop: UvRect) -> Self {
        self.uv = crop.packed();
        self
    }

    pub fn with_tint(mut self, tint: Rgba) -> Self {
        self.tint = tint;
        self
    }

    pub fn with_corner_radius(mut self, radius: f32) -> Self {
        self.corner_radii = [radius; 4];
        self
    }

    pub fn with_rotation(mut self, radians: f32) -> Self {
        self.style[0] = radians;
        self
    }

    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.style[1] = opacity;
        self
    }

    pub fn rotation(&self) -> f32 {
        self.style[0]
    }

    pub fn opacity(&self) -> f32 {
        self.style[1]
    }

    pub fn is_invisible(&self) -> bool {
        self.opacity() <= 0.0 || self.tint.is_invisible()
    }
}

const ATTRIBUTES: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32x2,  // origin
    1 => Float32x2,  // size
    2 => Float32x4,  // uv
    3 => Float32x4,  // tint
    4 => Float32x4,  // corner_radii
    5 => Float32x4,  // style
];

pub(crate) fn layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<ImageInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

pub(crate) fn pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    view_layout: &wgpu::BindGroupLayout,
    texture_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    pipeline::build(
        device,
        &PipelineDescriptor {
            label: "vellum-image",
            source: pipeline::shader_source!("shaders/image.wgsl"),
            vertex_entry: "vs_image",
            fragment_entry: "fs_image",
            format,
            buffers: &[layout()],
            bind_group_layouts: &[view_layout, texture_layout],
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            samples: crate::pipeline::BOARD_SAMPLES,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instance_layout_matches_the_vertex_attributes() {
        assert_eq!(size_of::<ImageInstance>(), 80);
        let offsets: Vec<_> = ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 16, 32, 48, 64]);
        assert_eq!(layout().array_stride, 80);
    }

    #[test]
    fn a_new_instance_shows_the_whole_image_opaquely() {
        let image = ImageInstance::new([0.0, 0.0], [100.0, 50.0], UvRect::FULL);
        assert_eq!(image.crop(), UvRect::FULL);
        assert_eq!(image.tint, Rgba::WHITE);
        assert_eq!((image.rotation(), image.opacity()), (0.0, 1.0));
        assert!(!image.is_invisible());
    }

    /// The crop must survive the round trip through the packed form, because it is
    /// the only place the sub-rect exists — nothing re-derives it.
    #[test]
    fn a_crop_round_trips_through_the_packed_uv() {
        let crop = UvRect::from_pixels(64, 32, 128, 64, (256, 128));
        let image = ImageInstance::new([0.0; 2], [10.0; 2], UvRect::FULL).with_crop(crop);
        assert_eq!(image.crop(), crop);
        assert_eq!(image.crop().min, [0.25, 0.25]);
        assert_eq!(image.crop().max, [0.75, 0.75]);
    }

    #[test]
    fn the_builders_compose() {
        let image = ImageInstance::new([0.0; 2], [10.0; 2], UvRect::FULL)
            .with_tint(Rgba::from_hex(0x00_80ff))
            .with_corner_radius(4.0)
            .with_rotation(1.5)
            .with_opacity(0.5);
        assert_eq!(image.corner_radii, [4.0; 4]);
        assert_eq!((image.rotation(), image.opacity()), (1.5, 0.5));
        assert!(!image.is_invisible());
        assert!(image.with_opacity(0.0).is_invisible());
        assert!(image.with_tint(Rgba::TRANSPARENT).is_invisible());
    }
}
