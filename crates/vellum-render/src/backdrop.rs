//! The blurred backdrop behind glass chrome: what gets blurred, at what resolution,
//! and — the part that actually matters — when nothing gets blurred at all.
//!
//! `docs/05-design-language.md` §3a budgets the whole material at **under 0.5 ms per
//! frame** and says why: the project exists because Miro stutters, so a blur that
//! costs frame time is not worth having. Three decisions follow from that number, and
//! none of them is an optimisation to add later.
//!
//! **Only the regions behind panels.** A full-frame blur at 1440 × 900 is 1.3 M
//! fragments per pass however little of it is covered. The toolbar, the context bar
//! and the zoom cluster together cover perhaps 6% of that. So the unit of work here
//! is a [`Region`] — one rectangle of canvas behind one panel — and they are packed
//! side by side into a single atlas, which is what keeps the pass count independent
//! of the panel count.
//!
//! **Quarter resolution.** The regions are captured at 1/4 per axis, so a pass over
//! them touches 1/16 the fragments. A blur is a low-pass filter; running it at full
//! resolution computes detail it is about to throw away. The capture itself is an
//! exact 4 × 4 box in four bilinear taps — see `shaders/backdrop.wgsl` for why a
//! single tap would alias.
//!
//! **A cache keyed on (region, canvas revision).** A panel sitting still over a board
//! that is not moving needs no work whatsoever: not a cheap pass, not a small pass —
//! none. [`BackdropCache::refresh`] encodes nothing in that case, and
//! [`Refresh::is_free`] says so. Only regions whose rectangle moved or whose canvas
//! changed are re-blurred, and the clean ones keep their pixels because every pass
//! loads rather than clears.
//!
//! # Why Kawase and not a separable Gaussian
//!
//! A separable Gaussian wide enough to matter is two passes of 9–13 taps. Two Kawase
//! passes are four taps each — eight taps total against twenty-plus — and because
//! every tap is a half-integer offset it is an equal blend of two texels, so the
//! effective kernel is far wider than the tap count suggests. See [`KAWASE_OFFSETS`]
//! for the arithmetic. At quarter resolution behind a 4 × 4 box filter, the
//! difference between the two in the final image is not visible; the difference in
//! cost is a factor of three.

use crate::buffer::{GrowableBuffer, Growth};
use crate::texture;

/// The already-rendered canvas, and how to tell whether it has changed.
///
/// Re-exported by [`crate::glass`], which is where a caller meets it.
#[derive(Debug, Clone, Copy)]
pub struct Backdrop<'a> {
    /// A view of the texture the board was drawn into. Needs `TEXTURE_BINDING`
    /// usage, and must not be the texture the composite is about to draw into.
    pub texture: &'a wgpu::TextureView,
    /// Physical pixels, matching that texture.
    pub width: u32,
    pub height: u32,
    /// Bumped by the caller whenever the canvas's pixels change. Equal revisions
    /// mean equal pixels, and that is the entire invalidation rule: a panel over an
    /// unchanged board re-blurs nothing.
    ///
    /// Wrong in the safe direction — bumping too often — costs the blur; wrong the
    /// other way shows a stale backdrop, so it wants to come from the same state
    /// that decides whether to redraw the canvas at all.
    pub revision: u64,
}

