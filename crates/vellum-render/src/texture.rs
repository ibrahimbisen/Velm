//! Image residency: what is on the GPU, at what size, and what gets thrown away
//! first.
//!
//! `docs/01-architecture.md` §3 states the problem plainly: the reference board's
//! **205 images decoded to RGBA exceed 1.5 GB** against a 400 MB idle-memory budget.
//! Downscaling on upload and evicting on pressure are therefore not optimisations to
//! add later — without both, the app cannot open its own reference board. This module
//! is the whole of that policy.
//!
//! # Three decisions
//!
//! **Downscale on upload, by exact halving.** An image is halved until neither side
//! exceeds [`TextureBudget::max_dimension`]. Halving rather than resampling to an
//! arbitrary size means a box filter with no kernel, no ringing and no phase error,
//! and it is what the mip chain is already doing anyway. A 4000 × 3000 photo becomes
//! 1000 × 750 against a 2048 cap — 12 MB of RGBA down to 750 KB before mips.
//!
//! **Mip chains are built on the CPU.** wgpu can generate them with a blit pipeline,
//! which is faster, but it requires `RENDER_ATTACHMENT` usage and a render pass per
//! level. Building them here keeps this module free of pipelines, keeps every
//! residency decision in one file, and makes the filter itself testable with
//! `cargo test` on a machine with no GPU. The chain is generated from the *already
//! downscaled* image, so it costs a third of a small image rather than a third of a
//! large one. Without mips, a photo drawn at 5% zoom aliases into noise, which is
//! exactly the view where the whole board is on screen.
//!
//! **Texels are stored premultiplied.** Bilinear filtering and mip reduction both
//! average neighbouring texels, and averaging straight-alpha colours pulls colour out
//! of fully transparent pixels — the classic dark halo around a cut-out PNG. The
//! image pipeline blends premultiplied to match; see `crate::pipeline`.
//!
//! # Eviction
//!
//! Evicting frees GPU memory and nothing else: this module never keeps the source
//! pixels, because doing so would move the same 1.5 GB into RAM. A caller therefore
//! has to be able to re-upload — which it can, since images live in the
//! content-addressed blob store — and [`TextureManager::evict_to_budget`] returns the
//! ids it dropped so the caller's own hash-to-id map can be pruned.
//!
//! # Resolution, which is the part eviction cannot do
//!
//! Measured on the reference board at fit-zoom: **785 MB resident against a 268 MB
//! budget, and not one byte evictable** — every one of the 122 images was on screen,
//! so there was nothing stale. Eviction reported that honestly and could do nothing
//! about it, because the problem was never *which* images were resident. It was that
//! each was resident at a resolution nothing on screen could show.
//!
//! [`TextureManager::resolve_detail`] fixes that. Each frame the renderer reports the
//! size every image is drawn at ([`TextureManager::note_drawn`]); a texture with more
//! detail than any draw asked for is **demoted** — rebuilt from its own mip chain,
//! entirely on the GPU, keeping its [`TextureId`] and so invisible to the caller —
//! and one too coarse for a draw is **dropped** so the caller re-uploads it from the
//! blob store. `crate::detail` holds the thresholds and the reasoning.
//!
//! Demotion needs no source pixels because the data is already there: level *k* of
//! the old texture becomes level 0 of the new one, which is `copy_texture_to_texture`
//! and no decode, no CPU filtering and no round trip through RAM. Refinement is the
//! asymmetric half — the finer texels are genuinely gone — and is paced to a few
//! textures per frame to match the caller's per-frame decode budget.

use crate::detail::{self, Detail, DetailPolicy};
use crate::error::RenderError;
use std::collections::HashMap;

/// Opaque handle to a resident texture. Dead once the texture is evicted; a caller
/// keying on content hashes re-uploads and receives a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextureId(pub(crate) u64);

/// Row-major, tightly packed RGBA8 with **straight** alpha — the form an image
/// decoder produces and the form `vellum-import` hands over.
#[derive(Debug, Clone, Copy)]
pub struct ImageSource<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

impl<'a> ImageSource<'a> {
    pub fn new(width: u32, height: u32, rgba: &'a [u8]) -> Self {
        Self { width, height, rgba }
    }

    fn validate(&self) -> Result<(), RenderError> {
        if self.width == 0 || self.height == 0 {
            return Err(RenderError::EmptyImage);
        }
        let expected = self.width as usize * self.height as usize * 4;
        if self.rgba.len() != expected {
            return Err(RenderError::ImageSize {
                width: self.width,
                height: self.height,
                expected,
                actual: self.rgba.len(),
            });
        }
        Ok(())
    }
}

/// How much GPU memory images may occupy, and how large any one of them may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureBudget {
    /// Ceiling on resident texture bytes, mip levels included.
    pub max_bytes: usize,
    /// Neither side of a stored image exceeds this.
    pub max_dimension: u32,
}

