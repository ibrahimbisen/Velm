//! The offscreen canvas the board is drawn into, and the blit that carries it to
//! the swapchain.
//!
//! # Why the board no longer goes straight to the screen
//!
//! `vellum_render::glass`'s module documentation is explicit about the frame it
//! needs: the translucent chrome of `docs/05-design-language.md` §3a *reads what has
//! already been drawn*, so the board has to land in a texture the blur pass can
//! sample rather than on a swapchain image, which is write-only. That is a change to
//! the shape of a frame, not a feature switch, so it is unconditional:
//!
//! ```text
//!   pass 1   board          → canvas texture
//!   (no pass) blur          → canvas texture, downsampled, behind the panels only
//!   pass 2   blit canvas    → swapchain
//!            glass          → swapchain
//!            egui chrome    → swapchain
//! ```
//!
//! The cost of the extra hop is one full-screen textured triangle — no blend, no
//! filtering, one texel fetched per pixel. On the reference board that is well under
//! a tenth of a millisecond, against the 0.5 ms §3a budgets for the material as a
//! whole, and it buys the frame structure every floating surface depends on.
//!
//! A blit rather than `copy_texture_to_texture`: the copy would need `COPY_DST` on
//! the swapchain, which is a surface capability rather than a guarantee, and a
//! fallback path that only runs on the adapters nobody tests on is worse than one
//! path that always runs.

use crate::chrome_pass::texture_bind_group_layout;

/// The board's offscreen target, and the pipeline that puts it on screen.
pub struct Compositor {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Kept so a resize can compare rather than reconfigure blindly.
    size: (u32, u32),
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// The multisampled attachment the board is rasterised into, resolved into `view`.
    /// See `vellum_render::BOARD_SAMPLES`.
    samples: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    /// The `locals` group the shared shader module declares at group 0. The blit's
    /// entry points do not read it, but the pipeline layout has to describe it.
    locals_layout: wgpu::BindGroupLayout,
    locals_group: wgpu::BindGroup,
}

impl Compositor {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vellum-compositor"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/chrome.wgsl").into()),
        });

        let locals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vellum-compositor-locals-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let locals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vellum-compositor-locals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let locals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vellum-compositor-locals"),
            layout: &locals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: locals.as_entire_binding() }],
        });

        let layout = texture_bind_group_layout(device, "vellum-compositor-layout");
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vellum-compositor"),
            bind_group_layouts: &[Some(&locals_layout), Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vellum-compositor"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_blit"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_blit"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // The canvas is opaque and covers the frame. Replacing rather than
                    // blending is both cheaper and the only thing that can be correct:
                    // the swapchain's previous contents are undefined.
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vellum-compositor-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Nearest, because this is a 1:1 copy. Linear would cost the same and
            // soften the board by half a texel for no reason at all.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let size = (width.max(1), height.max(1));
        let (texture, view, bind_group) =
            allocate(device, format, size, &layout, &sampler);
        let samples = multisample(device, format, size);

        Self {
            pipeline,
            layout,
            sampler,
            size,
            texture,
            view,
            samples,
            bind_group,
            locals_layout,
            locals_group,
        }
    }

    /// The texture the board is drawn into and the blur reads back.
    ///
    /// Single-sampled: it is the **resolve** target now, not the attachment the board draws
    /// into. See [`Self::board_attachment`].
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Where the board's own pass should render, and where it should resolve to.
    ///
    /// Returns `(view, resolve_target)`. The board is rasterised into a multisampled
    /// attachment and resolved down into [`Self::view`], so the blur, the blit and the
    /// read-back downstream see exactly the single-sampled canvas they always have — none of
    /// them needed changing. See `vellum_render::BOARD_SAMPLES` for why only the board is
    /// multisampled and what it costs.
    pub fn board_attachment(&self) -> (&wgpu::TextureView, Option<&wgpu::TextureView>) {
        (&self.samples, Some(&self.view))
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Reallocates the canvas when the window changed size. A no-op otherwise, and a
    /// no-op for a zero dimension, which is a minimise rather than a resize.
    pub fn resize(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        if width == 0 || height == 0 || self.size == (width, height) {
            return;
        }
        self.size = (width, height);
        let (texture, view, bind_group) =
            allocate(device, format, self.size, &self.layout, &self.sampler);
        self.texture = texture;
        self.view = view;
        self.bind_group = bind_group;
        // The multisampled attachment is the same size as the canvas by definition, so it
        // has to be reallocated with it — a stale one is a pass that fails validation rather
        // than one that draws the wrong thing, but only after the next resize.
        self.samples = multisample(device, format, self.size);
    }

    /// Draws the canvas over the whole of the caller's pass.
    pub fn blit(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.locals_group, &[]);
        pass.set_bind_group(1, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// The bind group layout the locals live in, exposed so a caller building its own
    /// pipeline against this shader module can match it.
    #[allow(dead_code)]
    pub(crate) fn locals_layout(&self) -> &wgpu::BindGroupLayout {
        &self.locals_layout
    }
}

impl std::fmt::Debug for Compositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compositor").field("size", &self.size).finish()
    }
}

/// The multisampled colour attachment the board is rasterised into.
///
/// Transient by design: `RENDER_ATTACHMENT` only, no `TEXTURE_BINDING` and no `COPY_SRC`.
/// Nothing ever reads it — the pass resolves it into the ordinary canvas texture and every
/// downstream stage reads *that*. Declaring the narrower usage lets a driver keep it in tile
/// memory on a TBDR GPU, which is every Apple GPU this runs on, so on the machine that
/// matters the 83MB this appears to cost is largely not paid.
fn multisample(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    size: (u32, u32),
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("vellum-canvas-msaa"),
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: vellum_render::BOARD_SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn allocate(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
) -> (wgpu::Texture, wgpu::TextureView, wgpu::BindGroup) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vellum-canvas"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        // `RENDER_ATTACHMENT` to draw the board into, `TEXTURE_BINDING` for the blit
        // and for the glass blur, `COPY_SRC` so a thumbnail or a PNG export can read
        // the frame back without rendering it a second time.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vellum-canvas"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    });
    (texture, view, bind_group)
}
