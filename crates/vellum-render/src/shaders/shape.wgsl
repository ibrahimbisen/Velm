// The general analytic shape: everything `vellum_shapes::SdfParams` can describe, in
// one pipeline and therefore one draw call. The kind is an instance field rather
// than four pipelines because a switch on a value that is constant across a
// primitive costs a predictable branch, while a pipeline change costs a state flush.

struct ShapeInstance {
    @location(0) centre: vec2<f32>,
    @location(1) half_extent: vec2<f32>,
    // Rounded box: corner radii in CSS order. Ellipse: radii in `xy`. Unused by the
    // box and polygon forms.
    @location(2) params: vec4<f32>,
    @location(3) fill: vec4<f32>,
    @location(4) border: vec4<f32>,
    // border width, rotation in radians, opacity, unused.
    @location(5) style: vec4<f32>,
    // kind, first polygon vertex, polygon vertex count, unused.
    @location(6) shape: vec4<u32>,
};

struct ShapeVertex {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half_extent: vec2<f32>,
    @location(2) @interpolate(flat) params: vec4<f32>,
    @location(3) @interpolate(flat) fill: vec4<f32>,
    @location(4) @interpolate(flat) border: vec4<f32>,
    @location(5) @interpolate(flat) style: vec4<f32>,
    @location(6) @interpolate(flat) shape: vec4<u32>,
};

// Every polygon on the board, concatenated. A shape addresses its own run by offset
// and length, so 47 differently-shaped widgets still cost one bind and one draw.
@group(1) @binding(0) var<storage, read> polygon_vertices: array<vec2<f32>>;

const KIND_BOX: u32 = 0u;
const KIND_ROUNDED_BOX: u32 = 1u;
const KIND_ELLIPSE: u32 = 2u;
const KIND_POLYGON: u32 = 3u;

// `d` accumulates the squared distance to the nearest edge; `s` accumulates a
// crossing-number winding test that flips the sign inside. Exact everywhere, at O(n)
// per fragment — for the twelve-or-fewer vertices these shapes have, cheaper than
// the vertex work a tessellated alternative would need.
fn sd_polygon(p: vec2<f32>, first: u32, count: u32) -> f32 {
    if (count < 3u) {
        return 1.0e30;
    }
    var squared = dot(p - polygon_vertices[first], p - polygon_vertices[first]);
    var s = 1.0;
    var j = count - 1u;
    for (var i = 0u; i < count; i = i + 1u) {
        let vi = polygon_vertices[first + i];
        let vj = polygon_vertices[first + j];
        let e = vj - vi;
        let w = p - vi;
        let b = w - e * clamp(dot(w, e) / dot(e, e), 0.0, 1.0);
        squared = min(squared, dot(b, b));
        // Named rather than packed into a `vec3<bool>`: WGSL's template
        // disambiguation cannot parse a comparison inside a templated constructor's
        // arguments, and the three conditions read better anyway.
        let below = p.y >= vi.y;
        let above = p.y < vj.y;
        let left_of_edge = e.x * w.y > e.y * w.x;
        if ((below && above && left_of_edge) || (!below && !above && !left_of_edge)) {
            s = -s;
        }
        j = i;
    }
    return s * sqrt(squared);
}

@vertex
fn vs_shape(@builtin(vertex_index) vertex_index: u32, instance: ShapeInstance) -> ShapeVertex {
    let half_extent = abs(instance.half_extent);
    let margin = units_per_pixel();
    let local = (unit_corner(vertex_index) * 2.0 - 1.0) * (half_extent + margin);

    var out: ShapeVertex;
    out.clip_position = to_clip(instance.centre + rotate(local, instance.style.y));
    out.local = local;
    out.half_extent = half_extent;
    out.params = instance.params;
    out.fill = instance.fill;
    out.border = instance.border;
    out.style = instance.style;
    out.shape = instance.shape;
    return out;
}

@fragment
fn fs_shape(in: ShapeVertex) -> @location(0) vec4<f32> {
    // Taken before the switch: the derivative must be evaluated in control flow that
    // does not depend on instance data.
    let footprint = pixel_footprint(in.local);

    var distance: f32;
    switch in.shape.x {
        case KIND_ROUNDED_BOX: {
            distance = sd_rounded_box(in.local, in.half_extent, in.params);
        }
        case KIND_ELLIPSE: {
            distance = sd_ellipse(in.local, in.params.xy);
        }
        case KIND_POLYGON: {
            distance = sd_polygon(in.local, in.shape.y, in.shape.z);
        }
        default: {
            distance = sd_box(in.local, in.half_extent);
        }
    }

    return fill_and_border(distance, footprint, in.fill, in.border, in.style.x, in.style.z);
}