impl Default for TextureBudget {
    /// 256 MB inside the 400 MB idle budget of `docs/01-architecture.md`, leaving
    /// room for the document, the spatial index and the glyph atlas. 2048 px is the
    /// size at which a board image still looks sharp at 100% on a Retina display and
    /// costs 16 MB — so the budget holds about fifteen of them at once, which is more
    /// than any one screenful of a board contains.
    fn default() -> Self {
        Self { max_bytes: 256 << 20, max_dimension: 2048 }
    }
}


/// Frames of visibility a texture is protected by.
///
/// One, so a texture drawn on the *previous* frame survives as well as one drawn on this one.
/// The guard used to be `last_marked < frame` alone, which is right only if eviction runs after
/// the frame's marks — and the caller runs it before the draw list is built, because evicting
/// after the list is built strands textures that list already references. Both orderings are
/// wrong with a zero-frame window; a one-frame window is correct in either.
const PROTECT_FRAMES: u64 = 1;

struct Resident {
    /// Kept, not just its bind group: demotion copies out of it.
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    /// Mip levels the texture holds. Always `1 + floor(log2(max(width, height)))`.
    levels: u32,
    /// The source image's size, so the finest level it could be restored to can be
    /// recomputed even if [`TextureBudget::max_dimension`] changes under it.
    source: (u32, u32),
    bytes: usize,
    /// Frame counter when this texture was last marked visible.
    last_marked: u64,
    /// World-pixel distance from the viewport at that moment; 0 means on screen.
    distance: f64,
    /// The largest whole-image texel size any draw has asked for since the last
    /// [`TextureManager::resolve_detail`]. `None` means nothing drew it.
    demanded: Option<(f32, f32)>,
}

pub struct TextureManager {
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    resident: HashMap<TextureId, Resident>,
    budget: TextureBudget,
    policy: DetailPolicy,
    bytes: usize,
    peak_bytes: usize,
    next_id: u64,
    frame: u64,
    /// Frame whose observations [`TextureManager::resolve_detail`] last acted on.
    /// `u64::MAX` so the first call always runs, whether or not the caller keeps a
    /// frame clock at all.
    resolved_frame: u64,
    /// What the last resolve found too coarse. **Every one is still resident** — see
    /// [`TextureManager::wants_refinement`], and [`TextureManager::begin_frame`] for what
    /// this used to be and what it cost.
    pending_refinement: Vec<Refinement>,
}

/// One texture the last resolve found too coarse, and the size it should come back at.
///
/// Carries the target because the demand it was derived from does not survive: `demanded` is
/// `take()`n out of the `Resident` as it is read, and the upload happens several frames later,
/// in another crate, by which time there is nothing left to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refinement {
    /// Still bound, still drawable. This is a request, not a handle to a dead texture.
    pub texture: TextureId,
    /// Top-level size to re-upload at, already capped by [`TextureBudget::max_dimension`].
    pub target: (u32, u32),
}

/// One texture `resolve_detail` found too coarse, before pacing picks which four to ask for.
///
/// Named rather than a tuple because the sort and the filter each read a different pair of its
/// fields, and `a.0.cmp(&b.0).then(a.1.cmp(&b.1))` says nothing about which is the deficit.
#[derive(Debug, Clone, Copy)]
struct RefineCandidate {
    /// Negative levels: how far below the demanded size the stored one is. Sorted ascending,
    /// so the most obviously soft image is refined first.
    deficit: i32,
    texture: TextureId,
    target: (u32, u32),
    stored: (u32, u32),
}

impl Refinement {
    /// The cap to hand [`TextureManager::upload_within`].
    #[must_use]
    pub fn max_dimension(&self) -> u32 {
        self.target.0.max(self.target.1)
    }
}

