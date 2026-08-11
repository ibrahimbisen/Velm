// Indexed triangles from `vellum-ink`, `vellum-shapes` and `vellum-connect`.
//
// Each vertex names a transform rather than carrying a baked world position. That is
// what lets one vertex buffer survive a pan: the camera moves, a few dozen floats of
// transform are rewritten, and the megabyte of ink geometry is not touched.

struct MeshTransform {
    // Column-major 2x2: (m00, m10, m01, m11).
    matrix: vec4<f32>,
    // In the view's own space — camera-relative world px, never absolute.
    translation: vec2<f32>,
    padding: vec2<f32>,
};

@group(1) @binding(0) var<storage, read> transforms: array<MeshTransform>;

struct MeshVertexIn {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) transform: u32,
};

struct MeshVertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_mesh(vertex: MeshVertexIn) -> MeshVertexOut {
    let t = transforms[vertex.transform];
    let placed = vec2<f32>(
        t.matrix.x * vertex.position.x + t.matrix.z * vertex.position.y,
        t.matrix.y * vertex.position.x + t.matrix.w * vertex.position.y,
    ) + t.translation;

    var out: MeshVertexOut;
    out.clip_position = to_clip(placed);
    out.color = vertex.color;
    return out;
}

@fragment
fn fs_mesh(in: MeshVertexOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
}
