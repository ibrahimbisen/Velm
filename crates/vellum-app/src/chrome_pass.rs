//! egui's paint output, drawn on `wgpu` by this crate.
//!
//! `docs/01-architecture.md` §6a rules out `egui-wgpu`: it pins `wgpu = "29"` while
//! the workspace is on 30, and two `wgpu` versions cannot share a `Device`. Adopting
//! it would tie the graphics stack to egui's release cadence, which is the webview
//! mistake of §1 in miniature. The alternative it names is this file.
//!
//! It is not much: egui's entire output is `Vec<ClippedPrimitive>` — position, uv and
//! colour triangles with a scissor rectangle each — plus a [`egui::TexturesDelta`] for
//! the font atlas. One pipeline, one vertex buffer, one index buffer, one bind group
//! per texture.
//!
//! # Why it is not folded into `vellum-render`
//!
//! `vellum-render` deliberately has no egui dependency: `vellum-ui` builds and tests
//! on a machine with no graphics stack, and `vellum-render` renders boards for PNG
//! exports and thumbnails that have no chrome at all. Keeping the bridge in the one
//! crate that owns both is what stops either of them growing a dependency it does not
//! need. `docs/01-architecture.md` §6a says the code may live in either; it lives here.
//!
//! # Two things that are easy to get wrong
//!
//! 1. **Points versus pixels.** egui lays out in logical points; the swapchain,
//!    the scissor rectangle and `crate::input` are all in physical pixels. The vertex
//!    shader divides by the viewport *in points*, and clip rectangles are multiplied
//!    by `pixels_per_point` here. `docs/06-mouse-controls.md` §5 records what mixing
//!    the two costs elsewhere in this app; it costs the same here.
//! 2. **Colour space.** The surface is a non-sRGB `Unorm` format, so nothing
//!    linearises. See the header of `shaders/chrome.wgsl`.

use std::collections::HashMap;

use egui::epaint::{ClippedPrimitive, Primitive};

/// The prepared draw plan for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Draw {
    texture: egui::TextureId,
    indices: std::ops::Range<u32>,
    base_vertex: i32,
    /// Physical pixels, already clamped to the framebuffer: x, y, width, height.
    scissor: [u32; 4],
}

/// One texture egui asked us to hold, with its sampler and bind group.
struct Texture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    options: egui::TextureOptions,
}

/// A buffer that only ever grows, so a steady-state frame reallocates nothing.
struct Buffer {
    buffer: wgpu::Buffer,
    capacity: wgpu::BufferAddress,
    usage: wgpu::BufferUsages,
    label: &'static str,
}

impl Buffer {
    fn new(
        device: &wgpu::Device,
        label: &'static str,
        usage: wgpu::BufferUsages,
        capacity: wgpu::BufferAddress,
    ) -> Self {
        Self {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: capacity,
                usage,
                mapped_at_creation: false,
            }),
            capacity,
            usage,
            label,
        }
    }

    /// Writes `bytes`, reallocating to the next power of two when they do not fit.
    fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // `write_buffer` requires a multiple of `COPY_BUFFER_ALIGNMENT`, and a vertex
        // array of 20-byte vertices is not one. Round the *capacity* up rather than
        // padding the data, and write the padded length from a scratch slice only when
        // the tail is short — which for egui's u32 indices and 20-byte vertices is
        // every odd vertex count.
        let needed = bytes.len() as wgpu::BufferAddress;
        let padded = needed.next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT);
        if padded > self.capacity {
            self.capacity = padded.next_power_of_two();
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: self.capacity,
                usage: self.usage,
                mapped_at_creation: false,
            });
        }
        if padded == needed {
            queue.write_buffer(&self.buffer, 0, bytes);
        } else {
            let mut padded_bytes = bytes.to_vec();
            padded_bytes.resize(padded as usize, 0);
            queue.write_buffer(&self.buffer, 0, &padded_bytes);
        }
    }
}