impl TextureManager {
    pub fn new(device: &wgpu::Device, budget: TextureBudget) -> Self {
        Self {
            layout: bind_group_layout(device, "vellum-image-layout"),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("vellum-image-sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                // The WebGPU baseline guarantees 16×; anisotropy is what keeps a
                // rotated or steeply-scaled image from blurring along one axis, and
                // board images are routinely drawn at a fraction of their size.
                anisotropy_clamp: 16,
                ..Default::default()
            }),
            resident: HashMap::new(),
            budget,
            policy: DetailPolicy::default(),
            bytes: 0,
            peak_bytes: 0,
            next_id: 0,
            frame: 0,
            resolved_frame: u64::MAX,
            pending_refinement: Vec::new(),
        }
    }

    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    pub fn budget(&self) -> TextureBudget {
        self.budget
    }

    pub fn set_budget(&mut self, budget: TextureBudget) {
        self.budget = budget;
    }

    /// How aggressively resolution follows the drawn size. See [`DetailPolicy`].
    pub fn detail_policy(&self) -> DetailPolicy {
        self.policy
    }

    pub fn set_detail_policy(&mut self, policy: DetailPolicy) {
        self.policy = policy;
    }

    /// Total resident bytes, mip levels included.
    pub fn resident_bytes(&self) -> usize {
        self.bytes
    }

    /// The highest [`Self::resident_bytes`] has ever been. What a memory budget is
    /// actually spent against — a steady state under budget is worth nothing if
    /// reaching it went three times over.
    pub fn peak_resident_bytes(&self) -> usize {
        self.peak_bytes
    }

    pub fn len(&self) -> usize {
        self.resident.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resident.is_empty()
    }

    pub fn contains(&self, id: TextureId) -> bool {
        self.resident.contains_key(&id)
    }

    /// Stored size in texels, after downscaling and any demotion. Not the source size.
    pub fn size(&self, id: TextureId) -> Option<(u32, u32)> {
        self.resident.get(&id).map(|r| (r.width, r.height))
    }

    /// The source image's size, as handed to [`Self::upload`].
    pub fn source_size(&self, id: TextureId) -> Option<(u32, u32)> {
        self.resident.get(&id).map(|r| r.source)
    }

    /// The finest top level this image could be restored to: its source, capped by
    /// [`TextureBudget::max_dimension`]. A texture already at its ceiling is as sharp
    /// as it will ever be, whatever the camera does.
    pub fn detail_ceiling(&self, id: TextureId) -> Option<(u32, u32)> {
        self.resident
            .get(&id)
            .map(|r| capped_size(r.source.0, r.source.1, self.budget.max_dimension))
    }

    pub fn bind_group(&self, id: TextureId) -> Option<&wgpu::BindGroup> {
        self.resident.get(&id).map(|r| &r.bind_group)
    }

    /// Downscales, premultiplies, builds the mip chain and uploads every level.
    ///
    /// Does **not** enforce the budget: an upload that pushes residency over it still
    /// succeeds, because failing here would leave the caller with an image it cannot
    /// draw and no way to recover. Call [`Self::evict_to_budget`] afterwards, which
    /// is free to throw away something else instead.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &ImageSource<'_>,
    ) -> Result<TextureId, RenderError> {
        self.upload_within(device, queue, image, self.budget.max_dimension)
    }

    /// [`Self::upload`], with an explicit ceiling on the stored top level.
    ///
    /// For a refinement, which knows the size it is about to be drawn at — see
    /// [`Refinement::max_dimension`]. Uploading one at [`TextureBudget::max_dimension`] costs
    /// 21.3 MB with mips and the very next `resolve_detail` demotes it to about 1.3 MB in the
    /// same frame, so the work is thrown away before it is drawn twice. Four of those a frame
    /// during a zoom is most of the residency budget spent on nothing.
    ///
    /// Never exceeds the budget's own cap: that one is a memory decision and this one is a
    /// sharpness decision, and the memory decision wins.
    pub fn upload_within(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &ImageSource<'_>,
        max_dimension: u32,
    ) -> Result<TextureId, RenderError> {
        image.validate()?;

        let levels = mip_chain(
            image.width,
            image.height,
            premultiply(image.rgba),
            max_dimension.clamp(1, self.budget.max_dimension),
        );
        let (width, height, _) = levels[0];
        let bytes: usize = levels.iter().map(|(_, _, data)| data.len()).sum();

        let texture = allocate(device, width, height, levels.len() as u32);

        for (level, (w, h, data)) in levels.iter().enumerate() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(*h),
                },
                wgpu::Extent3d { width: *w, height: *h, depth_or_array_layers: 1 },
            );
        }

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = bind(device, &self.layout, &view, &self.sampler, "vellum-image");

        let id = TextureId(self.next_id);
        self.next_id += 1;
        self.bytes += bytes;
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        self.resident.insert(
            id,
            Resident {
                texture,
                bind_group,
                width,
                height,
                levels: levels.len() as u32,
                source: (image.width, image.height),
                bytes,
                last_marked: self.frame,
                distance: 0.0,
                demanded: None,
            },
        );
        log::debug!(
            "uploaded {}x{} image as {width}x{height} in {} levels, {bytes} bytes; {} resident",
            image.width,
            image.height,
            levels.len(),
            self.bytes
        );
        Ok(id)
    }

    /// Advances the frame counter.
    ///
    /// Anything marked since the last call is protected from eviction, because
    /// evicting a texture the current frame needs would guarantee a re-upload stall
    /// in the middle of a pan.
    ///
    /// # It used to destroy textures here, and that was the flicker
    ///
    /// A texture the last resolve found too coarse was **removed** — and that removal *was* the
    /// message to the caller, which noticed the handle had gone and re-uploaded. The invariant
    /// this file claimed, *"no frame is ever missing the image"*, held exactly as long as
    /// `Assets::texture` decoded and uploaded inline in the same pass.
    ///
    /// `vellum_app::decode` later moved decoding onto worker threads and recorded that
    /// *"nothing downstream had to change: the answer simply arrives a few frames later"*.
    /// That is true of `Assets`' own `Option` contract and false of this one. The two modules'
    /// contracts diverged, and a same-frame invisible swap became several frames of flat grey
    /// placeholder on every image a zoom swept past — *"when i zoom in and out images flicker
    /// alot"*.
    ///
    /// So nothing is destroyed to ask a question now. See [`Self::wants_refinement`].
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// The textures the last resolve found too coarse, and the size each should come back at.
    ///
    /// **Not a destruction order.** Every one of these is still resident, still bound and still
    /// drawing at its current resolution; this is the manager asking the caller to upload a
    /// finer one and hand the old handle back. The caller keeps showing the coarse texture
    /// until the replacement is ready, which is the whole fix.
    ///
    /// Rebuilt wholesale by each [`Self::resolve_detail`], so it is a per-frame observation
    /// rather than a work queue — a texture that stops being drawn stops asking.
    pub fn wants_refinement(&self) -> &[Refinement] {
        &self.pending_refinement
    }

    /// Records that `id` is `distance` world pixels from the viewport this frame.
    /// Zero means on screen.
    pub fn mark(&mut self, id: TextureId, distance: f64) {
        if let Some(resident) = self.resident.get_mut(&id) {
            resident.last_marked = self.frame;
            resident.distance = if distance.is_finite() { distance.max(0.0) } else { f64::MAX };
        }
    }

    /// Records that something drew `id` at a size needing `texels` of the whole image
    /// — width and height, in texels, of the image as a whole rather than of the
    /// visible crop. `crate::detail::demanded_texels` converts one draw into it, and
    /// [`crate::Renderer::prepare`] does that for every image instance of a frame.
    ///
    /// Accumulates the maximum, because a texture drawn twice has to satisfy the
    /// larger of the two. Calling this is what makes [`Self::resolve_detail`] do
    /// anything at all; a texture nobody reports is left exactly as it is.
    pub fn note_drawn(&mut self, id: TextureId, texels: (f32, f32)) {
        let Some(resident) = self.resident.get_mut(&id) else { return };
        if !texels.0.is_finite() || !texels.1.is_finite() {
            return;
        }
        resident.demanded = Some(match resident.demanded {
            Some((w, h)) => (w.max(texels.0), h.max(texels.1)),
            None => texels,
        });
    }

    /// Matches every observed texture's resolution to the size it was drawn at.
    ///
    /// Runs at most once per frame. A caller preparing two targets from one frame —
    /// a thumbnail and the window — calls this twice; the second call decides nothing
    /// and its observations are carried into the next frame's decision instead, so a
    /// small offscreen pass can never demote what the window pass is about to need.
    ///
    /// Returns how many textures were rebuilt smaller. Those keep their [`TextureId`]
    /// and their contents; nothing above them in the stack can tell. Textures found
    /// *too coarse* are queued for the next [`Self::begin_frame`] instead.
    pub fn resolve_detail(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> usize {
        if self.resolved_frame == self.frame {
            return 0;
        }
        self.resolved_frame = self.frame;

        let mut demotions: Vec<(TextureId, u32)> = Vec::new();
        // Deficit first, so the paced refinement spends its frames on the images that
        // are most obviously soft rather than on whichever the map happened to yield.
        // The target and the stored size are captured *here* because `demanded` is consumed by
        // the `take()` above and the upload is several frames and one crate away — there is
        // nothing left to derive them from by then.
        let mut refinements: Vec<RefineCandidate> = Vec::new();
        for (id, resident) in &mut self.resident {
            let Some(demanded) = resident.demanded.take() else { continue };
            let ceiling = capped_size(resident.source.0, resident.source.1, self.budget.max_dimension);
            let stored = (resident.width, resident.height);
            match detail::decide(&self.policy, stored, ceiling, demanded) {
                Detail::Keep => {}
                Detail::Demote(levels) => demotions.push((*id, levels)),
                Detail::Refine => refinements.push(RefineCandidate {
                    deficit: detail::surplus_levels(stored, demanded),
                    texture: *id,
                    target: detail::refine_target(&self.policy, ceiling, demanded),
                    stored,
                }),
            }
        }

        self.pending_refinement.clear();
        if self.policy.refinements_per_frame > 0 {
            refinements.sort_by(|a, b| a.deficit.cmp(&b.deficit).then(a.texture.cmp(&b.texture)));
            self.pending_refinement.extend(
                refinements
                    .iter()
                    // A target no larger than what is already stored buys nothing and costs a
                    // full decode. Only reachable when `min_dimension` has clamped a demote,
                    // but it is one comparison against a wasted round trip.
                    .filter(|c| c.target.0 > c.stored.0 || c.target.1 > c.stored.1)
                    .take(self.policy.refinements_per_frame)
                    .map(|c| Refinement { texture: c.texture, target: c.target }),
            );
        }

        let mut demoted = 0;
        if !demotions.is_empty() {
            // Sorted so a run is reproducible: the map's iteration order is not, and a
            // residency figure that moves between runs cannot be used as a budget.
            demotions.sort_by_key(|(id, _)| *id);
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vellum-image-demote"),
            });
            for (id, levels) in demotions {
                if self.demote(device, &mut encoder, id, levels) {
                    demoted += 1;
                }
            }
            // One submission for the whole frame's demotions: each is a handful of
            // small copies, and a submit per texture would cost more than the copies.
            queue.submit([encoder.finish()]);
        }

        self.report(demoted);
        demoted
    }

    /// The residency line. Logged when the resident set changed shape and otherwise
    /// every [`REPORT_INTERVAL`] frames, so a run that settles still says where it
    /// settled — which is the only way to check a memory budget after the fact.
    fn report(&self, demoted: usize) {
        let queued = self.pending_refinement.len();
        if demoted == 0 && queued == 0 && !self.frame.is_multiple_of(REPORT_INTERVAL) {
            return;
        }
        log::info!(
            "image detail: {demoted} demoted, {queued} queued to refine; {} textures, {} bytes resident, {} peak, {} budget",
            self.resident.len(),
            self.bytes,
            self.peak_bytes,
            self.budget.max_bytes,
        );
    }

    /// Rebuilds `id` without its top `levels` mip levels, on the GPU.
    ///
    /// The data is already there — level *k* of the old texture is exactly what level
    /// 0 of the new one should hold — so this is a run of `copy_texture_to_texture`
    /// and no decode, no filtering and no round trip through RAM. The [`TextureId`],
    /// and therefore every reference the caller holds, survives.
    fn demote(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        id: TextureId,
        levels: u32,
    ) -> bool {
        let Some(resident) = self.resident.get_mut(&id) else { return false };
        if levels == 0 || levels >= resident.levels {
            return false;
        }

        let width = (resident.width >> levels).max(1);
        let height = (resident.height >> levels).max(1);
        let remaining = resident.levels - levels;
        let texture = allocate(device, width, height, remaining);

        for level in 0..remaining {
            let extent = wgpu::Extent3d {
                width: (width >> level).max(1),
                height: (height >> level).max(1),
                depth_or_array_layers: 1,
            };
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &resident.texture,
                    mip_level: levels + level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                extent,
            );
        }

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bytes = chain_bytes(width, height);
        self.bytes = self.bytes + bytes - resident.bytes;
        resident.bind_group = bind(device, &self.layout, &view, &self.sampler, "vellum-image");
        resident.texture = texture;
        resident.width = width;
        resident.height = height;
        resident.levels = remaining;
        resident.bytes = bytes;
        true
    }

    /// Frees textures until residency is within budget, and returns what went.
    ///
    /// Order is stalest first, ties broken by greatest distance from the viewport.
    /// Both halves matter and they are not the same signal: staleness says the user
    /// has stopped looking at a region, and distance says how expensive it would be
    /// to need it again. Because `distance` is only refreshed by [`Self::mark`], a
    /// stale entry's distance is exactly how far off-screen it was when last seen,
    /// which is the right tie-break after a jump across the board makes everything
    /// stale at once.
    ///
    /// Never evicts anything marked in the current frame. If the budget still cannot
    /// be met after that, it is left over budget rather than thrashing — that is a
    /// budget too small for one screenful, and dropping something needed right now
    /// would only turn a memory problem into a stutter.
    pub fn evict_to_budget(&mut self) -> Vec<TextureId> {
        if self.bytes <= self.budget.max_bytes {
            return Vec::new();
        }

        let candidates = eviction_order(
            self.resident
                .iter()
                .filter(|(_, r)| r.last_marked + PROTECT_FRAMES < self.frame)
                .map(|(id, r)| Candidate {
                    last_marked: r.last_marked,
                    distance: r.distance,
                    bytes: r.bytes,
                    id: *id,
                })
                .collect(),
        );

        let mut evicted = Vec::new();
        for candidate in candidates {
            if self.bytes <= self.budget.max_bytes {
                break;
            }
            self.resident.remove(&candidate.id);
            self.bytes -= candidate.bytes;
            evicted.push(candidate.id);
        }

        if self.bytes > self.budget.max_bytes {
            log::warn!(
                "texture residency is {} bytes against a {} byte budget and everything left is in use",
                self.bytes,
                self.budget.max_bytes
            );
        } else if !evicted.is_empty() {
            log::debug!("evicted {} textures, {} bytes resident", evicted.len(), self.bytes);
        }
        evicted
    }

    /// Drops one texture regardless of budget or recency — for an image the document
    /// no longer contains.
    pub fn remove(&mut self, id: TextureId) -> bool {
        match self.resident.remove(&id) {
            Some(resident) => {
                self.bytes -= resident.bytes;
                true
            }
            None => false,
        }
    }

    pub fn clear(&mut self) {
        self.resident.clear();
        self.pending_refinement.clear();
        self.bytes = 0;
    }
}

