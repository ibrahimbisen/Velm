//! Pipeline construction, shared so the five pipelines cannot drift apart in the
//! settings that have to match: blend mode, culling and multisampling.
//!
//! **Premultiplied alpha, everywhere.** Every fragment shader in this crate returns
//! `rgb` already scaled by `a`. That is not a stylistic choice: compositing a fill
//! against a border, and sampling any filtered or mipmapped texture, are both only
//! correct in premultiplied form. The public API still takes straight RGBA — see
//! [`crate::Rgba`] — so the convention never escapes WGSL.

/// The WGSL prelude every pipeline's source is prefixed with.
macro_rules! shader_source {
    ($file:literal) => {
        concat!(
            include_str!("shaders/common.wgsl"),
            "\n",
            include_str!($file),
        )
    };
}

pub(crate) use shader_source;

/// How many samples the **board** is rasterised at.
///
/// # Why the board is multisampled and nothing else is
///
/// Four of the five board pipelines resolve their own edges analytically: a quad, a shape, an
/// image and a glyph all compute sub-pixel coverage from a distance field (`common.wgsl`'s
/// `coverage`) or from the glyph rasteriser's own antialiased bitmap. **The mesh pipeline
/// cannot.** It draws tessellated geometry — freehand ink, connectors, and the stroked
/// outlines of shapes lyon rather than an SDF is asked to build — where there is no distance
/// to a boundary to compute, only triangles. `fs_mesh` returns a flat colour, so its edges
/// were binary: covered or not. A 3-unit stroke at 20° with no antialiasing is a literal
/// staircase, and it was the one thing on the board drawn that way.
///
/// The user put Miro and Velm side by side and said the pen *"looks so much more pixelated,
/// so much more uglier"*. Miro draws ink into a browser canvas, which the platform
/// rasteriser antialiases as a matter of course.
///
/// Multisampling rather than a feathered outline in `vellum-ink`, for one reason that
/// decided it: the ink mesh is cached per LOD band and then **scaled on the GPU** by the
/// item's `placement.scale`, so a feather baked into the geometry would be the wrong width on
/// every imported stroke — the exact class of bug the LOD band already had. MSAA is applied
/// after the transform and cannot be wrong about it. It also fixes connectors and shape
/// outlines for free, which share the pipeline.
///
/// **Cost, on a machine with 8GB that has kernel-panicked twice:** one extra colour
/// attachment at the canvas size, `4 × 4` bytes per pixel — about 83MB at a retina window,
/// against the 268MB texture-residency budget the board's own images live in. Four rather
/// than two because 1 and 4 are the sample counts WebGPU requires every backend to support;
/// 2 and 8 are optional, so choosing either would mean a runtime capability check for a
/// difference nobody can see.
pub const BOARD_SAMPLES: u32 = 4;

/// Everything about a pipeline that differs between the five.
pub(crate) struct PipelineDescriptor<'a> {
    pub label: &'a str,
    pub source: &'a str,
    pub vertex_entry: &'a str,
    pub fragment_entry: &'a str,
    pub format: wgpu::TextureFormat,
    pub buffers: &'a [wgpu::VertexBufferLayout<'a>],
    pub bind_group_layouts: &'a [&'a wgpu::BindGroupLayout],
    pub topology: wgpu::PrimitiveTopology,
    /// Samples per pixel. [`BOARD_SAMPLES`] for anything drawn into the board's own canvas,
    /// `1` for anything drawn onto the window — the glass composite and the blit both target
    /// the swapchain, which is not multisampled, and a pipeline's sample count must match
    /// the attachment it renders into or wgpu rejects the pass.
    pub samples: u32,
}

pub(crate) fn build(device: &wgpu::Device, desc: &PipelineDescriptor<'_>) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(desc.label),
        source: wgpu::ShaderSource::Wgsl(desc.source.into()),
    });

    let buffers: Vec<Option<wgpu::VertexBufferLayout<'_>>> =
        desc.buffers.iter().cloned().map(Some).collect();
    let layouts: Vec<Option<&wgpu::BindGroupLayout>> =
        desc.bind_group_layouts.iter().copied().map(Some).collect();
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(desc.label),
        bind_group_layouts: &layouts,
        immediate_size: 0,
    });

    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(desc.label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some(desc.vertex_entry),
            compilation_options: Default::default(),
            buffers: &buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(desc.fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: desc.format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: desc.topology,
            // No culling anywhere. A quad with a negative size — a rect dragged
            // right-to-left — flips its winding, and lyon does not promise a winding
            // for a tessellated fill either. Dropping those would read as the shape
            // vanishing, which is a far worse failure than drawing a back face.
            cull_mode: None,
            ..Default::default()
        },
        // Board content is ordered back-to-front by the scene layer's fractional
        // z-index and blended; a depth buffer would fight that ordering rather than
        // help it, and cost a full-resolution attachment per frame.
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: desc.samples,
            ..wgpu::MultisampleState::default()
        },
        multiview_mask: None,
        cache: None,
    })
}