/// The vertex attributes of `epaint::Vertex`: 8 bytes of position, 8 of uv, 4 of
/// straight sRGB colour. Asserted against the real type in this module's tests,
/// because a silent layout change would draw garbage rather than fail to compile.
const ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
    0 => Float32x2, // position, in points
    1 => Float32x2, // uv
    2 => Unorm8x4,  // sRGB, premultiplied
];

/// Room for a screenful of chrome before the first resize: about 3,200 vertices.
const INITIAL_VERTEX_BYTES: wgpu::BufferAddress = 64 * 1024;

/// egui's chrome, on the GPU.
pub struct ChromePass {
    pipeline: wgpu::RenderPipeline,
    locals: wgpu::Buffer,
    locals_group: wgpu::BindGroup,
    texture_layout: wgpu::BindGroupLayout,
    textures: HashMap<egui::TextureId, Texture>,
    vertices: Buffer,
    indices: Buffer,
    draws: Vec<Draw>,
    /// The next id handed out by [`ChromePass::register_texture`]. egui owns the
    /// `Managed` half of the namespace and never touches `User`.
    next_user_texture: u64,
}

impl ChromePass {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vellum-chrome"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/chrome.wgsl").into()),
        });

        let locals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vellum-chrome-locals-layout"),
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
            label: Some("vellum-chrome-locals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let locals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vellum-chrome-locals"),
            layout: &locals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: locals.as_entire_binding() }],
        });

        let texture_layout = texture_bind_group_layout(device, "vellum-chrome-texture-layout");

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vellum-chrome"),
            bind_group_layouts: &[Some(&locals_layout), Some(&texture_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vellum-chrome"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_chrome"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<egui::epaint::Vertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &ATTRIBUTES,
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_chrome"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // egui's colours are premultiplied, like every other pipeline in
                    // this project.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // egui does not promise a winding.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            locals,
            locals_group,
            texture_layout,
            textures: HashMap::new(),
            vertices: Buffer::new(
                device,
                "vellum-chrome-vertices",
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                INITIAL_VERTEX_BYTES,
            ),
            indices: Buffer::new(
                device,
                "vellum-chrome-indices",
                wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                INITIAL_VERTEX_BYTES,
            ),
            draws: Vec::new(),
            next_user_texture: 0,
        }
    }

    /// How many draw calls the last [`Self::prepare`] recorded. One per clip
    /// rectangle, which is one per panel, popover and scroll area.
    pub fn draw_calls(&self) -> usize {
        self.draws.len()
    }

    /// Resident textures: egui's font atlas, plus every board thumbnail.
    pub fn textures(&self) -> usize {
        self.textures.len()
    }

    /// Uploads a frame's triangles and texture changes, and records the draw plan.
    ///
    /// `size` is the framebuffer in **physical pixels** and `pixels_per_point` is
    /// `winit`'s scale factor. Must be called outside a render pass, like every
    /// other upload.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        primitives: &[ClippedPrimitive],
        delta: &egui::TexturesDelta,
        size: (u32, u32),
        pixels_per_point: f32,
    ) {
        for (id, image) in &delta.set {
            self.set_texture(device, queue, *id, image);
        }
        // Safe to do before painting rather than after: a texture in `free` is one
        // nothing in *this* frame's primitives references — egui frees a texture only
        // once it has stopped drawing with it — and wgpu keeps a resource alive until
        // the command buffers using it have completed regardless.
        for id in &delta.free {
            self.textures.remove(id);
        }

        let points = [
            size.0 as f32 / pixels_per_point,
            size.1 as f32 / pixels_per_point,
        ];
        queue.write_buffer(
            &self.locals,
            0,
            bytemuck::cast_slice(&[points[0], points[1], 0.0, 0.0]),
        );

        self.draws.clear();
        let mut vertices: Vec<egui::epaint::Vertex> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();

        for primitive in primitives {
            let Primitive::Mesh(mesh) = &primitive.primitive else {
                // `Primitive::Callback` is egui's escape hatch for a caller that wants
                // to render into a widget's rectangle with its own pipeline. Nothing in
                // `vellum-ui` emits one — the board is drawn *under* the chrome, not
                // inside it — so there is nothing to dispatch to and dropping it is the
                // whole handling rather than a stub.
                continue;
            };
            if mesh.indices.is_empty() {
                continue;
            }
            let Some(scissor) = scissor(primitive.clip_rect, pixels_per_point, size) else {
                // Entirely off-screen, or degenerate. A zero-sized scissor is a
                // validation error in wgpu, not a no-op draw.
                continue;
            };

            let base_vertex = vertices.len() as i32;
            let start = indices.len() as u32;
            indices.extend_from_slice(&mesh.indices);
            vertices.extend_from_slice(&mesh.vertices);

            let end = indices.len() as u32;
            match self.draws.last_mut() {
                // Consecutive meshes that share a texture and a clip rectangle are one
                // draw. egui emits a fresh primitive per widget group, so a dense panel
                // is otherwise dozens of draws that differ in nothing.
                Some(last)
                    if last.texture == mesh.texture_id
                        && last.scissor == scissor
                        && last.base_vertex == base_vertex =>
                {
                    last.indices.end = end;
                }
                _ => self.draws.push(Draw {
                    texture: mesh.texture_id,
                    indices: start..end,
                    base_vertex,
                    scissor,
                }),
            }
        }

        self.vertices
            .write(device, queue, bytemuck::cast_slice(&vertices));
        self.indices
            .write(device, queue, bytemuck::cast_slice(&indices));
    }

    /// Issues the recorded draws into a pass the caller opened.
    ///
    /// The pass belongs to the caller because the chrome is the *last* thing in a
    /// frame that already holds the board and the glass — see `crate::surface`.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.draws.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.locals_group, &[]);
        pass.set_vertex_buffer(0, self.vertices.buffer.slice(..));
        pass.set_index_buffer(self.indices.buffer.slice(..), wgpu::IndexFormat::Uint32);

        let mut bound: Option<egui::TextureId> = None;
        for draw in &self.draws {
            // A texture can be missing if egui referenced one it freed in the same
            // delta, or if a thumbnail was dropped between prepare and draw. Losing
            // one widget for one frame beats a validation error that kills the device.
            let Some(texture) = self.textures.get(&draw.texture) else { continue };
            if bound != Some(draw.texture) {
                pass.set_bind_group(1, &texture.bind_group, &[]);
                bound = Some(draw.texture);
            }
            pass.set_scissor_rect(draw.scissor[0], draw.scissor[1], draw.scissor[2], draw.scissor[3]);
            pass.draw_indexed(draw.indices.clone(), draw.base_vertex, 0..1);
        }
    }

    /// Hands a texture the app owns to egui, for a board thumbnail.
    ///
    /// `rgba` is straight (non-premultiplied) 8-bit RGBA, which is what
    /// `image::RgbaImage` produces and what the blob store holds. egui multiplies the
    /// widget's tint into it, so premultiplying here would darken every thumbnail by
    /// its own alpha; board thumbnails are opaque, which makes the distinction
    /// invisible until the day one is not.
    pub fn register_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Option<egui::TextureId> {
        if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
            return None;
        }
        let id = egui::TextureId::User(self.next_user_texture);
        self.next_user_texture += 1;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vellum-chrome-user-texture"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_texture(queue, &texture, rgba, [0, 0], width, height);

        let options = egui::TextureOptions::LINEAR;
        let bind_group = self.bind(device, &texture, options, "vellum-chrome-user-texture");
        self.textures
            .insert(id, Texture { texture, bind_group, options });
        Some(id)
    }

    /// Drops a texture registered with [`Self::register_texture`]. Ignored for a
    /// texture egui manages; freeing one of those is egui's call, not ours.
    pub fn free_texture(&mut self, id: egui::TextureId) {
        if matches!(id, egui::TextureId::User(_)) {
            self.textures.remove(&id);
        }
    }

    fn set_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: egui::TextureId,
        delta: &egui::epaint::ImageDelta,
    ) {
        let egui::epaint::ImageData::Color(image) = &delta.image;
        let width = image.size[0] as u32;
        let height = image.size[1] as u32;
        if width == 0 || height == 0 {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(image.pixels.as_slice());

        match (delta.pos, self.textures.get(&id)) {
            // A partial update of a texture we already hold: egui grows its font
            // atlas by patching, and re-uploading the whole 2048² page for one new
            // glyph is the difference between a frame and a stutter.
            (Some(pos), Some(existing)) => {
                let texture = existing.texture.clone();
                let stale_sampler = existing.options != delta.options;
                write_texture(
                    queue,
                    &texture,
                    bytes,
                    [pos[0] as u32, pos[1] as u32],
                    width,
                    height,
                );
                // A patch may also change the filtering — egui does it when a page is
                // reused for a differently sampled image — and the sampler lives in
                // the bind group, so the patch alone would leave the old one in force.
                if stale_sampler {
                    let bind_group =
                        self.bind(device, &texture, delta.options, "vellum-chrome-egui-texture");
                    self.textures.insert(
                        id,
                        Texture { texture, bind_group, options: delta.options },
                    );
                }
            }
            _ => {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("vellum-chrome-egui-texture"),
                    size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    // Not `Rgba8UnormSrgb`: the surface is linear-`Unorm` and the
                    // shader multiplies in gamma space. See the shader header.
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                write_texture(queue, &texture, bytes, [0, 0], width, height);
                let bind_group =
                    self.bind(device, &texture, delta.options, "vellum-chrome-egui-texture");
                self.textures.insert(
                    id,
                    Texture { texture, bind_group, options: delta.options },
                );
            }
        }
    }

    fn bind(
        &self,
        device: &wgpu::Device,
        texture: &wgpu::Texture,
        options: egui::TextureOptions,
        label: &str,
    ) -> wgpu::BindGroup {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&sampler_descriptor(options));
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        })
    }
}

