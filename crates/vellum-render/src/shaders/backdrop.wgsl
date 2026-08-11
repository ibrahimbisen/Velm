// The backdrop capture and blur behind glass chrome.
//
// Three passes share one vertex stage, because all three draw the same thing: one
// instanced quad per *region* — the rectangle of the canvas behind one panel. A
// region is a rectangle in the backdrop atlas at the destination and a rectangle in
// whatever texture is being read at the source, and nothing else about the three
// passes differs on the vertex side.
//
// Deliberately NOT prefixed with `common.wgsl`. These passes have no camera, no view
// transform and no signed distance field; they write axis-aligned texel rectangles in
// normalised device coordinates. Binding a view uniform they never read would be a
// bind group per pass for nothing.

struct BackdropParams {
    // The destination texture's size in texels. The vertex stage divides by it to
    // reach clip space; nothing else needs it.
    dst_size: vec2<f32>,
    // One source texel in normalised coordinates. Every tap offset below is stated
    // in source texels and scaled by this, so the same shader serves a full-
    // resolution canvas and a quarter-resolution atlas.
    src_texel: vec2<f32>,
    // Kawase tap offset, in source texels. Read by `fs_kawase` only.
    offset: f32,
    // Bilinear taps per axis for the box downsample. Read by `fs_downsample` only.
    taps: f32,
    padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> params: BackdropParams;

@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

struct RegionInstance {
    // Destination rectangle in the atlas, in texels: origin.xy, size.zw.
    @location(0) dst: vec4<f32>,
    // Source rectangle in the read texture's normalised coordinates: min.xy, max.zw.
    @location(1) src: vec4<f32>,
};

struct RegionVertex {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) src: vec4<f32>,
};

@vertex
fn vs_region(@builtin(vertex_index) vertex_index: u32, instance: RegionInstance) -> RegionVertex {
    let corner = vec2<f32>(f32(vertex_index & 1u), f32((vertex_index >> 1u) & 1u));
    let unit = (instance.dst.xy + corner * instance.dst.zw) / params.dst_size;

    var out: RegionVertex;
    // `+y` is down in atlas texels and up in clip space, so the y term is flipped
    // here rather than by loading the atlas upside down later.
    out.clip_position = vec4<f32>(unit.x * 2.0 - 1.0, 1.0 - unit.y * 2.0, 0.0, 1.0);
    out.uv = mix(instance.src.xy, instance.src.zw, corner);
    out.src = instance.src;
    return out;
}

// Full resolution to quarter resolution, in one exact box filter.
//
// A single bilinear tap across a 4x reduction would read four of the sixteen texels
// it is meant to average and alias the other twelve into noise — which on a board of
// hairlines and small text is the worst possible input to a blur. Instead `taps`
// samples per axis land on texel *corners*, so each one is an exact average of 2 x 2,
// and 2 x 2 of them box-filter the whole 4 x 4 block with four samples.
@fragment
fn fs_downsample(in: RegionVertex) -> @location(0) vec4<f32> {
    // Clamped to the source texture rather than to the region. A panel near the edge
    // of the window has a region that hangs off it, and edge-extending the canvas is
    // the right answer: sampling past it would darken the material along that edge.
    let lo = 0.5 * params.src_texel;
    let hi = max(lo, vec2<f32>(1.0) - lo);

    let n = i32(params.taps);
    var sum = vec4<f32>(0.0);
    for (var j = 0; j < n; j = j + 1) {
        for (var i = 0; i < n; i = i + 1) {
            let step = (vec2<f32>(f32(i), f32(j)) * 2.0 + 1.0 - params.taps) * params.src_texel;
            sum = sum + textureSampleLevel(
                source_texture,
                source_sampler,
                clamp(in.uv + step, lo, hi),
                0.0,
            );
        }
    }
    return sum / (params.taps * params.taps);
}

// One Kawase pass: four bilinear taps on the diagonals, at `offset` source texels.
//
// Offsets are half-integers, so each tap is an equal blend of two neighbouring
// texels and the 1D kernel of a pass at offset k + 0.5 is four equal weights at
// ±k and ±(k+1). Variance therefore adds across passes as (k² + (k+1)²) / 2, which
// is what lets two passes stand in for a Gaussian several times wider than either.
@fragment
fn fs_kawase(in: RegionVertex) -> @location(0) vec4<f32> {
    // Clamped to this region's own rectangle. Regions of different panels are
    // adjacent in the atlas and hold unrelated pixels; without the clamp the blur
    // would drag one panel's backdrop into the next one's edge.
    let lo = in.src.xy + 0.5 * params.src_texel;
    let hi = max(lo, in.src.zw - 0.5 * params.src_texel);
    let d = params.offset * params.src_texel;

    var sum = textureSampleLevel(
        source_texture, source_sampler, clamp(in.uv + vec2<f32>(-d.x, -d.y), lo, hi), 0.0);
    sum = sum + textureSampleLevel(
        source_texture, source_sampler, clamp(in.uv + vec2<f32>(d.x, -d.y), lo, hi), 0.0);
    sum = sum + textureSampleLevel(
        source_texture, source_sampler, clamp(in.uv + vec2<f32>(-d.x, d.y), lo, hi), 0.0);
    sum = sum + textureSampleLevel(
        source_texture, source_sampler, clamp(in.uv + vec2<f32>(d.x, d.y), lo, hi), 0.0);
    return sum * 0.25;
}
