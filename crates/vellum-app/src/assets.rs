//! Board assets: bytes on disk, pixels on the GPU, and the traffic between them.
//!
//! An item stores a BLAKE3 content hash, never pixels — that is what lets one photo
//! used on twenty boards be stored once, and what stops a board file depending on
//! Miro's id namespace. Turning a hash into something drawable means reading a file,
//! decoding it and uploading it, and `docs/01-architecture.md` §3 is blunt about the
//! scale: **the reference board's 205 images decode to more than 1.5 GB** against a
//! 400 MB idle budget.
//!
//! Two mechanisms keep that from being a problem, and neither belongs in the draw
//! path:
//!
//! - [`vellum_render::TextureManager`] owns residency — downscaling, mip chains and
//!   eviction. This module owns the *mapping* from content hash to whatever handle
//!   it currently has, and prunes it when eviction takes one away.
//! - Decoding happens on **worker threads** — [`crate::decode`] — and this module only
//!   ever *uploads* what they finished. A 4000 × 3000 JPEG takes tens of milliseconds
//!   to decode and nothing can preempt it once begun, so a per-frame time budget could
//!   bound how many decodes a frame *started* but never how long one took: measured on
//!   the reference board that was a 107 ms frame. The budget survives, applied to
//!   uploads, which unlike decodes can be stopped between images. Pictures appear over
//!   the next few frames, which is what every map and every photo grid does.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use vellum_render::{ImageSource, TextureId, TextureManager};
use vellum_store::BlobStore;

/// How long one frame may spend decoding and uploading. Small enough to stay inside
/// a 120 Hz frame alongside everything else, large enough that a modest image lands
/// in one go rather than being deferred forever.
const DECODE_BUDGET: Duration = Duration::from_millis(4);

/// What is known about one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State<T> {
    /// On the GPU. `size` is the **source** image's size, before
    /// [`TextureManager`]'s downscaling — because Miro's `crop` is in source pixels
    /// and a UV rect derived from the stored size would shift every crop on any
    /// image large enough to have been downscaled.
    Resident { texture: T, size: (u32, u32) },
    /// Referenced by an item but not in the blob store — the ordinary outcome of a
    /// clipboard paste with no `.rtb` alongside it.
    Absent,
    /// Present but not decodable. Recorded so the decode is not retried every frame
    /// for the life of the session.
    Undecodable,
}

/// The two-way map between content hashes and GPU handles.
///
/// Split out from [`Assets`] and generic over the handle so the bookkeeping — the
/// part with an invariant worth checking, that the reverse map never outlives the
/// forward one — is testable on a machine with no GPU.
#[derive(Debug)]
struct Bindings<T> {
    by_hash: HashMap<String, State<T>>,
    by_texture: HashMap<T, String>,
}

impl<T> Default for Bindings<T> {
    fn default() -> Self {
        Self { by_hash: HashMap::new(), by_texture: HashMap::new() }
    }
}

impl<T: Copy + Eq + std::hash::Hash> Bindings<T> {
    fn get(&self, asset_id: &str) -> Option<State<T>> {
        self.by_hash.get(asset_id).copied()
    }

    fn set(&mut self, asset_id: &str, state: State<T>) {
        if let State::Resident { texture, .. } = state {
            self.by_texture.insert(texture, asset_id.to_string());
        }
        self.by_hash.insert(asset_id.to_string(), state);
    }

    /// Forgets an asset entirely, so the next look-up re-reads it from disk.
    fn forget(&mut self, asset_id: &str) {
        if let Some(State::Resident { texture, .. }) = self.by_hash.remove(asset_id) {
            self.by_texture.remove(&texture);
        }
    }

    /// The asset a handle belongs to, if it is still bound.
    fn hash_of(&self, texture: T) -> Option<String> {
        self.by_texture.get(&texture).cloned()
    }

    /// Points `asset_id` at a new handle and hands back the one it displaced.
    ///
    /// **Atomic, and the return is the point.** [`Self::set`] inserts into the reverse map and
    /// does not remove the previous handle's entry — harmless today only because every rebind
    /// is preceded by a `forget` (which is what
    /// `rebinding_after_a_forget_leaves_no_stale_reverse_entry` was written about). A
    /// refinement swap has no `forget` in front of it, so doing this by hand would leave
    /// `by_texture[old]` pointing at a live asset, and the next `evicted(&[old])` would
    /// unbind the texture that is currently drawing.
    fn rebind(&mut self, asset_id: &str, state: State<T>) -> Option<T> {
        let previous = match self.by_hash.get(asset_id) {
            Some(State::Resident { texture, .. }) => Some(*texture),
            _ => None,
        };
        if let Some(old) = previous {
            self.by_texture.remove(&old);
        }
        self.set(asset_id, state);
        previous
    }