/// A rectangle of the canvas to blur, in physical canvas pixels.
///
/// `x` and `y` are signed and the rectangle is **not** clipped to the canvas: a panel
/// near the edge of the window has a region that hangs off it, and the capture shader
/// edge-extends instead. Clipping here would make the region's size depend on the
/// panel's position, which would repack the atlas on every frame of a drag; leaving
/// it unclipped means a moving panel keeps its slot and only its contents change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Region {
    pub x: i32,
    pub y: i32,
    /// Always a multiple of the downsample factor, so the atlas is an exact integer
    /// reduction of the canvas grid rather than a resample with a drifting phase.
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// The region that must be captured for a panel occupying `origin`..`origin+size`
    /// with `factor` × reduction and a blur reaching `margin` canvas pixels.
    ///
    /// The origin snaps *down* to a multiple of `factor` and the far edge snaps *up*,
    /// so the captured grid lines up with the canvas's own and a panel that moves by
    /// one pixel does not resample on a different phase.
    pub fn around(origin: [f32; 2], size: [f32; 2], margin: u32, factor: u32) -> Self {
        let factor = factor.max(1) as i32;
        let margin = margin as f32;
        // `div_euclid` throughout rather than `/`: a panel dragged off the left of the
        // window has a negative coordinate, and truncating division rounds it towards
        // zero — the wrong way — leaving the left edge of the blur short by up to
        // three pixels and the material visibly clipped there.
        let snap_down = |v: f32| ((v - margin).floor() as i32).div_euclid(factor) * factor;
        let snap_up =
            |v: f32| ((v + margin).ceil() as i32 + factor - 1).div_euclid(factor) * factor;

        let x0 = snap_down(origin[0]);
        let y0 = snap_down(origin[1]);
        let x1 = snap_up(origin[0] + size[0].abs());
        let y1 = snap_up(origin[1] + size[1].abs());

        Self {
            x: x0,
            y: y0,
            width: (x1 - x0).max(factor) as u32,
            height: (y1 - y0).max(factor) as u32,
        }
    }

    /// The region's size in atlas texels.
    pub fn atlas_size(&self, factor: u32) -> (u32, u32) {
        let factor = factor.max(1);
        (self.width.div_ceil(factor), self.height.div_ceil(factor))
    }

    /// The region in `canvas`-normalised coordinates, which is what the capture pass
    /// samples. May fall outside `0..1`; the shader clamps its taps to the texture.
    fn source_uv(&self, canvas: (u32, u32)) -> [f32; 4] {
        let w = canvas.0.max(1) as f32;
        let h = canvas.1.max(1) as f32;
        [
            self.x as f32 / w,
            self.y as f32 / h,
            (self.x + self.width as i32) as f32 / w,
            (self.y + self.height as i32) as f32 / h,
        ]
    }
}

/// Where one region's quarter-resolution copy lives, and what it was built from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    region: Region,
    /// `x, y, width, height` in atlas texels.
    atlas: [u32; 4],
    /// The canvas revision the copy was captured from. A mismatch is the whole of
    /// the invalidation rule.
    revision: u64,
}

/// Kawase tap offsets, in destination texels, one per pass.
///
/// Each pass takes four bilinear taps on the diagonals at ±`offset`. With `offset =
/// k + 0.5` every tap is an equal blend of texels `k` and `k+1`, so the 1D kernel is
/// four equal weights at ±k and ±(k+1) and its variance is `(k² + (k+1)²) / 2`.
/// Variance adds across passes, so `[1.5, 2.5]` gives `2.5 + 6.5 = 9`, i.e. σ = 3
/// atlas texels — **12 canvas pixels** at quarter resolution, which is a properly
/// frosted pane rather than a soft focus.
///
/// Two passes, as §3a specifies ("dual-pass Kawase"). A third would widen it further
/// but a two-pass sequence has no visible tap structure at this radius and a
/// three-pass one starts to.
const KAWASE_OFFSETS: [f32; 2] = [1.5, 2.5];

/// The passes ping-pong between two atlases, so an even count is what puts the
/// finished blur back in slot 0 — which is the one the composite binds, once, without
/// having to be told which way round this frame came out. Compile-time, because
/// adding a third offset is exactly the change that would break it silently.
const _: () = assert!(
    KAWASE_OFFSETS.len().is_multiple_of(2),
    "an odd pass count leaves the blur in atlas 1 while the composite reads atlas 0"
);

/// How far one texel's influence spreads, in canvas pixels, through the capture and
/// both blur passes together.
///
/// This is the margin a region has to carry around its panel. Get it wrong and the
/// blur near the panel's edge mixes in whatever the atlas happens to hold outside the
/// region — which the shader clamps to the region's own edge, so the failure looks
/// like a smeared frame rather than garbage, and is correspondingly easy to miss.
pub(crate) fn blur_reach(factor: u32) -> u32 {
    let factor = factor.max(1);
    // The capture's box filter reaches half a destination texel, i.e. `factor / 2`
    // canvas pixels. Each Kawase tap at `k + 0.5` reaches texel `k + 1`, which is
    // `offset + 0.5` texels.
    let taps: f32 = KAWASE_OFFSETS.iter().map(|offset| offset + 0.5).sum();
    let reach = factor as f32 / 2.0 + factor as f32 * taps;
    // Rounded up to a whole factor so the region's size stays a multiple of it.
    (reach.ceil() as u32).div_ceil(factor) * factor
}

