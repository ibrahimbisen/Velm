//! The renderer: five pipelines, the resources they bind, and the two calls that
//! turn a [`DrawList`] into pixels.
//!
//! No windowing. [`Renderer::new`] takes a device and a target format, and
//! [`Renderer::draw`] records into a render pass the caller already opened. That is
//! what lets the same code drive a swapchain, an offscreen texture for a PNG export
//! or a board thumbnail, and the pixel tests — and it is what keeps the eventual
//! `wasm32` + WebGPU viewer of `docs/01-architecture.md` §1 a recompile rather than a
//! port.

use crate::atlas::{AtlasConfig, GlyphAtlas};
use crate::buffer::{GrowableBuffer, Growth};
use crate::list::{Batch, BatchKind, DrawList};
use crate::mesh::{self, MeshBatch, MeshTransform};
use crate::quad;
use crate::shape;
use crate::texture::{TextureBudget, TextureManager};
use crate::view::Views;
use crate::{image, text};

/// An instance array plus its pipeline.
struct InstancePass {
    pipeline: wgpu::RenderPipeline,
    instances: GrowableBuffer,
}

impl InstancePass {
    fn write<T: bytemuck::Pod>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[T],
    ) {
        self.instances.write(device, queue, bytemuck::cast_slice(instances));
    }
}

/// A read-only storage buffer and the bind group over it, rebuilt when it grows.
struct StorageBinding {
    buffer: GrowableBuffer,
    layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    label: &'static str,
}

impl StorageBinding {
    fn new(device: &wgpu::Device, layout: wgpu::BindGroupLayout, label: &'static str) -> Self {
        let buffer = GrowableBuffer::new(
            device,
            label,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            INITIAL_STORAGE_BYTES,
        );
        let bind_group = bind_storage(device, &layout, buffer.buffer(), label);
        Self { buffer, layout, bind_group, label }
    }

    fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) {
        if self.buffer.write(device, queue, bytes) == Growth::Reallocated {
            self.bind_group = bind_storage(device, &self.layout, self.buffer.buffer(), self.label);
        }
    }
}

/// One 4 KB page: enough for 500 polygon vertices or 128 mesh transforms, which is
/// more than a screenful of either. A storage buffer may not be zero-sized, so this
/// is also the floor.
const INITIAL_STORAGE_BYTES: wgpu::BufferAddress = 4096;

pub struct Renderer {
    views: Views,
    quads: InstancePass,
    shapes: InstancePass,
    polygons: StorageBinding,
    images: InstancePass,
    glyphs: InstancePass,
    meshes: InstancePass,
    mesh_indices: GrowableBuffer,
    mesh_transforms: StorageBinding,
    textures: TextureManager,
    atlas: GlyphAtlas,
    /// The batch plan recorded by the last [`Renderer::prepare`].
    plan: Vec<Batch>,
}