/// Frames between residency reports once nothing is moving. About four seconds at
/// 60 Hz — often enough to see a leak, rare enough not to be noise in a log.
const REPORT_INTERVAL: u64 = 240;

/// A texture of `levels` mip levels, ready to be written into and copied out of.
///
/// `COPY_SRC` is what makes demotion possible at all: without it, shrinking a texture
/// would mean decoding the source again, which is the cost the whole scheme exists to
/// avoid.
fn allocate(device: &wgpu::Device, width: u32, height: u32, levels: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vellum-image"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: levels.max(1),
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // Non-sRGB to match the surface format `vellum-app` selects: Miro blends
        // in sRGB as browsers do, and letting the sampler linearise would make
        // every imported image subtly lighter than the original.
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// RGBA bytes of a full mip chain from `width` × `height` down to 1 × 1.
///
/// The accounting unit throughout this module. It is the texel count, not the
/// driver's allocation — tiling and alignment add a few percent that no API reports —
/// so it is comparable across levels and across runs, which is what a budget needs.
fn chain_bytes(width: u32, height: u32) -> usize {
    let (mut w, mut h) = (width.max(1), height.max(1));
    let mut total = 0usize;
    loop {
        total += w as usize * h as usize * 4;
        if w == 1 && h == 1 {
            return total;
        }
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
}

/// The size an image is stored at: halved until neither side exceeds the cap.
///
/// Exact halving rather than a resample to the cap — see this module's header — so
/// this is the same arithmetic [`mip_chain`] performs, expressed without the pixels.
fn capped_size(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
    let cap = max_dimension.max(1);
    let (mut w, mut h) = (width.max(1), height.max(1));
    while w > cap || h > cap {
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    (w, h)
}

/// A crop of a texture, in normalised coordinates.
///
/// The whole point of expressing a crop this way: changing it rewrites four floats in
/// an instance and uploads nothing, so cropping an image is free at any size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UvRect {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl UvRect {
    pub const FULL: Self = Self { min: [0.0, 0.0], max: [1.0, 1.0] };

    pub fn new(min: [f32; 2], max: [f32; 2]) -> Self {
        Self { min, max }
    }

    /// From a pixel rect within an image of `size` texels.
    pub fn from_pixels(x: u32, y: u32, width: u32, height: u32, size: (u32, u32)) -> Self {
        let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
        Self {
            min: [x as f32 / w, y as f32 / h],
            max: [(x + width) as f32 / w, (y + height) as f32 / h],
        }
    }

    pub(crate) fn packed(self) -> [f32; 4] {
        [self.min[0], self.min[1], self.max[0], self.max[1]]
    }
}

/// One texture considered for eviction.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    last_marked: u64,
    distance: f64,
    bytes: usize,
    id: TextureId,
}

/// Stalest first, then furthest from the viewport, then largest. Split out from
/// [`TextureManager::evict_to_budget`] so the policy — the part that decides which
/// image the user sees reappear as a blank rectangle — is testable without a GPU.
fn eviction_order(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        a.last_marked
            .cmp(&b.last_marked)
            .then(b.distance.total_cmp(&a.distance))
            .then(b.bytes.cmp(&a.bytes))
            .then(a.id.cmp(&b.id))
    });
    candidates
}