/// What one [`BackdropCache::refresh`] actually cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Refresh {
    pub refreshed: usize,
    pub passes: usize,
    pub texels: u64,
    pub uploads: usize,
}

/// The per-pass uniform, matching `BackdropParams` in `shaders/backdrop.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BackdropParams {
    dst_size: [f32; 2],
    src_texel: [f32; 2],
    offset: f32,
    taps: f32,
    padding: [f32; 2],
}

/// One region's quad, matching `RegionInstance` in `shaders/backdrop.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct RegionInstance {
    dst: [f32; 4],
    src: [f32; 4],
}

const REGION_ATTRIBUTES: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
    0 => Float32x4,  // dst
    1 => Float32x4,  // src
];

/// One half of the ping-pong pair.
struct AtlasTexture {
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

/// A horizontal strip of the atlas, as tall as the first region placed on it.
#[derive(Debug, Clone, Copy)]
struct Shelf {
    top: u32,
    height: u32,
    next_x: u32,
}

/// Packs region sizes into a `size` × `size` atlas, tallest first.
///
/// Shelf packing, as [`crate::atlas`] uses for glyphs, and for the same reason: the
/// failure mode is wasted space rather than overlap. Sorting by height first matters
/// more here than it does for glyphs, because a toolbar's region and a zoom cluster's
/// differ by a factor of five in height, and unsorted they would each open a shelf.
///
/// Returns placements in the caller's order, or `None` if anything did not fit.
fn pack(size: u32, dimensions: &[(u32, u32)]) -> Option<Vec<[u32; 4]>> {
    let mut order: Vec<usize> = (0..dimensions.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(dimensions[i].1));

    let mut shelves: Vec<Shelf> = Vec::new();
    let mut next_y = 0;
    let mut placed = vec![[0u32; 4]; dimensions.len()];

    for index in order {
        let (width, height) = dimensions[index];
        if width > size || height > size {
            return None;
        }
        let existing = shelves
            .iter_mut()
            .find(|shelf| height <= shelf.height && shelf.next_x + width <= size);
        let (x, y) = match existing {
            Some(shelf) => {
                let x = shelf.next_x;
                shelf.next_x += width;
                (x, shelf.top)
            }
            None => {
                if next_y + height > size {
                    return None;
                }
                shelves.push(Shelf { top: next_y, height, next_x: width });
                let y = next_y;
                next_y += height;
                (0, y)
            }
        };
        placed[index] = [x, y, width, height];
    }
    Some(placed)
}

/// The smallest atlas worth allocating. 256² is a megabyte and holds the regions
/// behind a full set of chrome at 1440 × 900 with room to spare; anything smaller
/// would just guarantee an immediate grow.
const MIN_ATLAS: u32 = 256;

pub(crate) struct BackdropCache {
    params_layout: wgpu::BindGroupLayout,
    params: GrowableBuffer,
    params_bind_group: wgpu::BindGroup,
    params_stride: u32,
    source_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    capture: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    instances: GrowableBuffer,
    staged: Vec<RegionInstance>,
    /// Ping-pong pair. With an even number of blur passes the finished image is
    /// always back in slot 0, which is what lets the composite bind one fixed group.
    atlas: [AtlasTexture; 2],
    size: u32,
    max_size: u32,
    slots: Vec<Slot>,
    canvas: (u32, u32),
    factor: u32,
    dirty: Vec<usize>,
    dimensions: Vec<(u32, u32)>,
}

impl BackdropCache {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let params_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vellum-backdrop-params-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    // One block per pass, addressed by dynamic offset — the same
                    // arrangement `crate::view` uses, and for the same reason: a
                    // mid-pass uniform rewrite silently applies the last value to
                    // every pass that read it.
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(size_of::<BackdropParams>() as u64),
                },
                count: None,
            }],
        });
        let params_stride = align_to(
            size_of::<BackdropParams>() as u32,
            device.limits().min_uniform_buffer_offset_alignment,
        );
        let params = GrowableBuffer::new(
            device,
            "vellum-backdrop-params",
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            u64::from(params_stride) * (1 + KAWASE_OFFSETS.len() as u64),
        );
        let params_bind_group = bind_params(device, &params_layout, params.buffer());

        let source_layout = texture::bind_group_layout(device, "vellum-backdrop-source-layout");
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vellum-backdrop-sampler"),
            // Clamping is a second line of defence: every shader here clamps its own
            // taps, but a region flush against the atlas edge must not wrap round to
            // the other side even if that arithmetic is ever wrong.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let capture = region_pipeline(
            device,
            "vellum-backdrop-capture",
            "fs_downsample",
            &params_layout,
            &source_layout,
        );
        let blur = region_pipeline(
            device,
            "vellum-backdrop-blur",
            "fs_kawase",
            &params_layout,
            &source_layout,
        );

        let max_size = device.limits().max_texture_dimension_2d.max(MIN_ATLAS);
        // A 1 × 1 atlas from the start rather than an `Option`: the composite
        // pipeline binds group 1 unconditionally, and a bind group that is sometimes
        // absent is a validation error waiting for the first opaque frame.
        let atlas = create_atlas(device, &source_layout, &sampler, 1);

        Self {
            params_layout,
            params,
            params_bind_group,
            params_stride,
            source_layout,
            sampler,
            capture,
            blur,
            instances: GrowableBuffer::new(
                device,
                "vellum-backdrop-regions",
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                1024,
            ),
            staged: Vec::new(),
            atlas,
            size: 1,
            max_size,
            slots: Vec::new(),
            canvas: (0, 0),
            factor: 0,
            dirty: Vec::new(),
            dimensions: Vec::new(),
        }
    }

    /// The layout the composite pipeline binds the finished atlas with.
    pub(crate) fn source_layout(&self) -> &wgpu::BindGroupLayout {
        &self.source_layout
    }

    /// The finished blur, for the composite pass.
    pub(crate) fn result_bind_group(&self) -> &wgpu::BindGroup {
        &self.atlas[0].bind_group
    }

    pub(crate) fn atlas_size(&self) -> u32 {
        self.size
    }

    /// Where region `index` of the last [`Self::refresh`] landed, in atlas texels.
    pub(crate) fn slot(&self, index: usize) -> Option<[u32; 4]> {
        self.slots.get(index).map(|slot| slot.atlas)
    }

    /// Drops every slot, so the next translucent frame rebuilds from scratch.
    ///
    /// Called when the material is switched off. Keeping stale slots would be
    /// harmless but would also mean a user who toggles Reduce Transparency twice sees
    /// a backdrop captured from whatever the board looked like before.
    pub(crate) fn forget(&mut self) {
        self.slots.clear();
        self.factor = 0;
    }

    /// Brings the atlas up to date for `regions`, and returns what that cost.
    ///
    /// Encodes nothing at all when every region is already current — which is the
    /// common case, because chrome does not move and a board being looked at rather
    /// than edited does not change.
    pub(crate) fn refresh(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        source: &Backdrop<'_>,
        regions: &[Region],
        factor: u32,
    ) -> Refresh {
        let mut refresh = Refresh::default();
        let canvas = (source.width, source.height);
        let revision = source.revision;
        if regions.is_empty() || canvas.0 == 0 || canvas.1 == 0 {
            self.slots.clear();
            return refresh;
        }

        // A change of canvas size or of reduction factor invalidates every capture:
        // both change what a texel of the atlas means.
        let reset = self.canvas != canvas || self.factor != factor;
        self.canvas = canvas;
        self.factor = factor;

        self.dimensions.clear();
        self.dimensions
            .extend(regions.iter().map(|region| region.atlas_size(factor)));

        // Repack only when the *shape* of the demand changed. A panel being dragged
        // keeps its size, so its slot survives and only its contents are recaptured.
        let layout_holds = !reset
            && self.slots.len() == regions.len()
            && self
                .slots
                .iter()
                .zip(&self.dimensions)
                .all(|(slot, &(w, h))| slot.atlas[2] == w && slot.atlas[3] == h);

        if !layout_holds && !self.repack(device, regions) {
            // Nothing fits even at the largest texture the adapter allows. The caller
            // reads `slot(i) == None` and composites those panels as a flat tint,
            // which is the documented degradation rather than a missing panel.
            log::warn!(
                "backdrop atlas cannot hold {} regions at {}x{}; glass falls back to a flat tint",
                regions.len(),
                self.max_size,
                self.max_size
            );
            self.slots.clear();
            return refresh;
        }

        self.dirty.clear();
        for (index, region) in regions.iter().enumerate() {
            let slot = &mut self.slots[index];
            if slot.region != *region || slot.revision != revision {
                slot.region = *region;
                slot.revision = revision;
                self.dirty.push(index);
            }
        }
        if self.dirty.is_empty() {
            return refresh;
        }

        // Two instance arrays back to back in one buffer: the capture pass reads
        // regions in canvas coordinates, the blur passes read the same rectangles in
        // atlas coordinates, and a draw range picks between them.
        let atlas_size = self.size as f32;
        self.staged.clear();
        for &index in &self.dirty {
            let slot = self.slots[index];
            self.staged.push(RegionInstance {
                dst: atlas_rect(slot.atlas),
                src: slot.region.source_uv(canvas),
            });
        }
        for &index in &self.dirty {
            let slot = self.slots[index];
            self.staged.push(RegionInstance {
                dst: atlas_rect(slot.atlas),
                src: atlas_uv(slot.atlas, atlas_size),
            });
        }
        self.instances
            .write(device, queue, bytemuck::cast_slice(&self.staged));
        refresh.uploads += 1;

        self.write_params(device, queue, canvas);
        refresh.uploads += 1;

        // Built here rather than kept, because it is only valid for the view handed
        // over this frame — and because reaching this line at all already means
        // there is real work to do, so it is never the cost of an idle frame.
        let source_bind_group = texture::bind(
            device,
            &self.source_layout,
            source.texture,
            &self.sampler,
            "vellum-backdrop-canvas",
        );

        // Nothing needs preserving when every region is being rebuilt. On a tile-based
        // GPU a clear is free where a load is a full read of the attachment, three
        // times a frame, for pixels about to be overwritten anyway.
        let count = self.dirty.len() as u32;
        let preserve = self.dirty.len() < self.slots.len();

        self.encode(encoder, &source_bind_group, 0, 0, 0..count, preserve);
        for pass in 0..KAWASE_OFFSETS.len() {
            // Ping-pong, ending in slot 0 — see the assertion at [`KAWASE_OFFSETS`].
            let read = pass % 2;
            self.encode(
                encoder,
                &self.atlas[read].bind_group,
                1 - read,
                1 + pass,
                count..count * 2,
                preserve,
            );
        }

        refresh.refreshed = self.dirty.len();
        refresh.passes = 1 + KAWASE_OFFSETS.len();
        refresh.texels = self
            .dirty
            .iter()
            .map(|&index| {
                let atlas = self.slots[index].atlas;
                u64::from(atlas[2]) * u64::from(atlas[3])
            })
            .sum::<u64>()
            * refresh.passes as u64;
        refresh
    }

    /// One pass: `target` is the atlas index written, `params` the uniform block.
    ///
    /// `preserve` is false when every region is being rebuilt, which lets the
    /// attachment be cleared rather than loaded.
    fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::BindGroup,
        target: usize,
        params: usize,
        instances: std::ops::Range<u32>,
        preserve: bool,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vellum-backdrop"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.atlas[target].view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Loading is what lets a region that did not change keep the
                    // pixels it was given on an earlier frame, which is the whole
                    // point of the cache — but it is also a full read of the
                    // attachment, so it is skipped when there is nothing to keep.
                    load: if preserve {
                        wgpu::LoadOp::Load
                    } else {
                        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(if params == 0 { &self.capture } else { &self.blur });
        pass.set_bind_group(
            0,
            &self.params_bind_group,
            &[params as u32 * self.params_stride],
        );
        pass.set_bind_group(1, source, &[]);
        pass.set_vertex_buffer(0, self.instances.buffer().slice(..));
        pass.draw(0..4, instances);
    }

    fn write_params(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, canvas: (u32, u32)) {
        let stride = self.params_stride as usize;
        let atlas = [self.size as f32, self.size as f32];
        let mut blocks = vec![0u8; stride * (1 + KAWASE_OFFSETS.len())];

        let capture = BackdropParams {
            dst_size: atlas,
            src_texel: [1.0 / canvas.0.max(1) as f32, 1.0 / canvas.1.max(1) as f32],
            offset: 0.0,
            // One bilinear tap per axis already averages 2 × 2, so a `factor` × `factor`
            // box needs `factor / 2` of them per axis.
            taps: (self.factor.max(1) as f32 / 2.0).max(1.0),
            padding: [0.0; 2],
        };
        blocks[..size_of::<BackdropParams>()].copy_from_slice(bytemuck::bytes_of(&capture));

        for (pass, offset) in KAWASE_OFFSETS.iter().enumerate() {
            let block = BackdropParams {
                dst_size: atlas,
                src_texel: [1.0 / atlas[0], 1.0 / atlas[1]],
                offset: *offset,
                taps: 1.0,
                padding: [0.0; 2],
            };
            let start = (1 + pass) * stride;
            blocks[start..start + size_of::<BackdropParams>()]
                .copy_from_slice(bytemuck::bytes_of(&block));
        }

        if self.params.write(device, queue, &blocks) == Growth::Reallocated {
            self.params_bind_group =
                bind_params(device, &self.params_layout, self.params.buffer());
        }
    }

    /// Lays every region out afresh, growing the atlas until they fit.
    ///
    /// Returns false only when the largest texture the adapter supports still cannot
    /// hold them, which needs a panel wider than 8192 physical pixels.
    fn repack(&mut self, device: &wgpu::Device, regions: &[Region]) -> bool {
        let needed: u32 = self
            .dimensions
            .iter()
            .map(|&(w, h)| w.max(h))
            .max()
            .unwrap_or(1)
            .max(MIN_ATLAS);
        let mut size = self.size.max(needed.next_power_of_two());

        let placed = loop {
            if let Some(placed) = pack(size, &self.dimensions) {
                break placed;
            }
            if size >= self.max_size {
                return false;
            }
            size = (size * 2).min(self.max_size);
        };

        if size != self.size {
            self.atlas = create_atlas(device, &self.source_layout, &self.sampler, size);
            self.size = size;
        }

        self.slots.clear();
        self.slots.extend(regions.iter().zip(&placed).map(|(region, atlas)| Slot {
            region: *region,
            atlas: *atlas,
            // A revision no canvas can have, so every freshly packed slot is dirty.
            revision: u64::MAX,
        }));
        true
    }
}