impl Renderer {
    /// Brings up every pipeline for a target of `format`.
    ///
    /// `format` should be a non-sRGB `Unorm` format, which is what `vellum-app`
    /// selects: Miro stores colours as sRGB hex and blends in sRGB as browsers do, so
    /// letting the hardware linearise would make every imported board subtly lighter
    /// than the original.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        Self::with_limits(device, format, TextureBudget::default(), AtlasConfig::default())
    }

    pub fn with_limits(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        budget: TextureBudget,
        atlas: AtlasConfig,
    ) -> Self {
        let views = Views::new(device);
        let textures = TextureManager::new(device, budget);
        let atlas = GlyphAtlas::new(device, atlas);

        let polygons = StorageBinding::new(
            device,
            shape::polygon_bind_group_layout(device),
            "vellum-polygon-vertices",
        );
        let mesh_transforms = StorageBinding::new(
            device,
            mesh::transform_bind_group_layout(device),
            "vellum-mesh-transforms",
        );

        Self {
            quads: InstancePass {
                pipeline: quad::pipeline(device, format, views.layout()),
                instances: instance_buffer(device, "vellum-quad-instances"),
            },
            shapes: InstancePass {
                pipeline: shape::pipeline(device, format, views.layout(), &polygons.layout),
                instances: instance_buffer(device, "vellum-shape-instances"),
            },
            images: InstancePass {
                pipeline: image::pipeline(
                    device,
                    format,
                    views.layout(),
                    textures.bind_group_layout(),
                ),
                instances: instance_buffer(device, "vellum-image-instances"),
            },
            glyphs: InstancePass {
                pipeline: text::pipeline(device, format, views.layout(), atlas.bind_group_layout()),
                instances: instance_buffer(device, "vellum-glyph-instances"),
            },
            meshes: InstancePass {
                pipeline: mesh::pipeline(device, format, views.layout(), &mesh_transforms.layout),
                instances: GrowableBuffer::new(
                    device,
                    "vellum-mesh-vertices",
                    wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    INITIAL_INSTANCE_BYTES,
                ),
            },
            mesh_indices: GrowableBuffer::new(
                device,
                "vellum-mesh-indices",
                wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                INITIAL_INSTANCE_BYTES,
            ),
            polygons,
            mesh_transforms,
            views,
            textures,
            atlas,
            plan: Vec::new(),
        }
    }

    /// Image residency. Upload here, and mark and evict once per frame.
    pub fn textures(&self) -> &TextureManager {
        &self.textures
    }

    pub fn textures_mut(&mut self) -> &mut TextureManager {
        &mut self.textures
    }

    /// The glyph atlas. Call [`GlyphAtlas::prepare`] with the frame's glyphs before
    /// building the draw list that references them.
    pub fn atlas(&self) -> &GlyphAtlas {
        &self.atlas
    }

    pub fn atlas_mut(&mut self) -> &mut GlyphAtlas {
        &mut self.atlas
    }

    /// Advances the residency clocks of both caches. Call once per frame, before
    /// marking anything.
    ///
    /// It used to return the textures it had just **destroyed** to ask the caller for a finer
    /// upload. Nothing is destroyed to ask a question now — see
    /// [`TextureManager::begin_frame`] for why that was the flicker, and
    /// [`Self::wants_refinement`] for what replaced it. The return type changed to `()` on
    /// purpose: it makes the compiler name every caller that was reading the old meaning.
    pub fn begin_frame(&mut self) {
        self.textures.begin_frame();
        self.atlas.begin_frame();
    }

    /// The textures the last frame found too coarse, and the size each should come back at.
    ///
    /// Each is still resident and still drawing. See [`TextureManager::wants_refinement`].
    #[must_use]
    pub fn wants_refinement(&self) -> &[crate::texture::Refinement] {
        self.textures.wants_refinement()
    }

    /// Uploads everything `list` needs and records the batch plan.
    ///
    /// Separate from [`Self::draw`] because uploads have to happen outside a render
    /// pass, and because the caller may want to prepare more than one target — a
    /// thumbnail and the window — from one list.
    ///
    /// Also where image resolution is chosen. The list is the only place that knows
    /// how large each image is *drawn* — the instance's size in its view's units,
    /// divided by that view's units per pixel, divided by the UV span it shows — and
    /// that number, not what is on screen versus off it, is what decides how much of
    /// a texture is worth keeping. Doing it here rather than asking the caller to
    /// report sizes means every caller gets it, including the ones that only ever
    /// build a list and present it.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, list: &DrawList) {
        self.observe_detail(list);
        self.textures.resolve_detail(device, queue);

        self.views.write(device, queue, list.views());
        self.quads.write(device, queue, list.quad_instances());
        self.shapes.write(device, queue, list.shape_instances());
        self.images.write(device, queue, list.image_instances());
        self.glyphs.write(device, queue, list.glyph_instances());
        self.write_meshes(device, queue, list.meshes());

        // A storage buffer may not be empty, so an arena with nothing in it still
        // uploads one vertex. It is never read: no instance addresses it.
        let polygons = list.polygons().vertices();
        self.polygons.write(
            device,
            queue,
            if polygons.is_empty() {
                bytemuck::cast_slice(&[[0.0f32, 0.0]])
            } else {
                bytemuck::cast_slice(polygons)
            },
        );

        self.plan.clear();
        self.plan.extend_from_slice(list.batches());
    }

    /// Re-uploads only the mesh transform table.
    ///
    /// The fast path for a pan: the camera moved, so every camera-relative
    /// translation changed, but not one triangle did. A caller that keeps a
    /// [`MeshBatch`] across frames rewrites its transforms and calls this instead of
    /// re-preparing megabytes of ink.
    pub fn update_mesh_transforms(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        transforms: &[MeshTransform],
    ) {
        self.mesh_transforms.write(device, queue, storage_bytes(transforms));
    }

    /// Reports every image draw's size to the texture manager.
    ///
    /// Walks batches rather than the instance array because a batch is what carries
    /// the texture id and the view — an instance on its own says how big it is but
    /// not in whose units, and the two views a frame uses differ by the zoom, which
    /// is the entire signal.
    fn observe_detail(&mut self, list: &DrawList) {
        let views = list.views();
        let instances = list.image_instances();
        for batch in list.batches() {
            let BatchKind::Images(id) = batch.kind else { continue };
            let Some(view) = views.get(batch.view as usize) else { continue };
            let range = batch.start as usize..(batch.end as usize).min(instances.len());
            for instance in &instances[range] {
                let texels =
                    crate::detail::demanded_texels(instance.size, view.units_per_pixel, instance.crop());
                self.textures.note_drawn(id, texels);
            }
        }
    }

    fn write_meshes(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, meshes: &MeshBatch) {
        self.meshes.write(device, queue, meshes.vertices());
        self.mesh_indices
            .write(device, queue, bytemuck::cast_slice(meshes.indices()));
        self.mesh_transforms
            .write(device, queue, storage_bytes(meshes.transforms()));
    }

    /// Issues the recorded draws into `pass`.
    ///
    /// The pass belongs to the caller, so a frame can put chrome, a scissored panel
    /// or a debug overlay in the same one. State is set only when it changes, which
    /// is what makes a run of same-kind batches a single pipeline bind.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let mut bound_pipeline: Option<BatchKind> = None;
        let mut bound_view: Option<u32> = None;
        let mut bound_group: Option<BatchKind> = None;

        for batch in &self.plan {
            // The bind group for a texture or an atlas page can be missing: a texture
            // evicted between building the list and drawing it, or a page dropped by
            // a repack. Skipping the batch loses one image or one run of glyphs for
            // one frame, which is what the caller's next `prepare` will fix.
            let group = match batch.kind {
                BatchKind::Images(id) => match self.textures.bind_group(id) {
                    Some(group) => Some(group),
                    None => continue,
                },
                BatchKind::Glyphs(page) => match self.atlas.bind_group(page) {
                    Some(group) => Some(group),
                    None => continue,
                },
                BatchKind::Shapes => Some(&self.polygons.bind_group),
                BatchKind::Meshes => Some(&self.mesh_transforms.bind_group),
                BatchKind::Quads => None,
            };

            let pipeline_kind = pipeline_identity(batch.kind);
            if bound_pipeline != Some(pipeline_kind) {
                pass.set_pipeline(self.pipeline_for(batch.kind));
                bound_pipeline = Some(pipeline_kind);
                // A pipeline change invalidates nothing in wgpu, but the vertex
                // buffer and bind groups belong to the pipeline's layout, so rebind.
                bound_view = None;
                bound_group = None;
                match batch.kind {
                    BatchKind::Meshes => {
                        pass.set_vertex_buffer(0, self.meshes.instances.buffer().slice(..));
                        pass.set_index_buffer(
                            self.mesh_indices.buffer().slice(..),
                            wgpu::IndexFormat::Uint32,
                        );
                    }
                    _ => {
                        pass.set_vertex_buffer(0, self.instances_for(batch.kind).slice(..));
                    }
                }
            }

            if bound_view != Some(batch.view) {
                pass.set_bind_group(0, self.views.bind_group(), &[self.views.offset(batch.view)]);
                bound_view = Some(batch.view);
            }

            if let Some(group) = group
                && bound_group != Some(batch.kind)
            {
                pass.set_bind_group(1, group, &[]);
                bound_group = Some(batch.kind);
            }

            match batch.kind {
                BatchKind::Meshes => pass.draw_indexed(batch.start..batch.end, 0, 0..1),
                _ => pass.draw(0..4, batch.start..batch.end),
            }
        }
    }

    /// How many draw calls the last [`Self::prepare`] recorded.
    pub fn draw_calls(&self) -> usize {
        self.plan.len()
    }

    fn pipeline_for(&self, kind: BatchKind) -> &wgpu::RenderPipeline {
        match kind {
            BatchKind::Quads => &self.quads.pipeline,
            BatchKind::Shapes => &self.shapes.pipeline,
            BatchKind::Images(_) => &self.images.pipeline,
            BatchKind::Glyphs(_) => &self.glyphs.pipeline,
            BatchKind::Meshes => &self.meshes.pipeline,
        }
    }

    fn instances_for(&self, kind: BatchKind) -> &wgpu::Buffer {
        match kind {
            BatchKind::Quads => self.quads.instances.buffer(),
            BatchKind::Shapes => self.shapes.instances.buffer(),
            BatchKind::Images(_) => self.images.instances.buffer(),
            BatchKind::Glyphs(_) => self.glyphs.instances.buffer(),
            BatchKind::Meshes => self.meshes.instances.buffer(),
        }
    }
}

