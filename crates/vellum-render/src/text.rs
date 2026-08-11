//! Glyph quads.
//!
//! # Text is drawn in screen pixels, not world pixels
//!
//! This is the one thing about the text path that is easy to get wrong and hard to
//! see afterwards. `vellum-text` rasterises a glyph at the size it will *occupy on
//! screen* — that is why [`GlyphKey`](vellum_text::GlyphKey) carries a device-pixel
//! size and a subpixel phase at all — and snaps its baseline to an integer pixel.
//! Drawing the resulting bitmap through the board's camera transform would scale it
//! a second time, undoing both the hinting and the phase, and text would be
//! permanently soft.
//!
//! So a caller pushes text into a **screen-pixel view** ([`crate::View::screen`]),
//! with the block's origin already projected through the camera, and passes the
//! camera's zoom as the layout scale. [`crate::DrawList::push_layout`] does exactly
//! that and is the intended entry point.

use crate::color::Rgba;
use crate::pipeline::{self, PipelineDescriptor};

/// One glyph. Mirrored by `GlyphInstance` in `shaders/text.wgsl` and by
/// [`ATTRIBUTES`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GlyphInstance {
    /// Top-left of the glyph's bitmap, in device pixels.
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// `uv_min.xy, uv_max.xy` of the atlas slot.
    uv: [f32; 4],
    /// Tints a coverage glyph. For a colour glyph only the alpha is used, as the
    /// run's opacity.
    pub color: Rgba,
    /// `0` = coverage, `1` = colour bitmap. The rest is padding.
    flags: [u32; 4],
}

/// Matches `GLYPH_COLOR_BITMAP` in `shaders/text.wgsl`.
const COLOR_BITMAP: u32 = 1;

impl GlyphInstance {
    /// Places `slot` with its pen at an integer device pixel.
    ///
    /// `pen` is `(PhysicalGlyph::x, PhysicalGlyph::y)` — the *baseline* origin, which
    /// `vellum_text::PlacedGlyph::physical` already snapped to the pixel grid.
    pub fn from_slot(slot: &crate::AtlasSlot, pen: (i32, i32), color: Rgba) -> Self {
        Self {
            origin: slot.quad_origin(pen),
            size: slot.quad_size(),
            uv: [slot.uv_min[0], slot.uv_min[1], slot.uv_max[0], slot.uv_max[1]],
            color,
            flags: [if slot.is_color() { COLOR_BITMAP } else { 0 }, 0, 0, 0],
        }
    }

    pub fn is_color_bitmap(&self) -> bool {
        self.flags[0] == COLOR_BITMAP
    }
}

const ATTRIBUTES: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
    0 => Float32x2,  // origin
    1 => Float32x2,  // size
    2 => Float32x4,  // uv
    3 => Float32x4,  // color
    4 => Uint32x4,   // flags
];

pub(crate) fn layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<GlyphInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

pub(crate) fn pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    view_layout: &wgpu::BindGroupLayout,
    atlas_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    pipeline::build(
        device,
        &PipelineDescriptor {
            label: "vellum-text",
            source: pipeline::shader_source!("shaders/text.wgsl"),
            vertex_entry: "vs_glyph",
            fragment_entry: "fs_glyph",
            format,
            buffers: &[layout()],
            bind_group_layouts: &[view_layout, atlas_layout],
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            samples: crate::pipeline::BOARD_SAMPLES,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AtlasKind, AtlasPage, AtlasSlot};

    fn slot(kind: AtlasKind) -> AtlasSlot {
        AtlasSlot {
            page: AtlasPage { kind, index: 0 },
            width: 12,
            height: 18,
            uv_min: [0.25, 0.5],
            uv_max: [0.3, 0.6],
            left: 2,
            top: 14,
        }
    }

    #[test]
    fn the_instance_layout_matches_the_vertex_attributes() {
        assert_eq!(size_of::<GlyphInstance>(), 64);
        let offsets: Vec<_> = ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 16, 32, 48]);
        assert_eq!(layout().array_stride, 64);
    }

    /// The quad's top-left corner maps to `uv_min`, and `uv_min.y` is the slot's
    /// first row. Pinning the pairing here is the cheap half of the check; the
    /// expensive half is `glyphs_are_not_vertically_flipped` in `tests/pixels.rs`,
    /// which reads the actual framebuffer.
    #[test]
    fn a_slot_becomes_a_quad_with_its_uv_the_right_way_up() {
        let instance = GlyphInstance::from_slot(&slot(AtlasKind::Coverage), (100, 200), Rgba::BLACK);
        assert_eq!(instance.origin, [102.0, 186.0]);
        assert_eq!(instance.size, [12.0, 18.0]);
        assert_eq!(instance.uv, [0.25, 0.5, 0.3, 0.6]);
        assert!(!instance.is_color_bitmap());
    }

    #[test]
    fn a_colour_glyph_is_flagged_so_the_shader_does_not_tint_it() {
        let instance = GlyphInstance::from_slot(&slot(AtlasKind::Color), (0, 0), Rgba::WHITE);
        assert!(instance.is_color_bitmap());
        assert_eq!(instance.flags[0], COLOR_BITMAP);
    }
}