pub(crate) fn bind_group_layout(device: &wgpu::Device, label: &str) -> wgpu::BindGroupLayout {
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

pub(crate) fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
    label: &str,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}

/// Straight RGBA to premultiplied, in place of the caller's buffer.
fn premultiply(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for texel in out.chunks_exact_mut(4) {
        let a = u32::from(texel[3]);
        for channel in &mut texel[..3] {
            // Rounded rather than truncated: truncation drifts a fully opaque white
            // to 254 and shows up as a seam where a premultiplied image meets an
            // unpremultiplied one.
            *channel = ((u32::from(*channel) * a + 127) / 255) as u8;
        }
    }
    out
}

/// Halves `data` until it fits `max_dimension`, then continues down to 1 × 1.
///
/// Returns `(width, height, texels)` per level, largest first. Always at least one
/// level, because an image is never zero-sized by the time it gets here.
fn mip_chain(
    width: u32,
    height: u32,
    data: Vec<u8>,
    max_dimension: u32,
) -> Vec<(u32, u32, Vec<u8>)> {
    let max_dimension = max_dimension.max(1);
    let (mut w, mut h, mut level) = (width, height, data);
    while w > max_dimension || h > max_dimension {
        let (nw, nh, next) = halve(w, h, &level);
        w = nw;
        h = nh;
        level = next;
    }

    let mut chain = vec![(w, h, level)];
    while w > 1 || h > 1 {
        let (nw, nh, next) = {
            let (_, _, previous) = chain.last().expect("the chain starts with one level");
            halve(w, h, previous)
        };
        w = nw;
        h = nh;
        chain.push((w, h, next));
    }
    chain
}