    /// Drops the mappings for handles the renderer evicted.
    fn evicted(&mut self, textures: &[T]) {
        for texture in textures {
            if let Some(hash) = self.by_texture.remove(texture) {
                self.by_hash.remove(&hash);
            }
        }
    }

    fn resident(&self) -> usize {
        self.by_texture.len()
    }
}

/// The hash-to-texture map, and the decode budget that fills it.
pub struct Assets {
    blobs: BlobStore,
    bindings: Bindings<TextureId>,
    /// Decoding, on worker threads. See [`crate::decode`] for why it is not inline.
    pool: crate::decode::DecodePool,
    spent: Duration,
    decoded: u64,
    bytes_decoded: u64,
    /// Refinements asked for and not yet landed: asset id → the size to upload at.
    ///
    /// Held here rather than read from the renderer each frame because the renderer's answer
    /// is a *per-frame observation* that vanishes on any frame the image is not drawn, while
    /// the decode takes several frames to come back.
    refining: HashMap<String, (u32, u32)>,
}

/// Refinement decodes outstanding at once.
///
/// Four, matching `DetailPolicy::refinements_per_frame` — but this is the binding constraint,
/// not that one: the renderer bounds what it *exposes* per frame and this bounds what is
/// actually *decoding*, and the two worker threads are the scarce resource.
///
/// It also bounds the memory the whole fix costs. A superseded texture is kept alive until its
/// replacement lands, and a texture at its ceiling never refines, so each is at most one
/// halving below the 2048 cap — four of those is about 21 MB, on a machine with 8 GB that has
/// kernel-panicked during compilation twice. Against that, the old path uploaded 21 MB *per
/// refinement* and threw it away in the same frame, so the peak goes down.
const MAX_REFINEMENTS_IN_FLIGHT: usize = 4;

impl std::fmt::Debug for Assets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assets")
            .field("root", &self.blobs.root())
            .field("resident", &self.bindings.resident())
            .finish()
    }
}

impl Assets {
    pub fn new(blobs: BlobStore) -> Self {
        Self {
            pool: crate::decode::DecodePool::new(&blobs),
            blobs,
            bindings: Bindings::default(),
            spent: Duration::ZERO,
            decoded: 0,
            bytes_decoded: 0,
            refining: HashMap::new(),
        }
    }