fn atlas_rect(atlas: [u32; 4]) -> [f32; 4] {
    [
        atlas[0] as f32,
        atlas[1] as f32,
        atlas[2] as f32,
        atlas[3] as f32,
    ]
}

fn atlas_uv(atlas: [u32; 4], size: f32) -> [f32; 4] {
    [
        atlas[0] as f32 / size,
        atlas[1] as f32 / size,
        (atlas[0] + atlas[2]) as f32 / size,
        (atlas[1] + atlas[3]) as f32 / size,
    ]
}

fn create_atlas(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    size: u32,
) -> [AtlasTexture; 2] {
    std::array::from_fn(|index| {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vellum-backdrop-atlas"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Non-sRGB, matching the surface format `vellum-app` selects and the
            // image atlas: the blur averages in the same space the canvas was
            // composited in, so the material cannot come out lighter than the board
            // it is standing on.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = texture::bind(
            device,
            layout,
            &view,
            sampler,
            if index == 0 { "vellum-backdrop-atlas-0" } else { "vellum-backdrop-atlas-1" },
        );
        AtlasTexture { view, bind_group }
    })
}

fn bind_params(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vellum-backdrop-params"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer,
                offset: 0,
                size: wgpu::BufferSize::new(size_of::<BackdropParams>() as u64),
            }),
        }],
    })
}

