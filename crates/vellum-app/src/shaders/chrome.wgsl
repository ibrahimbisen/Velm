// egui's tessellated triangles, and the blit that puts the offscreen board on the
// swapchain underneath them.
//
// Both live in one module because both are "a textured quad in screen space" and
// sharing the module means sharing the bind group layouts, which is what lets the
// compositor and the chrome be two pipelines rather than two subsystems.
//
// COLOUR SPACE. `vellum-app` selects a non-sRGB `Unorm` surface format (see
// `surface::preferred_format`), so nothing here linearises and nothing gamma-encodes:
// egui hands over sRGB bytes with premultiplied alpha, the font atlas is uploaded as
// `Rgba8Unorm`, and the product of the two is written straight out. That is egui's
// `fs_main_gamma_framebuffer` case. Getting this wrong is not subtle — text comes out
// visibly too light or too dark — but it is invisible in a screenshot diff of a
// single colour, which is why it is written down.

struct Locals {
    /// The viewport in egui *points*, not pixels. egui lays out in points and the
    /// scissor rectangles are scaled to pixels on the CPU.
    screen_size: vec2<f32>,
    _padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> locals: Locals;

@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    // sRGB with premultiplied alpha, straight from `epaint::Color32`.
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_chrome(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    out.uv = uv;
    out.color = color;
    out.position = vec4<f32>(
        2.0 * position.x / locals.screen_size.x - 1.0,
        1.0 - 2.0 * position.y / locals.screen_size.y,
        0.0,
        1.0,
    );
    return out;
}

@fragment
fn fs_chrome(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color * textureSample(source_texture, source_sampler, in.uv);
}

/// The full-screen triangle that carries the offscreen canvas onto the swapchain.
///
/// A triangle rather than two triangles for a quad: it needs no vertex buffer at all,
/// and it has no diagonal seam for the rasteriser to double-shade.
@vertex
fn vs_blit(@builtin(vertex_index) index: u32) -> VertexOutput {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VertexOutput;
    out.uv = uv;
    out.color = vec4<f32>(1.0, 1.0, 1.0, 1.0);
    out.position = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    return out;
}

@fragment
fn fs_blit(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, in.uv);
}