    /// Whether any asset is still being decoded.
    ///
    /// `--screenshot` waits on this — see [`crate::decode::DecodePool::in_flight`].
    pub fn is_busy(&self) -> bool {
        self.pool.in_flight() > 0
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Assets decoded since startup, and their total decoded size in bytes.
    pub fn decode_stats(&self) -> (u64, u64) {
        (self.decoded, self.bytes_decoded)
    }

    /// How many assets are on the GPU right now.
    pub fn resident(&self) -> usize {
        self.bindings.resident()
    }

    /// Resets the per-frame decode budget. Call once per frame.
    pub fn begin_frame(&mut self) {
        self.spent = Duration::ZERO;
    }

    /// Spends this frame's whole decode budget without decoding anything, so no image
    /// work starts until the next frame that does not call it.
    ///
    /// **[`DECODE_BUDGET`] is checked before a decode and never during one**, because
    /// nothing can preempt `image::load_from_memory` once it is running. So the budget
    /// bounds how many decodes a frame *starts*, not how long one takes: a single
    /// 4000 × 3000 JPEG costs what it costs. Measured on the reference board — 596
    /// items and 205 assets — that is a **107 ms frame at 3.6 fps**, then five seconds
    /// between 18 and 50 fps before it settles back to 100.
    ///
    /// Five seconds of that is tolerable while looking at a board arriving. It is not
    /// tolerable underneath a dialog the user is **typing into**, which is exactly where
    /// it landed: importing one Miro board after another, the name field for the *next*
    /// board is being typed while the *previous* board's assets are still decoding, and
    /// keystrokes go into 100 ms frames. Reported as *"sometimes it doesn't immediately
    /// read my writing / typing the name"*.
    ///
    /// So while an overlay is up the board behind it stops decoding and draws the
    /// placeholders it already draws for an image that has not arrived yet. This does
    /// **not** make the import slower overall — the same work happens over the same
    /// number of frames afterwards — it moves it off the frames where a keypress has to
    /// land. Nothing is dropped or forgotten: [`Self::texture`] is a "not this frame,
    /// ask again" contract, which is the whole reason this is a legal thing to do.
    pub fn suspend_decoding(&mut self) {
        self.spent = DECODE_BUDGET;
    }

    /// The texture for `asset_id`, and the **source** image's dimensions, decoding
    /// and uploading it if there is budget.
    ///
    /// `None` means "not this frame": the asset is missing, undecodable, or the
    /// frame has already spent its decode budget. A caller draws a placeholder and
    /// asks again next frame — which is why this returns an `Option` rather than
    /// blocking or erroring.
    pub fn texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        textures: &mut TextureManager,
        asset_id: &str,
    ) -> Option<(TextureId, (u32, u32))> {
        match self.bindings.get(asset_id) {
            Some(State::Resident { texture, size }) => {
                // A texture the manager evicted since we last looked is stale here.
                if textures.contains(texture) {
                    return Some((texture, size));
                }
                self.bindings.forget(asset_id);
            }
            Some(State::Absent | State::Undecodable) => return None,
            None => {}
        }

        // Not here, and not this frame. The decode happens on a worker and the upload in
        // [`Self::collect`]; this only asks. `device`, `queue` and `textures` stay in the
        // signature because callers pass them and because that is where the upload used
        // to be — see the module header before "simplifying" them away.
        let _ = (device, queue, textures);
        if self.spent < DECODE_BUDGET {
            self.pool.request(asset_id);
        }
        None
    }

    /// Uploads everything the decoders finished, and records what they could not do.
    ///
    /// Called once a frame from `draw::Painter::paint`, which is the one place that holds
    /// the device, the queue, the texture manager and this together — and it runs *before*
    /// the draw list that references the textures is built, so an image that arrives this
    /// frame is drawn this frame rather than next.
    ///
    /// The budget still applies, but it now bounds **uploads**, which is a bound that
    /// works: an upload is a memcpy of a known size and can be stopped between images,
    /// where a decode could not be stopped at all once begun. Anything left in the
    /// channel simply waits for the next frame.
    pub fn collect(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        textures: &mut TextureManager,
    ) {
        self.request_refinements(textures);

        // **Take answers only while there is budget to upload them**, and leave the rest in
        // the channel. This used to drain the whole channel and *discard* everything past the
        // budget — clearing `in_flight` so the same file was read and decoded again next
        // frame, and again, while its item drew a placeholder throughout. One 2048px upload
        // costs more than the whole 4 ms on its own, so on a board of large images that was
        // every answer but the first, every frame.
        while self.spent < DECODE_BUDGET {
            let Some(answer) = self.pool.next_ready() else { break };
            // `Some` iff this answer is a *refinement* of something already on screen, which
            // is what makes the two failure paths below different from a first load's.
            let refine_target = self.refining.remove(&answer.asset_id);
            let started = Instant::now();
            match answer.image {
                Ok(Some(image)) => {
                    self.decoded += 1;
                    self.bytes_decoded += image.rgba.len() as u64;
                    let source = ImageSource::new(image.width, image.height, &image.rgba);
                    let cap = refine_target
                        .map_or(textures.budget().max_dimension, |(w, h)| w.max(h));
                    match textures.upload_within(device, queue, &source, cap) {
                        Ok(texture) => {
                            let state =
                                State::Resident { texture, size: (image.width, image.height) };
                            // **The swap.** The old texture is freed in the same statement it
                            // is replaced, which is safe on both sides: `collect` runs at the
                            // top of `Painter::paint`, before this frame's list is built, so
                            // no list that will be drawn references it; and wgpu keeps the
                            // allocation alive until the command buffers using it retire.
                            if let Some(old) = self.bindings.rebind(&answer.asset_id, state) {
                                textures.remove(old);
                            }
                            // `upload` deliberately ignores the budget so a caller is
                            // never left holding an image it cannot draw; enforcing it is
                            // the caller's job — see `enforce_budget`, which now runs after
                            // the frame's marks rather than here, before any of them.
                        }
                        Err(error) => {
                            log::warn!("asset {}: {error}", answer.asset_id);
                            // A refinement that fails keeps what is already on screen. Writing
                            // `Undecodable` here would turn an image that has been drawing for
                            // minutes into a permanent placeholder because one re-read failed.
                            if refine_target.is_none() {
                                self.bindings.set(&answer.asset_id, State::Undecodable);
                            }
                        }
                    }
                }
                // Same rule for both: on a *first* load these are the real answer; on a
                // refinement the pixels already on the GPU are the last copy, and throwing
                // them away because the blob moved is strictly worse than keeping them.
                Ok(None) => {
                    if refine_target.is_none() {
                        self.bindings.set(&answer.asset_id, State::Absent);
                    }
                }
                Err(error) => {
                    log::warn!("asset {}: {error}", answer.asset_id);
                    if refine_target.is_none() {
                        self.bindings.set(&answer.asset_id, State::Undecodable);
                    }
                }
            }
            self.spent += started.elapsed();
        }
    }

