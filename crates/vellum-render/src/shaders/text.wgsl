// Glyph quads sampled from the atlas.
//
// No distance field and no rounding: a glyph is a rasterised bitmap placed at an
// integer device pixel, and the whole point of rasterising per subpixel phase is
// that the GPU then does a 1:1 texel-to-pixel blit. Filtering it would undo that.

struct GlyphInstance {
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    // uv_min.xy, uv_max.xy of the atlas slot.
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
    // 0 = coverage, tinted by `color`; 1 = colour bitmap, drawn as-is.
    @location(4) flags: vec4<u32>,
};

struct GlyphVertex {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
    @location(2) @interpolate(flat) flags: vec4<u32>,
};

@group(1) @binding(0) var atlas_texture: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

const GLYPH_COLOR_BITMAP: u32 = 1u;

@vertex
fn vs_glyph(@builtin(vertex_index) vertex_index: u32, instance: GlyphInstance) -> GlyphVertex {
    let corner = unit_corner(vertex_index);

    var out: GlyphVertex;
    out.clip_position = to_clip(instance.origin + corner * instance.size);
    // Corner (0,0) is the quad's top-left and maps to uv_min, whose v is the slot's
    // *first* row. Atlas rows are stored top-down and v increases downwards, so the
    // glyph arrives the way up it was rasterised. Getting this pair backwards is the
    // classic upside-down-text bug, and `glyphs_are_not_vertically_flipped` in
    // `tests/pixels.rs` is what stops it coming back.
    out.uv = mix(instance.uv.xy, instance.uv.zw, corner);
    out.color = instance.color;
    out.flags = instance.flags;
    return out;
}

@fragment
fn fs_glyph(in: GlyphVertex) -> @location(0) vec4<f32> {
    let texel = textureSample(atlas_texture, atlas_sampler, in.uv);
    if (in.flags.x == GLYPH_COLOR_BITMAP) {
        // Premultiplied on upload; `color.a` carries the run's opacity only.
        return texel * in.color.a;
    }
    let alpha = texel.r * in.color.a;
    return vec4<f32>(in.color.rgb * alpha, alpha);
}
