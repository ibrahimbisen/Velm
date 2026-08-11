//! Views: the mapping from a coordinate space onto clip space, and the pixel size
//! that antialiasing in that space needs.
//!
//! A frame draws in at least two spaces. Board content arrives in **camera-relative
//! world pixels** — `f64` world coordinates with the camera's origin already
//! subtracted, because `f32` alone quantises the reference board's 41282 px extent
//! coarsely enough to shimmer at deep zoom (`vellum_scene::camera` measures it).
//! Screen chrome arrives in physical pixels and must not move when the camera does.
//!
//! Both are one `vec4` of scale-and-translate, so they share every pipeline. They are
//! held in a single uniform buffer addressed by dynamic offset, which is what lets a
//! frame interleave board and overlay draws without a bind group per view or a
//! mid-pass uniform rewrite (which silently applies the last value to both).

use crate::buffer::{GrowableBuffer, Growth};
use vellum_scene::{Camera, ClipTransform, ScreenSize};

/// One coordinate space a frame draws in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub transform: ClipTransform,
    /// How many of this space's units one screen pixel spans.
    ///
    /// `1/zoom` for board content, `1.0` for screen overlays. Antialiasing needs the
    /// pixel's size in the space the distance field is evaluated in — a shape does
    /// not know the camera, only how big a pixel is next to it.
    pub units_per_pixel: f32,
}

impl View {
    /// The board view for `camera`: camera-relative world pixels.
    pub fn board(camera: &Camera) -> Self {
        Self {
            transform: camera.clip_transform(),
            units_per_pixel: (1.0 / camera.zoom()) as f32,
        }
    }

    /// The overlay view: physical screen pixels, origin top-left, `+y` down.
    pub fn screen(viewport: ScreenSize) -> Self {
        Self {
            transform: ClipTransform::screen_pixels(viewport),
            units_per_pixel: 1.0,
        }
    }
}

/// The uniform handed to every vertex shader, matching `View` in `common.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    scale_translate: [f32; 4],
    /// `x` is `units_per_pixel`; the rest is reserved. A `vec4` rather than a bare
    /// `f32` because WGSL aligns uniform struct members to 16 bytes and a `vec4` gets
    /// that without an `@align` a reader would have to know to look for.
    params: [f32; 4],
}

impl From<View> for ViewUniform {
    fn from(v: View) -> Self {
        Self {
            scale_translate: [
                v.transform.scale[0],
                v.transform.scale[1],
                v.transform.translate[0],
                v.transform.translate[1],
            ],
            // A non-positive or non-finite value would make every antialiased edge
            // either hard or NaN; a zero-sized viewport produces exactly that for one
            // frame when a window is minimised.
            params: [
                if v.units_per_pixel.is_finite() && v.units_per_pixel > 0.0 {
                    v.units_per_pixel
                } else {
                    1.0
                },
                0.0,
                0.0,
                0.0,
            ],
        }
    }
}

/// The per-view uniform buffer and the bind group every pipeline binds at group 0.
pub(crate) struct Views {
    buffer: GrowableBuffer,
    layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    /// Distance between consecutive views in the buffer, rounded up to the adapter's
    /// dynamic-offset alignment (256 bytes on every backend that matters).
    stride: u32,
    staging: Vec<u8>,
}

impl Views {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vellum-view-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(size_of::<ViewUniform>() as u64),
                },
                count: None,
            }],
        });

        let stride = align_to(
            size_of::<ViewUniform>() as u32,
            device.limits().min_uniform_buffer_offset_alignment,
        );
        let buffer = GrowableBuffer::new(
            device,
            "vellum-view-uniforms",
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            u64::from(stride) * INITIAL_VIEWS,
        );
        let bind_group = bind(device, &layout, buffer.buffer());

        Self {
            buffer,
            layout,
            bind_group,
            stride,
            staging: Vec::new(),
        }
    }

    pub(crate) fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    pub(crate) fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    pub(crate) fn offset(&self, index: u32) -> wgpu::DynamicOffset {
        index * self.stride
    }

    pub(crate) fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, views: &[View]) {
        let stride = self.stride as usize;
        self.staging.clear();
        self.staging.resize(stride * views.len(), 0);
        for (i, view) in views.iter().enumerate() {
            let uniform = ViewUniform::from(*view);
            let start = i * stride;
            self.staging[start..start + size_of::<ViewUniform>()]
                .copy_from_slice(bytemuck::bytes_of(&uniform));
        }
        if self.buffer.write(device, queue, &self.staging) == Growth::Reallocated {
            self.bind_group = bind(device, &self.layout, self.buffer.buffer());
        }
    }
}

/// Board and overlay, which is what a frame needs before any panel exists.
const INITIAL_VIEWS: u64 = 4;

fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vellum-view-bind-group"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer,
                offset: 0,
                size: wgpu::BufferSize::new(size_of::<ViewUniform>() as u64),
            }),
        }],
    })
}

fn align_to(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment.max(1)) * alignment.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_scene::{ScreenPoint, WorldPoint};

    #[test]
    fn the_uniform_is_two_vec4s() {
        assert_eq!(size_of::<ViewUniform>(), 32);
    }

    #[test]
    fn a_board_view_carries_the_reciprocal_of_the_zoom() {
        let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        camera.set_zoom_about(4.0, ScreenPoint::new(800.0, 450.0));
        let view = View::board(&camera);
        assert_eq!(view.units_per_pixel, 0.25);
        assert_eq!(view.transform, camera.clip_transform());
    }

    #[test]
    fn a_screen_view_measures_one_unit_per_pixel() {
        let view = View::screen(ScreenSize::new(1600.0, 900.0));
        assert_eq!(view.units_per_pixel, 1.0);
        assert_eq!(view.transform.apply([0.0, 0.0]), [-1.0, 1.0]);
        assert_eq!(view.transform.apply([1600.0, 900.0]), [1.0, -1.0]);
    }

    /// A minimised window produces a zero-sized viewport for one frame. The
    /// antialiasing width must stay finite and positive through it, or every edge on
    /// the board turns into NaN geometry on the way back.
    #[test]
    fn a_degenerate_view_falls_back_to_one_unit_per_pixel() {
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let uniform = ViewUniform::from(View {
                transform: ClipTransform { scale: [1.0, 1.0], translate: [0.0, 0.0] },
                units_per_pixel: bad,
            });
            assert_eq!(uniform.params[0], 1.0, "units_per_pixel {bad}");
        }
    }

    /// The camera at maximum zoom on the far corner of the reference board: the
    /// uniform must still be exactly representable, because it is the multiplier
    /// applied to every vertex.
    #[test]
    fn the_uniform_survives_the_reference_board_at_full_zoom() {
        let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        camera.set_center(WorldPoint::new(41_282.89, 17_515.36));
        camera.set_zoom_about(64.0, ScreenPoint::new(800.0, 450.0));
        let uniform = ViewUniform::from(View::board(&camera));
        assert!(uniform.scale_translate.iter().all(|v| v.is_finite()));
        assert_eq!(uniform.params[0], 1.0 / 64.0);
    }

    #[test]
    fn alignment_rounds_up_and_tolerates_a_zero_alignment() {
        assert_eq!(align_to(32, 256), 256);
        assert_eq!(align_to(256, 256), 256);
        assert_eq!(align_to(257, 256), 512);
        assert_eq!(align_to(32, 0), 32);
    }
}