    /// Asks the decode pool for the finer uploads the renderer wants.
    ///
    /// The texture stays resident and drawing throughout — that is the whole difference from
    /// what this replaced, where the renderer signalled by *destroying* the texture and the
    /// image fell back to a flat placeholder for the several frames the decode took.
    fn request_refinements(&mut self, textures: &TextureManager) {
        let wanted: Vec<vellum_render::Refinement> = textures.wants_refinement().to_vec();
        for want in wanted {
            // Decided last frame; the budget sweep may have evicted it since. With no reverse
            // entry this is not a refinement any more, and `texture`'s ordinary miss path will
            // request it from scratch.
            let Some(asset_id) = self.bindings.hash_of(want.texture) else { continue };
            // The zoom is still moving, so last frame's target is stale. Always take the newest
            // one, even for a job already in flight: one map write against a second decode.
            if let Some(target) = self.refining.get_mut(&asset_id) {
                *target = want.target;
                continue;
            }
            if self.refining.len() >= MAX_REFINEMENTS_IN_FLIGHT {
                continue;
            }
            // Refinement never queues ahead of a first load. An image being refined is on
            // screen at a slightly soft resolution; one being loaded is a grey rectangle.
            if self.pool.in_flight() > self.refining.len() {
                continue;
            }
            if self.pool.request(&asset_id) {
                self.refining.insert(asset_id, want.target);
            }
        }
    }

    /// Brings texture residency back inside its budget, with this frame's visibility in hand.
    ///
    /// **Strictly after the last `TextureManager::mark` of the frame.** It used to run inside
    /// `collect`, which runs at the top of `Painter::paint` — *before* every one of this
    /// frame's marks — so `evict_to_budget`'s `last_marked < frame` guard was vacuously true
    /// for every texture and eviction could take an image out from under the list about to be
    /// built. That is the reference board at a fitted zoom, where the genuinely stale set is
    /// empty and eviction reaches straight into what is on screen.
    ///
    /// Safe to run after the list is built: anything in it was marked this frame, so the guard
    /// refuses it, and `Renderer::prepare` already tolerates a bind group that has gone.
    pub fn enforce_budget(&mut self, textures: &mut TextureManager) {
        let evicted = textures.evict_to_budget();
        for id in &evicted {
            if let Some(hash) = self.bindings.hash_of(*id) {
                self.refining.remove(&hash);
            }
        }
        self.bindings.evicted(&evicted);
    }

}

