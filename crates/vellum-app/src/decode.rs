//! Image decoding, off the frame thread.
//!
//! # Why this exists
//!
//! [`crate::assets`]'s `DECODE_BUDGET` is checked **before** a decode and never during
//! one, because nothing can preempt `image::load_from_memory` once it is running. So the
//! budget bounds how many decodes a frame *starts*, not how long one takes, and a single
//! 4000 × 3000 JPEG costs what it costs. Measured on the reference board — 596 items and
//! 205 assets — that was a **107 ms frame at 3.6 fps**, then five seconds between 18 and
//! 50 fps, RSS peaking at **0.66 GB**, before settling back to 100.
//!
//! Reported by the user as *"when i am trying to import from miro the application slows
//! down alot"*, and — because a 58-board migration starts the next import while the last
//! one is still decoding — as *"sometimes it doesnt immidietly read my writing"* and
//! *"i have to press multiple times cmd v"*. One cause, three symptoms.
//!
//! Suspending decoding under a dialog bought the keystrokes back. This is the other half:
//! the work moves to worker threads, and the frame thread only ever *uploads*.
//!
//! # Why it is safe to make this asynchronous
//!
//! `Assets::texture` has always been a **"not this frame, ask again next frame"**
//! contract — it returns `Option` and its callers already draw a placeholder and retry.
//! Nothing downstream had to change: the answer simply arrives a few frames later than it
//! used to, from a thread instead of from the frame.
//!
//! # The two bounds that matter
//!
//! - **Decoded images in memory.** The old sequential path held exactly one at a time. An
//!   unbounded queue here would be the third out-of-memory event on a machine that has
//!   kernel-panicked twice, so the answers channel is a `sync_channel` of [`READY_QUEUE`]
//!   and a worker **blocks on send** once it is full. Ceiling is `WORKERS + READY_QUEUE`
//!   decoded images — four — rather than 205.
//! - **Threads.** Two, not one per core. The frame thread is the one that must stay
//!   responsive, and this machine is 8 cores while also running the compiler.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};

use vellum_store::{BlobStore, Hash};

/// Decoding threads. See the module header for why this is not `num_cpus`.
const WORKERS: usize = 2;

/// How many decoded images may sit waiting for the frame thread to upload them.
///
/// Small on purpose: this is a *memory* bound, not a throughput one. One 4000 × 3000 RGBA
/// is 48 MB, so the whole pipeline's ceiling is roughly 190 MB of pixels in the worst
/// case, and typically far less.
const READY_QUEUE: usize = 2;

/// One decoded image, ready for the GPU.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Dimensions and a byte count — **never the pixels**.
///
/// Hand-written rather than derived on purpose: `rgba` is up to 48 MB, and a derived
/// `Debug` would dump every byte of it into a log line or a test failure. `unwrap_err`
/// on a `Result<Option<Image>, _>` needs the `Ok` side to be `Debug`, so this is reached
/// by ordinary test code rather than by anything deliberate.
impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

/// What a worker found for one asset.
pub struct Decoded {
    pub asset_id: String,
    /// `Ok(None)` is "no such blob" — the ordinary outcome of a paste with no `.rtb`
    /// beside it, and *not* an error. `Err` is present-but-undecodable, which is worth
    /// logging once and never retrying.
    pub image: Result<Option<Image>, String>,
}

/// The pool, its channels, and what it has been asked for but not yet answered.
pub struct DecodePool {
    outbound: Sender<String>,
    inbound: Receiver<Decoded>,
    /// Assets asked for and not yet returned. Keeps a request from being sent sixty times
    /// a second for every frame an image is on screen but not yet decoded — which, at one
    /// request per visible image per frame, is the whole reason this set exists.
    in_flight: HashSet<String>,
}

