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
}

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
        for answer in self.pool.drain() {
            if self.spent >= DECODE_BUDGET {
                // Out of budget with pixels in hand. Forget that it was in flight so the
                // next frame asks again rather than dropping the image on the floor —
                // `texture` only requests assets it has no binding for, so without this
                // the asset would never be asked for again and would never appear.
                self.pool.forget(&answer.asset_id);
                continue;
            }
            let started = Instant::now();
            let state = match answer.image {
                Ok(Some(image)) => {
                    self.decoded += 1;
                    self.bytes_decoded += image.rgba.len() as u64;
                    let source = ImageSource::new(image.width, image.height, &image.rgba);
                    match textures.upload(device, queue, &source) {
                        Ok(texture) => {
                            // `upload` deliberately ignores the budget so a caller is
                            // never left holding an image it cannot draw; enforcing it is
                            // the caller's job, here.
                            let evicted = textures.evict_to_budget();
                            self.bindings.evicted(&evicted);
                            State::Resident { texture, size: (image.width, image.height) }
                        }
                        Err(error) => {
                            log::warn!("asset {}: {error}", answer.asset_id);
                            State::Undecodable
                        }
                    }
                }
                Ok(None) => State::Absent,
                Err(error) => {
                    log::warn!("asset {}: {error}", answer.asset_id);
                    State::Undecodable
                }
            };
            self.bindings.set(&answer.asset_id, state);
            self.spent += started.elapsed();
        }
    }

    /// Drops the mapping for textures the renderer evicted.
    ///
    /// The bytes are gone from the GPU; the hash is still in the document, so the
    /// entry becomes *unknown* rather than *absent* — the next time the image is on
    /// screen it is uploaded again from the blob store, which is exactly the
    /// contract [`TextureManager::evict_to_budget`] documents.
    pub fn evicted(&mut self, textures: &[TextureId]) {
        self.bindings.evicted(textures);
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