/// Batches differing only in which texture or atlas page they bind share a pipeline,
/// so comparing on this rather than on [`BatchKind`] avoids a redundant pipeline
/// bind between two runs of images.
fn pipeline_identity(kind: BatchKind) -> BatchKind {
    match kind {
        BatchKind::Images(_) => BatchKind::Images(crate::texture::TextureId(u64::MAX)),
        BatchKind::Glyphs(_) => BatchKind::Glyphs(crate::atlas::AtlasPage {
            kind: crate::atlas::AtlasKind::Coverage,
            index: u32::MAX,
        }),
        other => other,
    }
}

/// A storage buffer may not be zero-sized, and a bind group over an empty one is a
/// validation error, so an empty table uploads one identity entry that nothing reads.
fn storage_bytes(transforms: &[MeshTransform]) -> &[u8] {
    const IDENTITY: [MeshTransform; 1] = [MeshTransform::IDENTITY];
    if transforms.is_empty() {
        bytemuck::cast_slice(&IDENTITY)
    } else {
        bytemuck::cast_slice(transforms)
    }
}

/// Room for a comfortable screenful before the first resize.
const INITIAL_INSTANCE_BYTES: wgpu::BufferAddress = 256 * 1024;

fn instance_buffer(device: &wgpu::Device, label: &'static str) -> GrowableBuffer {
    GrowableBuffer::new(
        device,
        label,
        wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        INITIAL_INSTANCE_BYTES,
    )
}

