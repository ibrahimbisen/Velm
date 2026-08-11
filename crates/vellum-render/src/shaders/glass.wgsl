// The translucent chrome material of `docs/05-design-language.md` §3a.
//
// One instanced rounded rectangle per floating panel, composited from five things in
// this order: the blurred backdrop, a saturation lift, a colour tint, a 1px hairline,
// and a single specular catch along the **top edge only**.
//
// That last one is the whole point. A blur with a tint over it is a smear; what makes
// the Apple material read as a physical pane is the 1px inner line at the top picking
// up light. It is one `mix` and it is not optional.

struct GlassInstance {
    @location(0) origin: vec2<f32>,
    @location(1) size: vec2<f32>,
    // The panel's own rectangle inside the backdrop atlas: min.xy, max.zw. Degenerate
    // when `flags.x` says there is no backdrop to sample.
    @location(2) uv: vec4<f32>,
    // rgb is the tint colour; `a` is how much of it is laid over the backdrop —
    // 0.72 in light mode, 0.68 in dark, per §3a. Not the panel's own alpha.
    @location(3) tint: vec4<f32>,
    @location(4) border: vec4<f32>,
    // The top-edge specular: white at about 14%.
    @location(5) highlight: vec4<f32>,
    @location(6) corner_radii: vec4<f32>,
    // border_width, highlight_width, saturation, fill_alpha.
    @location(7) style: vec4<f32>,
    // sample_backdrop, opacity, unused, unused.
    @location(8) flags: vec4<f32>,
};

struct GlassVertex {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half_extent: vec2<f32>,
    @location(2) @interpolate(flat) uv: vec4<f32>,
    @location(3) @interpolate(flat) tint: vec4<f32>,
    @location(4) @interpolate(flat) border: vec4<f32>,
    @location(5) @interpolate(flat) highlight: vec4<f32>,
    @location(6) @interpolate(flat) corner_radii: vec4<f32>,
    @location(7) @interpolate(flat) style: vec4<f32>,
    @location(8) @interpolate(flat) flags: vec4<f32>,
};

@group(1) @binding(0) var backdrop_texture: texture_2d<f32>;
@group(1) @binding(1) var backdrop_sampler: sampler;

@vertex
fn vs_glass(@builtin(vertex_index) vertex_index: u32, instance: GlassInstance) -> GlassVertex {
    let half_extent = abs(instance.size) * 0.5;
    // A pixel of margin so the outer half of the edge's coverage ramp has somewhere
    // to land, exactly as the quad and image pipelines do it.
    let margin = units_per_pixel();
    let local = (unit_corner(vertex_index) * 2.0 - 1.0) * (half_extent + margin);
    let centre = instance.origin + instance.size * 0.5;

    var out: GlassVertex;
    out.clip_position = to_clip(centre + local);
    out.local = local;
    out.half_extent = half_extent;
    out.uv = instance.uv;
    out.tint = instance.tint;
    out.border = instance.border;
    out.highlight = instance.highlight;
    out.corner_radii = instance.corner_radii;
    out.style = instance.style;
    out.flags = instance.flags;
    return out;
}

// The outward unit normal of the same rounded box `sd_rounded_box` measures.
//
// Exact rather than a finite difference, and it is what confines the specular to the
// top edge: `-n.y` is 1 along the straight top, 0 along the sides and the bottom, and
// falls off through the corners on its own. A y-band would instead paint a straight
// line across the corner curves, where the real edge has already turned downwards.
fn rounded_box_normal(p: vec2<f32>, half_extent: vec2<f32>, radii: vec4<f32>) -> vec2<f32> {
    var r = select(radii.x, radii.y, p.x > 0.0);
    if (p.y > 0.0) {
        r = select(radii.w, radii.z, p.x > 0.0);
    }
    r = clamp(r, 0.0, min(half_extent.x, half_extent.y));
    let q = abs(p) - half_extent + r;
    let s = sign(p);

    if (max(q.x, q.y) > 0.0) {
        // In a corner's quarter-disc, or on a straight run where one component of
        // `m` is zero and this reduces to an axis-aligned normal anyway.
        // `max(q.x, q.y) > 0` guarantees `length_m > 0`; the guard is there so a
        // denormal cannot divide by zero and hand back a NaN normal.
        let m = max(q, vec2<f32>(0.0));
        let length_m = length(m);
        return select(vec2<f32>(0.0), s * m / length_m, length_m > 0.0);
    }
    // Inside the inset rectangle: the nearest edge is the one with the larger `q`.
    return select(s * vec2<f32>(0.0, 1.0), s * vec2<f32>(1.0, 0.0), q.x > q.y);
}

// Pulls colour back out of the backdrop.
//
// A blur averages neighbouring hues towards their mean, which is grey; without this
// a sticky-note yellow under the toolbar reads as beige. §3a asks for a *slight*
// lift, so this stays close to 1 — a large value would posterise the board rather
// than keep it alive.
fn lift_saturation(rgb: vec3<f32>, amount: f32) -> vec3<f32> {
    // Rec. 709 luma: the same weighting the display applies, so the lift rotates
    // colour without changing perceived brightness.
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return clamp(mix(vec3<f32>(luma), rgb, amount), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_glass(in: GlassVertex) -> @location(0) vec4<f32> {
    // At the top of the entry point and in uniform control flow, which is where a
    // screen-space derivative has to be taken.
    let footprint = pixel_footprint(in.local);
    let distance = sd_rounded_box(in.local, in.half_extent, in.corner_radii);

    let border_width = max(in.style.x, 0.0);
    let highlight_width = max(in.style.y, 0.0);
    let saturation = in.style.z;
    let fill_alpha = in.style.w;
    let opacity = in.flags.y;

    var base = in.tint.rgb;
    if (in.flags.x > 0.5) {
        // The panel's rectangle maps onto its sub-rect of the atlas. `unit` is
        // clamped because the quad is a pixel larger on every side than the panel.
        // `textureSampleLevel` rather than `textureSample`: the atlas has one mip
        // level, and an explicit LOD is what makes this legal inside a branch.
        let unit = clamp(
            in.local / max(in.half_extent, vec2<f32>(1.0e-6)) * 0.5 + 0.5,
            vec2<f32>(0.0),
            vec2<f32>(1.0),
        );
        let backdrop = textureSampleLevel(
            backdrop_texture,
            backdrop_sampler,
            mix(in.uv.xy, in.uv.zw, unit),
            0.0,
        ).rgb;
        base = mix(lift_saturation(backdrop, saturation), in.tint.rgb, in.tint.a);
    }

    // The specular: a 1px ring just *inside* the hairline, masked to upward-facing
    // edges. Squaring the normal's y term tightens it onto the top run instead of
    // letting it creep a third of the way down the sides.
    let inner = coverage(distance + border_width, footprint);
    let band = max(inner - coverage(distance + border_width + highlight_width, footprint), 0.0);
    let up = max(-rounded_box_normal(in.local, in.half_extent, in.corner_radii).y, 0.0);
    base = mix(base, in.highlight.rgb, band * up * up * in.highlight.a);

    // The same premultiplied composite every other pipeline here uses, so the glass
    // hairline and a quad's border cannot end up a pixel apart.
    return fill_and_border(
        distance,
        footprint,
        vec4<f32>(base, fill_alpha),
        in.border,
        border_width,
        opacity,
    );
}