impl DecodePool {
    /// Starts the pool. The threads live for the process and idle on an empty channel.
    pub fn new(blobs: &BlobStore) -> Self {
        let (outbound, requests) = channel::<String>();
        let (answers, inbound) = sync_channel::<Decoded>(READY_QUEUE);
        // One receiver shared by every worker behind a mutex, exactly as `crate::links`
        // does it: `mpsc::Receiver` is not `Sync`, and a mutex around `recv` is the
        // standard way to fan one queue out to a pool.
        let requests = Arc::new(Mutex::new(requests));

        for index in 0..WORKERS {
            let requests = Arc::clone(&requests);
            let answers: SyncSender<Decoded> = answers.clone();
            // `BlobStore` is `Clone` over a bare `PathBuf`, so each worker gets its own
            // handle rather than sharing one behind a second lock.
            let blobs = blobs.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("velm-decode-{index}"))
                .spawn(move || {
                    loop {
                        // The lock is held only to take a request, never across the
                        // decode — otherwise the pool would be one thread with extra
                        // steps.
                        let request = {
                            let queue = match requests.lock() {
                                Ok(queue) => queue,
                                // A panicking sibling poisoned it. Nothing here is left
                                // half written — a request is one owned `String` — so
                                // carrying on is safe, and stopping would silently leave
                                // the board without images for the session.
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            queue.recv()
                        };
                        // The sender was dropped: the app is shutting down.
                        let Ok(asset_id) = request else { return };
                        let image = decode(&blobs, &asset_id);
                        // Blocks once `READY_QUEUE` images are waiting, which is the
                        // memory bound doing its job. `Err` means the receiver is gone.
                        if answers.send(Decoded { asset_id, image }).is_err() {
                            return;
                        }
                    }
                });
            if let Err(error) = spawned {
                log::warn!("image decoding: worker {index} would not start ({error})");
            }
        }

        Self { outbound, inbound, in_flight: HashSet::new() }
    }

    /// Asks for an asset, unless it is already being decoded.
    ///
    /// Returns whether a request actually went out.
    pub fn request(&mut self, asset_id: &str) -> bool {
        if asset_id.is_empty() || self.in_flight.contains(asset_id) {
            return false;
        }
        if self.outbound.send(asset_id.to_owned()).is_err() {
            return false;
        }
        self.in_flight.insert(asset_id.to_owned());
        true
    }

    /// Everything decoded since the last call. Never blocks.
    /// One decoded image, if a worker has finished one.
    ///
    /// **Peeled off one at a time so a caller out of upload budget simply stops asking.** The
    /// pixels then stay in the channel, the worker stays blocked behind them, and the next
    /// frame picks up where this one left off.
    ///
    /// This replaced a `drain` that pulled the whole channel, because the caller could only
    /// afford to upload one or two of them — a 2048px texture is ~22 MB and takes longer than
    /// the 4 ms budget on its own — and **threw the rest away**, clearing `in_flight` so the
    /// same files were read and decoded again on the next frame, and the next. Measured: on a
    /// board of twelve large images the same asset was decoded and discarded repeatedly while
    /// its item drew a placeholder, which is *"when i moved the things it removes the images"*.
    /// `sync_channel(READY_QUEUE)` was already the memory bound, so the discard was fighting a
    /// limit that was doing its job.
    pub fn next_ready(&mut self) -> Option<Decoded> {
        let decoded = self.inbound.try_recv().ok()?;
        self.in_flight.remove(&decoded.asset_id);
        Some(decoded)
    }

    /// How many assets are being decoded right now.
    ///
    /// `--screenshot` waits on this: a picture taken while the pool is still working is a
    /// picture of a half-loaded board, which is precisely the regression an asynchronous
    /// decoder threatens in the one tool this project verifies its chrome with.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Forgets that an asset is in flight, so it can be asked for again.
    ///
    /// For eviction: the texture went away, the answer in the channel is about to be
    /// dropped or has already been used, and the next look-up must be free to re-request.
    pub fn forget(&mut self, asset_id: &str) {
        self.in_flight.remove(asset_id);
    }
}

