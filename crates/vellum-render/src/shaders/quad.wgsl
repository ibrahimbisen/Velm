// The hot path: one instanced rounded rectangle. Stickies, frames, cards, selection
// rects and every piece of chrome are this and nothing else.

struct QuadInstance {
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) fill: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) corner_radii: vec4<f32>,
    // border width, rotation in radians, opacity, unused.
    @location(5) style: vec4<f32>,
};

struct QuadVertex {
    @builtin(position) clip_position: vec4<f32>,
    // Item-local: origin at the quad's centre, y down, unrotated. This is the space
    // the distance field is evaluated in, so rotation costs the fragment stage
    // nothing.
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half_extent: vec2<f32>,
    @location(2) @interpolate(flat) fill: vec4<f32>,
    @location(3) @interpolate(flat) border: vec4<f32>,
    @location(4) @interpolate(flat) corner_radii: vec4<f32>,
    @location(5) @interpolate(flat) style: vec4<f32>,
};

@vertex
fn vs_quad(@builtin(vertex_index) vertex_index: u32, instance: QuadInstance) -> QuadVertex {
    // `abs` because a rect dragged right-to-left arrives with a negative size, and
    // the centre-plus-half-extent form has to survive that rather than vanish.
    let half_extent = abs(instance.size) * 0.5;
    let margin = units_per_pixel();
    let local = (unit_corner(vertex_index) * 2.0 - 1.0) * (half_extent + margin);
    let centre = instance.origin + instance.size * 0.5;

    var out: QuadVertex;
    out.clip_position = to_clip(centre + rotate(local, instance.style.y));
    out.local = local;
    out.half_extent = half_extent;
    out.fill = instance.fill;
    out.border = instance.border;
    out.corner_radii = instance.corner_radii;
    out.style = instance.style;
    return out;
}

@fragment
fn fs_quad(in: QuadVertex) -> @location(0) vec4<f32> {
    let footprint = pixel_footprint(in.local);
    let distance = sd_rounded_box(in.local, in.half_extent, in.corner_radii);
    return fill_and_border(distance, footprint, in.fill, in.border, in.style.x, in.style.z);
}