/// The two backdrop pipelines differ only in their fragment entry point.
///
/// Built here rather than through [`crate::pipeline`] for one reason: these passes
/// **replace** their target instead of blending into it. Premultiplied blending is
/// right for everything that composites onto a board and wrong for a filter writing
/// a fresh copy of an image.
fn region_pipeline(
    device: &wgpu::Device,
    label: &str,
    fragment_entry: &str,
    params_layout: &wgpu::BindGroupLayout,
    source_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/backdrop.wgsl").into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(params_layout), Some(source_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_region"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<RegionInstance>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &REGION_ATTRIBUTES,
            })],
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Rounds `value` up to a multiple of `alignment`, tolerating a zero alignment the
/// way [`crate::view`] does — an adapter reporting one would otherwise divide by zero
/// before anything had a chance to report it.
fn align_to(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment.max(1)) * alignment.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The uniform is two `vec4`s. A mismatch with the WGSL would misread every tap
    /// offset and produce a blur that is subtly the wrong width, which is exactly the
    /// kind of thing nobody notices until the material looks cheap.
    #[test]
    fn the_params_block_is_two_vec4s() {
        assert_eq!(size_of::<BackdropParams>(), 32);
        assert_eq!(size_of::<RegionInstance>(), 32);
        let offsets: Vec<_> = REGION_ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 16]);
    }

    /// σ = 3 atlas texels at quarter resolution is 12 canvas pixels, which is the
    /// number the module docs claim and the number the material was tuned against.
    #[test]
    fn the_kawase_sequence_is_as_wide_as_it_claims() {
        let variance: f32 = KAWASE_OFFSETS
            .iter()
            .map(|offset| {
                let k = offset - 0.5;
                (k * k + (k + 1.0) * (k + 1.0)) / 2.0
            })
            .sum();
        assert!((variance - 9.0).abs() < 1e-5, "variance {variance}");
        assert!((variance.sqrt() - 3.0).abs() < 1e-5);
    }

    /// The margin has to cover everything the two passes and the capture can reach,
    /// or the blur near a panel's edge mixes in the region's own clamped border.
    #[test]
    fn the_margin_covers_the_whole_kernel() {
        // Capture reaches factor/2; each pass reaches (offset + 0.5) atlas texels.
        for factor in [2u32, 4] {
            let expected = factor as f32 / 2.0 + factor as f32 * (2.0 + 3.0);
            let reach = blur_reach(factor);
            assert!(reach as f32 >= expected, "factor {factor}: {reach} < {expected}");
            assert_eq!(reach % factor, 0, "factor {factor}: {reach} is not a whole texel");
        }
        assert_eq!(blur_reach(4), 24);
        assert_eq!(blur_reach(2), 12);
    }

    /// A region snaps outwards onto the reduction grid so the capture never resamples
    /// on a shifting phase — the thing that would make a static panel shimmer while
    /// the board behind it is panned.
    #[test]
    fn a_region_snaps_outwards_onto_the_reduction_grid() {
        let region = Region::around([100.0, 60.0], [200.0, 44.0], 24, 4);
        assert_eq!(region.x % 4, 0);
        assert_eq!(region.y % 4, 0);
        assert_eq!(region.width % 4, 0);
        assert_eq!(region.height % 4, 0);
        // It contains the panel with the full margin on every side.
        assert!(region.x as f32 <= 100.0 - 24.0);
        assert!(region.y as f32 <= 60.0 - 24.0);
        assert!((region.x + region.width as i32) as f32 >= 300.0 + 24.0);
        assert!((region.y + region.height as i32) as f32 >= 104.0 + 24.0);
    }

    /// A panel hanging off the left or top of the window keeps a region of the same
    /// size, because clipping it would repack the atlas on every frame of a drag.
    #[test]
    fn a_region_is_not_clipped_to_the_canvas() {
        let inside = Region::around([200.0, 200.0], [180.0, 40.0], 24, 4);
        let hanging = Region::around([-40.0, -12.0], [180.0, 40.0], 24, 4);
        assert_eq!(
            (inside.width, inside.height),
            (hanging.width, hanging.height),
            "a panel that moved off screen must not resize its slot"
        );
        assert!(hanging.x < 0 && hanging.y < 0);
        assert_eq!(hanging.x.rem_euclid(4), 0, "negative origins snap too: {hanging:?}");
        assert_eq!(hanging.y.rem_euclid(4), 0);
    }

    /// A fractional origin is what a panel animating into place actually has, and the
    /// snapped region still has to cover it.
    #[test]
    fn a_fractional_panel_is_still_fully_covered() {
        let region = Region::around([100.4, 60.7], [199.3, 43.1], 24, 4);
        assert!(region.x as f32 <= 100.4 - 24.0);
        assert!((region.x + region.width as i32) as f32 >= 100.4 + 199.3 + 24.0);
        assert!(region.y as f32 <= 60.7 - 24.0);
        assert!((region.y + region.height as i32) as f32 >= 60.7 + 43.1 + 24.0);
    }

    /// A zero-sized panel is a real shape for a collapsing popover to have for one
    /// frame, and it must not produce a zero-sized atlas slot — a texture region of
    /// no width is a validation error, not a blank panel.
    #[test]
    fn a_degenerate_panel_still_has_a_slot() {
        let region = Region::around([10.0, 10.0], [0.0, 0.0], 24, 4);
        let (w, h) = region.atlas_size(4);
        assert!(w > 0 && h > 0, "{region:?} -> {w}x{h}");
    }

    #[test]
    fn regions_pack_without_overlapping() {
        let dimensions = [(40, 120), (110, 20), (30, 30), (200, 60), (12, 12)];
        let placed = pack(256, &dimensions).expect("five small regions fit in 256 texels");
        assert_eq!(placed.len(), dimensions.len());
        for (slot, (w, h)) in placed.iter().zip(&dimensions) {
            assert_eq!((slot[2], slot[3]), (*w, *h));
            assert!(slot[0] + slot[2] <= 256 && slot[1] + slot[3] <= 256, "{slot:?} escapes");
        }
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                let apart = a[0] + a[2] <= b[0]
                    || b[0] + b[2] <= a[0]
                    || a[1] + a[3] <= b[1]
                    || b[1] + b[3] <= a[1];
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_region_larger_than_the_atlas_does_not_pack() {
        assert!(pack(64, &[(65, 10)]).is_none());
        assert!(pack(64, &[(10, 65)]).is_none());
        // And a set that individually fits but collectively does not.
        assert!(pack(64, &[(64, 40), (64, 40)]).is_none());
        assert!(pack(64, &[]).is_some(), "no regions is not a failure");
    }

    /// The realistic set §3a describes — a left toolbar, a context bar, a zoom
    /// cluster and a menu — has to fit the smallest atlas, or every frame with chrome
    /// on it would start by growing a texture.
    #[test]
    fn a_full_set_of_chrome_fits_the_smallest_atlas() {
        let panels = [
            ([16.0, 120.0], [56.0, 320.0]),
            ([420.0, 24.0], [360.0, 44.0]),
            ([1240.0, 820.0], [172.0, 40.0]),
            ([500.0, 300.0], [280.0, 220.0]),
        ];
        let dimensions: Vec<_> = panels
            .iter()
            .map(|(origin, size)| {
                Region::around(*origin, *size, blur_reach(4), 4).atlas_size(4)
            })
            .collect();
        assert!(pack(MIN_ATLAS, &dimensions).is_some(), "{dimensions:?}");
    }

    #[test]
    fn alignment_rounds_up_and_tolerates_a_zero_alignment() {
        assert_eq!(align_to(32, 256), 256);
        assert_eq!(align_to(256, 256), 256);
        assert_eq!(align_to(32, 0), 32);
    }
}