/// Reads and decodes one blob. Runs on a worker thread; touches no GPU state.
fn decode(blobs: &BlobStore, asset_id: &str) -> Result<Option<Image>, String> {
    let hash: Hash = asset_id.parse().map_err(|_| format!("`{asset_id}` is not a content hash"))?;
    let Some(bytes) = blobs.get(&hash).map_err(|e| e.to_string())? else {
        return Ok(None);
    };

    // A PDF is not a corrupt image, and saying so matters: the reference board carries one
    // `document` widget whose blob is a four-page PDF, and the decoder's "the image format
    // could not be determined" pointed the reader at a broken file rather than at the
    // named gap — nothing renders PDFs yet.
    if bytes.starts_with(b"%PDF-") {
        return Err("a PDF, and PDF rendering is not built yet — the bytes are stored intact"
            .to_owned());
    }

    let decoded = image::load_from_memory(&bytes).map_err(|e| e.to_string())?.into_rgba8();
    let (width, height) = decoded.dimensions();
    Ok(Some(Image { width, height, rgba: decoded.into_raw() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Favicons are still served as ICO, and leaving the codec out failed silently.**
    ///
    /// Measured against the host on the reference board: `alibaba.com/favicon.ico` answers
    /// `200 image/x-icon`, 1406 bytes, magic `00 00 01 00`, a 16×16 BMP-in-ICO. Built without
    /// `image/ico`, `load_from_memory` returned `Unsupported`, `Assets` recorded
    /// `State::Undecodable`, and **nothing retried it for the life of the session** — so the
    /// card reserved a box for an icon it would never draw. A YouTube card worked throughout,
    /// because YouTube declares a *PNG* in its `<link rel=icon>`, which is exactly the
    /// difference the user photographed between two cards on one board.
    ///
    /// The bytes are built here rather than committed, so this needs no network and no
    /// third party's asset — and it still pins the only thing that can regress, which is
    /// whether the codec is compiled in at all. `Cargo.toml` is one word away from this
    /// breaking again, and nothing else in the suite would notice.
    #[test]
    fn an_ico_favicon_decodes() {
        let icon = image::RgbaImage::from_pixel(16, 16, image::Rgba([255, 102, 0, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(icon)
            .write_to(&mut bytes, image::ImageFormat::Ico)
            .expect("the ico encoder is part of the same feature as the decoder");
        let bytes = bytes.into_inner();
        assert_eq!(&bytes[..4], &[0, 0, 1, 0], "an ICO, by its own magic");

        let decoded = image::load_from_memory(&bytes)
            .expect("an ICO favicon must decode — see Cargo.toml's `image` features");
        assert_eq!(decoded.into_rgba8().dimensions(), (16, 16));
    }

    /// The set is what stops one visible image producing sixty requests a second.
    #[test]
    fn an_asset_already_in_flight_is_not_asked_for_twice() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        let mut pool = DecodePool::new(&blobs);

        assert!(pool.request("abc"), "the first request went nowhere");
        assert!(!pool.request("abc"), "the same asset was asked for twice");
        assert_eq!(pool.in_flight(), 1);
    }

    /// An empty hash is what an item that imported without its bytes carries. It is not a
    /// fault, and it must not occupy a worker or be logged 205 times.
    #[test]
    fn an_empty_asset_id_is_never_requested() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        let mut pool = DecodePool::new(&blobs);

        assert!(!pool.request(""));
        assert_eq!(pool.in_flight(), 0);
    }

    /// A blob that is not there answers `Ok(None)` — "absent" — rather than `Err`.
    /// Conflating the two would log a warning for every card on a board pasted without
    /// its `.rtb`, which is the documented ordinary case.
    #[test]
    fn a_missing_blob_is_absent_rather_than_undecodable() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        // A well-formed hash that was never stored, spelled out rather than computed
        // so this test needs no hashing crate of its own.
        let hash = "0".repeat(64);

        assert!(matches!(decode(&blobs, &hash), Ok(None)));
    }

    /// Round trip through a real worker: store a PNG, ask for it, get its pixels back.
    #[test]
    fn a_stored_image_comes_back_decoded() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();

        let mut png = Vec::new();
        let source = image::RgbaImage::from_pixel(7, 3, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let hash = blobs.put(&png).unwrap().to_hex().to_string();

        let mut pool = DecodePool::new(&blobs);
        assert!(pool.request(&hash));

        // The worker is a real thread, so this polls rather than assuming one scheduling.
        let mut answer = None;
        for _ in 0..200 {
            answer = pool.next_ready();
            if answer.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let decoded = answer.expect("the worker never answered");
        assert_eq!(decoded.asset_id, hash);
        let image = decoded.image.expect("a valid PNG would not decode").expect("blob was absent");
        assert_eq!((image.width, image.height), (7, 3));
        assert_eq!(image.rgba.len(), 7 * 3 * 4);
        assert_eq!(pool.in_flight(), 0, "draining left the asset marked in flight");
    }

    /// A PDF is named as the gap it is, not reported as a corrupt image.
    #[test]
    fn a_pdf_names_the_missing_feature() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        let hash = blobs.put(b"%PDF-1.7 and then some").unwrap().to_hex().to_string();

        let error = decode(&blobs, &hash).unwrap_err();
        assert!(error.contains("PDF rendering is not built yet"), "{error}");
    }
}
