// Shared by every pipeline in this crate: the view transform, the vertex-index unit
// quad, the signed distance functions, and the coverage rule that makes an analytic
// edge antialiased. Prepended to each pipeline's own source at compile time, so a
// change here reaches all five at once and none of them can drift.

struct View {
    // `xy` scales the incoming position, `zw` translates it, mapping the view's own
    // space onto clip space. One uniform serves board content (camera-relative world
    // px) and screen overlays (physical px); only the numbers differ.
    scale_translate: vec4<f32>,
    // `x` is how many of the view's units one screen pixel spans: 1/zoom for board
    // content, 1.0 for overlays. Antialiasing needs the pixel's size in the space
    // the distance field is evaluated in, and it also sizes the margin a shape's
    // quad grows by so the outer half of an edge's gradient has somewhere to land.
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> view: View;

fn units_per_pixel() -> f32 {
    return view.params.x;
}

fn to_clip(position: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(position * view.scale_translate.xy + view.scale_translate.zw, 0.0, 1.0);
}

// Unit-quad corners for a 4-vertex triangle strip: (0,0) (1,0) (0,1) (1,1). Derived
// from the vertex index so none of the instanced pipelines needs a vertex buffer at
// all — the only per-frame traffic is the instance array.
fn unit_corner(vertex_index: u32) -> vec2<f32> {
    return vec2<f32>(f32(vertex_index & 1u), f32((vertex_index >> 1u) & 1u));
}

fn rotate(v: vec2<f32>, radians: f32) -> vec2<f32> {
    let c = cos(radians);
    let s = sin(radians);
    return vec2<f32>(v.x * c - v.y * s, v.x * s + v.y * c);
}

// How many local units one screen pixel covers, measured from the interpolated local
// coordinate itself. Screen-space derivatives are the only formulation that stays
// correct under an arbitrary instance rotation *and* an arbitrary camera zoom, which
// is what "resolution independent" has to mean on a board that zooms 0.01x to 64x.
//
// Every call site is at the top of a fragment entry point, in uniform control flow —
// never inside a loop whose trip count comes from instance data, which is undefined.
fn pixel_footprint(local: vec2<f32>) -> f32 {
    let dx = dpdx(local);
    let dy = dpdy(local);
    return sqrt(max(dot(dx, dx), dot(dy, dy)));
}

// Coverage of the half-plane `distance <= 0`, given `footprint` local units per
// pixel. A linear ramp is the box filter of an edge sweeping across a pixel: it is
// *exact* for an axis-aligned edge, so a pixel-aligned rectangle still resolves to a
// hard boundary rather than a smeared one, and it degrades gracefully everywhere
// else. A smoothstep would soften those edges for no gain.
fn coverage(distance: f32, footprint: f32) -> f32 {
    if (footprint <= 0.0) {
        return select(0.0, 1.0, distance <= 0.0);
    }
    return clamp(0.5 - distance / footprint, 0.0, 1.0);
}

// The formulas below are transcribed from `vellum_shapes::sdf`, which documents each
// one and asserts in Rust that its sign agrees with the tessellated outline. Keeping
// the WGSL a literal transcription is what makes that agreement mean anything here.

fn sd_box(p: vec2<f32>, half_extent: vec2<f32>) -> f32 {
    let d = abs(p) - half_extent;
    return length(max(d, vec2<f32>(0.0))) + min(max(d.x, d.y), 0.0);
}

// `radii` is in CSS order — top-left, top-right, bottom-right, bottom-left — with y
// downwards. Clamped to the half extent so a caller cannot ask for a corner larger
// than the box, which would invert the inset rectangle the formula is built on.
fn sd_rounded_box(p: vec2<f32>, half_extent: vec2<f32>, radii: vec4<f32>) -> f32 {
    var r = select(radii.x, radii.y, p.x > 0.0);
    if (p.y > 0.0) {
        r = select(radii.w, radii.z, p.x > 0.0);
    }
    r = clamp(r, 0.0, min(half_extent.x, half_extent.y));
    let q = abs(p) - half_extent + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

// The standard first-order approximation. Its *sign* is exact — it reduces to the
// algebraic inside test |p/r| < 1 — and only the magnitude is approximate, which
// here affects nothing but the width of a sub-pixel antialiasing ramp.
fn sd_ellipse(p: vec2<f32>, radii: vec2<f32>) -> f32 {
    if (radii.x <= 0.0 || radii.y <= 0.0) {
        return 1.0e30;
    }
    let k1 = length(p / radii);
    let k2 = length(p / (radii * radii));
    if (k2 <= 0.0) {
        // The centre is the singularity of the approximation; the distance there is
        // simply the shorter radius.
        return -min(radii.x, radii.y);
    }
    return k1 * (k1 - 1.0) / k2;
}

// Fill and border composited into a **premultiplied** colour.
//
// Premultiplied output, and premultiplied blending in every pipeline here, is what
// turns this composite into a sum instead of a division by a total alpha that can be
// zero. It is also the only form in which a mipmapped or bilinearly filtered texture
// blends correctly at all, so the two decisions are really one.
//
// The border is drawn *inside* the edge, matching CSS `box-sizing: border-box` and
// Miro: growing outwards would make a bordered shape overflow the bounds the scene
// layer culls and hit-tests against.
fn fill_and_border(
    distance: f32,
    footprint: f32,
    fill: vec4<f32>,
    border: vec4<f32>,
    border_width: f32,
    opacity: f32,
) -> vec4<f32> {
    let outer = coverage(distance, footprint);
    let inner = coverage(distance + max(border_width, 0.0), footprint);
    let fill_alpha = inner * fill.a * opacity;
    let border_alpha = max(outer - inner, 0.0) * border.a * opacity;
    return vec4<f32>(
        fill.rgb * fill_alpha + border.rgb * border_alpha,
        fill_alpha + border_alpha,
    );
}