impl std::fmt::Debug for ChromePass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromePass")
            .field("draws", &self.draws.len())
            .field("textures", &self.textures.len())
            .finish()
    }
}

/// The texture-and-sampler bind group layout the chrome and the compositor share.
pub(crate) fn texture_bind_group_layout(
    device: &wgpu::Device,
    label: &str,
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn sampler_descriptor(options: egui::TextureOptions) -> wgpu::SamplerDescriptor<'static> {
    let address = match options.wrap_mode {
        egui::TextureWrapMode::ClampToEdge => wgpu::AddressMode::ClampToEdge,
        egui::TextureWrapMode::Repeat => wgpu::AddressMode::Repeat,
        egui::TextureWrapMode::MirroredRepeat => wgpu::AddressMode::MirrorRepeat,
    };
    let filter = |f: egui::TextureFilter| match f {
        egui::TextureFilter::Nearest => wgpu::FilterMode::Nearest,
        egui::TextureFilter::Linear => wgpu::FilterMode::Linear,
    };
    wgpu::SamplerDescriptor {
        label: Some("vellum-chrome-sampler"),
        address_mode_u: address,
        address_mode_v: address,
        address_mode_w: address,
        mag_filter: filter(options.magnification),
        min_filter: filter(options.minification),
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    }
}