fn bind_storage(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
    label: &str,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::{AtlasKind, AtlasPage};
    use crate::texture::TextureId;

    /// Two image batches must not re-bind the pipeline between them — only the
    /// texture changed, and a redundant pipeline bind is exactly the state churn the
    /// batching exists to remove.
    #[test]
    fn batches_that_differ_only_in_binding_share_a_pipeline() {
        let a = BatchKind::Images(TextureId(1));
        let b = BatchKind::Images(TextureId(2));
        assert_ne!(a, b);
        assert_eq!(pipeline_identity(a), pipeline_identity(b));

        let page = |index| BatchKind::Glyphs(AtlasPage { kind: AtlasKind::Coverage, index });
        assert_eq!(pipeline_identity(page(0)), pipeline_identity(page(3)));
        let color = BatchKind::Glyphs(AtlasPage { kind: AtlasKind::Color, index: 0 });
        assert_eq!(pipeline_identity(page(0)), pipeline_identity(color));
    }

    #[test]
    fn distinct_pipelines_stay_distinct() {
        let kinds = [
            BatchKind::Quads,
            BatchKind::Shapes,
            BatchKind::Images(TextureId(0)),
            BatchKind::Glyphs(AtlasPage { kind: AtlasKind::Coverage, index: 0 }),
            BatchKind::Meshes,
        ];
        for (i, a) in kinds.iter().enumerate() {
            for b in &kinds[i + 1..] {
                assert_ne!(pipeline_identity(*a), pipeline_identity(*b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn an_empty_transform_table_still_uploads_one_entry() {
        assert_eq!(storage_bytes(&[]).len(), size_of::<MeshTransform>());
        assert_eq!(
            storage_bytes(&[MeshTransform::IDENTITY, MeshTransform::at([1.0, 2.0])]).len(),
            2 * size_of::<MeshTransform>()
        );
    }
}