/// The UV sub-rect for a crop expressed in **source image** pixels.
///
/// Miro's `crop` is in the original image's pixels, and `TextureManager` downscales
/// on upload — so the stored texture is very often smaller than the image the crop
/// was authored against. Normalising against the *source* size and not the stored
/// one is what keeps a crop correct after a 4000 px photo becomes a 2048 px texture.
pub fn crop_uv(crop: vellum_doc::Crop, source: (f64, f64)) -> vellum_render::UvRect {
    let (width, height) = (source.0.max(1.0), source.1.max(1.0));
    let min = [
        (crop.x / width).clamp(0.0, 1.0) as f32,
        (crop.y / height).clamp(0.0, 1.0) as f32,
    ];
    let max = [
        ((crop.x + crop.width) / width).clamp(0.0, 1.0) as f32,
        ((crop.y + crop.height) / height).clamp(0.0, 1.0) as f32,
    ];
    vellum_render::UvRect::new(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::Crop;

    #[test]
    fn a_crop_normalises_against_the_source_image_not_the_stored_texture() {
        let crop = Crop { x: 1000.0, y: 750.0, width: 2000.0, height: 1500.0 };
        let uv = crop_uv(crop, (4000.0, 3000.0));
        assert_eq!(uv.min, [0.25, 0.25]);
        assert_eq!(uv.max, [0.75, 0.75]);
    }

    /// A crop authored against a larger image than the one on disk — or a corrupt
    /// value — must clamp rather than sample outside the texture, which on a clamped
    /// sampler smears the edge row across the whole item.
    #[test]
    fn an_out_of_range_crop_is_clamped() {
        let uv = crop_uv(
            Crop { x: -100.0, y: -100.0, width: 9_000.0, height: 9_000.0 },
            (1000.0, 1000.0),
        );
        assert_eq!(uv.min, [0.0, 0.0]);
        assert_eq!(uv.max, [1.0, 1.0]);
    }

    #[test]
    fn a_degenerate_source_size_does_not_divide_by_zero() {
        let uv = crop_uv(Crop { x: 0.0, y: 0.0, width: 10.0, height: 10.0 }, (0.0, 0.0));
        assert!(uv.min.iter().chain(uv.max.iter()).all(|v| v.is_finite()));
    }

    /// An evicted texture must leave *no* trace: an entry that stayed behind would
    /// hand the renderer a dead handle, and the image would silently never come back.
    #[test]
    fn eviction_forgets_the_hash_so_it_can_be_uploaded_again() {
        let mut bindings: Bindings<u32> = Bindings::default();
        bindings.set("abc", State::Resident { texture: 1, size: (10, 10) });
        bindings.set("def", State::Resident { texture: 2, size: (20, 20) });
        assert_eq!(bindings.resident(), 2);

        bindings.evicted(&[1]);

        assert_eq!(bindings.get("abc"), None, "an evicted asset stayed bound");
        assert_eq!(bindings.get("def"), Some(State::Resident { texture: 2, size: (20, 20) }));
        assert_eq!(bindings.resident(), 1);
    }

    /// A missing asset is remembered as missing. Without that, every frame retries a
    /// file that is not there — which on the reference board pasted without its
    /// `.rtb` is 205 failed reads per frame.
    #[test]
    fn an_absent_asset_is_recorded_rather_than_retried() {
        let mut bindings: Bindings<u32> = Bindings::default();
        bindings.set("abc", State::Absent);
        assert_eq!(bindings.get("abc"), Some(State::Absent));
        assert_eq!(bindings.resident(), 0, "an absent asset took a texture slot");
    }

    /// Re-binding a hash to a new handle must not leave the old one pointing at it,
    /// or a later eviction of the *old* handle would unbind the live texture.
    #[test]
    fn rebinding_after_a_forget_leaves_no_stale_reverse_entry() {
        let mut bindings: Bindings<u32> = Bindings::default();
        bindings.set("abc", State::Resident { texture: 1, size: (8, 8) });
        bindings.forget("abc");
        bindings.set("abc", State::Resident { texture: 2, size: (8, 8) });

        bindings.evicted(&[1]);
        assert_eq!(bindings.get("abc"), Some(State::Resident { texture: 2, size: (8, 8) }));
    }

    #[test]
    fn the_decode_budget_resets_every_frame() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        let mut assets = Assets::new(blobs);

        assets.spent = DECODE_BUDGET * 2;
        assets.begin_frame();
        assert_eq!(assets.spent, Duration::ZERO);
        assert_eq!(assets.decode_stats(), (0, 0));
    }

    /// A suspended frame must refuse a decode *and* leave the next frame free to do one.
    ///
    /// The second half is the one worth pinning: `suspend_decoding` spends the budget
    /// rather than latching a flag, so a caller that stopped calling it — the dialog
    /// closed — gets decoding back at the next `begin_frame` with no second call to
    /// remember. A latch would have to be cleared, and the frame that forgot would
    /// starve the board of every image for the rest of the session.
    #[test]
    fn suspending_costs_this_frame_only() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        let mut assets = Assets::new(blobs);

        assets.begin_frame();
        assets.suspend_decoding();
        assert!(assets.spent >= DECODE_BUDGET, "a suspended frame still had budget to decode");

        assets.begin_frame();
        assert_eq!(assets.spent, Duration::ZERO, "the suspension outlived its frame");
    }
}