fn write_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    bytes: &[u8],
    origin: [u32; 2],
    width: u32,
    height: u32,
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x: origin[0], y: origin[1], z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * width),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
}

/// egui's clip rectangle, in points, as a wgpu scissor rectangle in physical pixels.
///
/// Returns `None` for anything that would be empty on screen. That is not a
/// nicety: `set_scissor_rect` with a zero width or a rectangle past the framebuffer
/// is a validation error, and egui does emit off-screen clip rectangles for widgets
/// scrolled out of view.
fn scissor(clip: egui::Rect, pixels_per_point: f32, size: (u32, u32)) -> Option<[u32; 4]> {
    let scale = |v: f32| (v * pixels_per_point).round();
    let min_x = scale(clip.min.x).max(0.0) as u32;
    let min_y = scale(clip.min.y).max(0.0) as u32;
    let max_x = (scale(clip.max.x).max(0.0) as u32).min(size.0);
    let max_y = (scale(clip.max.y).max(0.0) as u32).min(size.1);
    if min_x >= max_x || min_y >= max_y {
        return None;
    }
    Some([min_x, min_y, max_x - min_x, max_y - min_y])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vertex layout is written twice — once as attribute offsets here, once as
    /// the field order of `epaint::Vertex` — and only one of them is checked by the
    /// compiler. A mismatch draws garbage rather than failing to build.
    #[test]
    fn the_vertex_layout_matches_epaints_vertex() {
        assert_eq!(size_of::<egui::epaint::Vertex>(), 20);
        let offsets: Vec<_> = ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 16]);
        assert_eq!(ATTRIBUTES[2].format, wgpu::VertexFormat::Unorm8x4);
    }

    #[test]
    fn a_clip_rectangle_scales_from_points_to_pixels() {
        let clip = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(110.0, 70.0));
        assert_eq!(scissor(clip, 1.0, (1440, 900)), Some([10, 20, 100, 50]));
        assert_eq!(scissor(clip, 2.0, (2880, 1800)), Some([20, 40, 200, 100]));
    }

    /// A rectangle that hangs off the framebuffer is clamped, not passed through:
    /// `set_scissor_rect` past the attachment is a validation error.
    #[test]
    fn a_clip_rectangle_is_clamped_to_the_framebuffer() {
        let clip = egui::Rect::from_min_max(egui::pos2(-50.0, -50.0), egui::pos2(2000.0, 2000.0));
        assert_eq!(scissor(clip, 1.0, (1440, 900)), Some([0, 0, 1440, 900]));
    }

    /// egui emits clip rectangles for widgets scrolled out of view, and an empty
    /// scissor is a validation error rather than a draw of nothing.
    #[test]
    fn an_empty_or_offscreen_clip_rectangle_is_dropped() {
        let empty = egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(10.0, 40.0));
        assert_eq!(scissor(empty, 1.0, (1440, 900)), None);

        let offscreen =
            egui::Rect::from_min_max(egui::pos2(2000.0, 2000.0), egui::pos2(2100.0, 2100.0));
        assert_eq!(scissor(offscreen, 1.0, (1440, 900)), None);

        let inverted = egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(50.0, 50.0));
        assert_eq!(scissor(inverted, 1.0, (1440, 900)), None);

        assert_eq!(scissor(egui::Rect::NOTHING, 1.0, (1440, 900)), None);
    }

    /// egui's `TextureOptions` are the widget author's intent; they have to survive
    /// the trip to a sampler or a nearest-filtered swatch comes out blurred.
    #[test]
    fn texture_options_map_onto_sampler_settings() {
        let nearest = sampler_descriptor(egui::TextureOptions::NEAREST);
        assert_eq!(nearest.mag_filter, wgpu::FilterMode::Nearest);
        assert_eq!(nearest.address_mode_u, wgpu::AddressMode::ClampToEdge);

        let linear = sampler_descriptor(egui::TextureOptions::LINEAR);
        assert_eq!(linear.mag_filter, wgpu::FilterMode::Linear);

        let repeat = sampler_descriptor(egui::TextureOptions {
            wrap_mode: egui::TextureWrapMode::Repeat,
            ..egui::TextureOptions::LINEAR
        });
        assert_eq!(repeat.address_mode_u, wgpu::AddressMode::Repeat);
    }
}