/// A 2 × 2 box filter. Odd dimensions clamp the second sample onto the last row or
/// column, which repeats an edge texel rather than reading past the buffer.
fn halve(width: u32, height: u32, src: &[u8]) -> (u32, u32, Vec<u8>) {
    let dst_width = (width / 2).max(1);
    let dst_height = (height / 2).max(1);
    let mut dst = vec![0u8; dst_width as usize * dst_height as usize * 4];

    let texel = |x: u32, y: u32| -> usize {
        (y.min(height - 1) as usize * width as usize + x.min(width - 1) as usize) * 4
    };

    for y in 0..dst_height {
        for x in 0..dst_width {
            let corners = [
                texel(x * 2, y * 2),
                texel(x * 2 + 1, y * 2),
                texel(x * 2, y * 2 + 1),
                texel(x * 2 + 1, y * 2 + 1),
            ];
            let out = (y as usize * dst_width as usize + x as usize) * 4;
            for channel in 0..4 {
                let sum: u32 = corners.iter().map(|&c| u32::from(src[c + channel])).sum();
                dst[out + channel] = ((sum + 2) / 4) as u8;
            }
        }
    }
    (dst_width, dst_height, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, texel: [u8; 4]) -> Vec<u8> {
        texel.repeat(width as usize * height as usize)
    }

    #[test]
    fn premultiplication_scales_colour_by_alpha_and_rounds() {
        // Opaque white must stay exactly white — truncation would give 254.
        assert_eq!(premultiply(&[255, 255, 255, 255]), vec![255, 255, 255, 255]);
        assert_eq!(premultiply(&[255, 0, 0, 128]), vec![128, 0, 0, 128]);
        // A fully transparent texel keeps no colour at all, which is what stops a
        // filtered edge dragging it into its neighbours.
        assert_eq!(premultiply(&[255, 255, 255, 0]), vec![0, 0, 0, 0]);
    }

    #[test]
    fn halving_averages_two_by_two_blocks() {
        // A 2x2 image, one texel per corner.
        let src = [
            0, 0, 0, 255, //
            255, 255, 255, 255, //
            255, 255, 255, 255, //
            255, 255, 255, 255,
        ];
        let (w, h, out) = halve(2, 2, &src);
        assert_eq!((w, h), (1, 1));
        // (0 + 255 + 255 + 255 + 2) / 4 = 191
        assert_eq!(out, vec![191, 191, 191, 255]);
    }

    #[test]
    fn halving_an_odd_dimension_clamps_rather_than_reading_past_the_edge() {
        let (w, h, out) = halve(3, 1, &solid(3, 1, [10, 20, 30, 40]));
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![10, 20, 30, 40]);
        let (w, h, _) = halve(1, 1, &solid(1, 1, [1, 2, 3, 4]));
        assert_eq!((w, h), (1, 1), "a 1x1 image cannot halve further");
    }

    #[test]
    fn a_chain_starts_below_the_cap_and_ends_at_one_texel() {
        let chain = mip_chain(4000, 3000, solid(4000, 3000, [7, 7, 7, 255]), 2048);
        assert_eq!((chain[0].0, chain[0].1), (2000, 1500), "one halving clears a 2048 cap");
        assert_eq!((chain.last().unwrap().0, chain.last().unwrap().1), (1, 1));
        for (w, h, data) in &chain {
            assert_eq!(data.len(), *w as usize * *h as usize * 4);
        }
        // Every level halves, so the chain is short even for a large image.
        assert_eq!(chain.len(), 11);
    }

    /// The two effects, separated. Halving to clear the cap quarters the top level;
    /// the mip chain then adds a third of that back. A 48 MB photo becomes 16 MB
    /// resident — which is still 3.3 GB across the reference board's 205 images, and
    /// is exactly why eviction has to exist as well.
    #[test]
    fn downscaling_quarters_a_photo_and_the_mip_chain_adds_a_third_back() {
        let source_bytes = 4000usize * 3000 * 4;
        let chain = mip_chain(4000, 3000, solid(4000, 3000, [7, 7, 7, 255]), 2048);
        let top = chain[0].2.len();
        let resident: usize = chain.iter().map(|(_, _, d)| d.len()).sum();

        assert_eq!(top, source_bytes / 4);
        assert!(resident * 3 < top * 4 + 8, "the chain overshot a third: {resident} over {top}");
        assert!(resident * 2 < source_bytes, "{resident} against {source_bytes}");
    }

    /// The accounting and the pixels have to agree. `chain_bytes` is what every
    /// residency figure and every budget decision is computed from, and `capped_size`
    /// is what decides whether a texture could be re-uploaded any sharper — both are
    /// predictions about what [`mip_chain`] would produce, made without the pixels.
    /// A drift between them is a residency figure that quietly stops being true.
    #[test]
    fn the_byte_and_size_predictions_match_the_chain_they_predict() {
        for (w, h) in [(2048, 2048), (4000, 3000), (37, 5), (1, 1), (1024, 3)] {
            let chain = mip_chain(w, h, solid(w, h, [1, 2, 3, 4]), 2048);
            let top = (chain[0].0, chain[0].1);
            assert_eq!(capped_size(w, h, 2048), top, "top level of {w}x{h}");
            assert_eq!(
                chain_bytes(top.0, top.1),
                chain.iter().map(|(_, _, d)| d.len()).sum::<usize>(),
                "chain bytes of {w}x{h}"
            );
            assert_eq!(
                chain.len() as u32,
                1 + top.0.max(top.1).ilog2(),
                "level count of {w}x{h}"
            );
        }
        assert_eq!(capped_size(0, 0, 0), (1, 1), "a degenerate size still terminates");
    }

    /// Demotion drops levels off the top of an existing chain, so the size and level
    /// count it computes have to be the ones the *tail* of that chain already has.
    /// Off by one and the copy reads a level that is not there.
    #[test]
    fn dropping_levels_lands_on_a_level_the_chain_already_holds() {
        let chain = mip_chain(2048, 1024, solid(2048, 1024, [9; 4]), 2048);
        for drop in 0..chain.len() as u32 {
            let width = (2048u32 >> drop).max(1);
            let height = (1024u32 >> drop).max(1);
            assert_eq!((width, height), (chain[drop as usize].0, chain[drop as usize].1));
            assert_eq!(chain.len() as u32 - drop, 1 + width.max(height).ilog2());
        }
    }

    #[test]
    fn an_image_already_within_the_cap_keeps_its_size() {
        let chain = mip_chain(64, 32, solid(64, 32, [1, 2, 3, 4]), 2048);
        assert_eq!((chain[0].0, chain[0].1), (64, 32));
        assert_eq!(chain.len(), 7, "64 -> 32 -> 16 -> 8 -> 4 -> 2 -> 1");
    }

    #[test]
    fn a_mismatched_buffer_is_rejected_before_it_reaches_the_gpu() {
        let short = ImageSource::new(4, 4, &[0; 16]);
        assert!(matches!(short.validate(), Err(RenderError::ImageSize { expected: 64, actual: 16, .. })));
        assert!(matches!(ImageSource::new(0, 4, &[]).validate(), Err(RenderError::EmptyImage)));
        assert!(ImageSource::new(4, 4, &[0; 64]).validate().is_ok());
    }

    fn candidate(last_marked: u64, distance: f64, id: u64) -> Candidate {
        Candidate { last_marked, distance, bytes: 1 << 20, id: TextureId(id) }
    }

    /// The policy in one assertion: an image the user stopped looking at two frames
    /// ago goes before one they stopped looking at last frame, however far away it
    /// is; among equally stale images, the furthest one goes first.
    #[test]
    fn eviction_takes_the_stalest_and_then_the_furthest() {
        let order = eviction_order(vec![
            candidate(9, 100_000.0, 1),
            candidate(2, 0.0, 2),
            candidate(9, 0.0, 3),
            candidate(2, 5_000.0, 4),
        ]);
        let ids: Vec<_> = order.iter().map(|c| c.id.0).collect();
        assert_eq!(ids, vec![4, 2, 1, 3]);
    }

    #[test]
    fn eviction_order_is_deterministic_for_identical_candidates() {
        let order = eviction_order(vec![candidate(1, 0.0, 7), candidate(1, 0.0, 3)]);
        assert_eq!(order.iter().map(|c| c.id.0).collect::<Vec<_>>(), vec![3, 7]);
    }

    /// A distance that is not a number — a NaN leaking out of a degenerate camera —
    /// must sort as "as far away as possible" rather than corrupting the order.
    #[test]
    fn a_nonsense_distance_does_not_scramble_the_order() {
        let order = eviction_order(vec![
            candidate(1, f64::NAN, 1),
            candidate(1, 10.0, 2),
            candidate(1, 0.0, 3),
        ]);
        assert_eq!(order.iter().map(|c| c.id.0).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn a_uv_rect_from_pixels_covers_the_named_region() {
        let crop = UvRect::from_pixels(50, 25, 50, 25, (100, 50));
        assert_eq!(crop.min, [0.5, 0.5]);
        assert_eq!(crop.max, [1.0, 1.0]);
        assert_eq!(UvRect::FULL.packed(), [0.0, 0.0, 1.0, 1.0]);
        // A zero-sized image must not divide by zero.
        assert!(UvRect::from_pixels(0, 0, 1, 1, (0, 0)).max[0].is_finite());
    }
}
