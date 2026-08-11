// Textured quads. The UV sub-rect is what makes a crop free: cropping an image
// changes four floats in an instance and uploads nothing.

struct ImageInstance {
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    // uv_min.xy, uv_max.xy — the sub-rect of the texture this quad shows.
    @location(2) uv: vec4<f32>,
    @location(3) tint: vec4<f32>,
    @location(4) corner_radii: vec4<f32>,
    // rotation in radians, opacity, unused, unused.
    @location(5) style: vec4<f32>,
};

struct ImageVertex {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half_extent: vec2<f32>,
    @location(2) @interpolate(flat) uv: vec4<f32>,
    @location(3) @interpolate(flat) tint: vec4<f32>,
    @location(4) @interpolate(flat) corner_radii: vec4<f32>,
    @location(5) @interpolate(flat) style: vec4<f32>,
};

@group(1) @binding(0) var image_texture: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

@vertex
fn vs_image(@builtin(vertex_index) vertex_index: u32, instance: ImageInstance) -> ImageVertex {
    let half_extent = abs(instance.size) * 0.5;
    let margin = units_per_pixel();
    let local = (unit_corner(vertex_index) * 2.0 - 1.0) * (half_extent + margin);
    let centre = instance.origin + instance.size * 0.5;

    var out: ImageVertex;
    out.clip_position = to_clip(centre + rotate(local, instance.style.x));
    out.local = local;
    out.half_extent = half_extent;
    out.uv = instance.uv;
    out.tint = instance.tint;
    out.corner_radii = instance.corner_radii;
    out.style = instance.style;
    return out;
}

@fragment
fn fs_image(in: ImageVertex) -> @location(0) vec4<f32> {
    let footprint = pixel_footprint(in.local);
    let distance = sd_rounded_box(in.local, in.half_extent, in.corner_radii);
    let mask = coverage(distance, footprint);

    // The texture coordinate is derived from the *image* rect, not from the quad,
    // which is a pixel larger on each side so the outer half of the edge gradient is
    // not clipped. Clamping keeps that margin sampling the crop's own border texel
    // instead of reaching into a neighbouring sprite; the mask zeroes it anyway.
    let unit = clamp(in.local / max(in.half_extent, vec2<f32>(1.0e-6)) * 0.5 + 0.5, vec2<f32>(0.0), vec2<f32>(1.0));
    let texel = textureSample(image_texture, image_sampler, mix(in.uv.xy, in.uv.zw, unit));

    // The atlas holds premultiplied texels (see `texture.rs`), so scaling rgb and a
    // by the same factor keeps the result premultiplied, and tinting rgb alone is
    // exactly a colour multiply on the unpremultiplied value.
    let alpha = mask * in.style.y * in.tint.a;
    return vec4<f32>(texel.rgb * in.tint.rgb * alpha, texel.a * alpha);
}
