//! Fetching a board's pictures and getting them onto the GPU.
//!
//! # Why this is not a port of `vellum-app`'s decode pool
//!
//! `decode.rs` runs two OS threads and bounds memory by *blocking the producer* on a
//! `sync_channel`. `wasm32-unknown-unknown` has neither, and reaching for wasm threads would
//! be choosing the harder path for a worse result: **`createImageBitmap` decodes on the
//! browser's own thread pool**, off the main thread, with no worker plumbing at all.
//!
//! `docs/01-architecture.md` §1 lists "every decoded image crossing native → IPC → JS → WASM →
//! GPU with at least two copies" as a standing cost of WASM. For images it is the opposite:
//! the browser decodes, and only the finished RGBA crosses into wasm memory once.
//!
//! # The two bounds that replace the blocking channel
//!
//! **In flight**, so a pan across a picture-heavy board does not start two hundred fetches at
//! once, and **uploads per frame**, so arriving pixels cannot stall a frame. Both are counts
//! rather than a time budget, deliberately: `vellum-app` measured that a 4 ms budget checked
//! *before* a decode bounds how many decodes a frame starts, not how long one takes, and a
//! single large upload blew straight through it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::JsCast;

use vellum_render::{ImageSource, TextureId, TextureManager};

/// Outstanding `createImageBitmap` calls. One 2048² RGBA is 16 MB in wasm memory, and that
/// memory is never returned to the OS, so the peak is what the tab keeps forever.
const MAX_IN_FLIGHT: usize = 4;

/// Textures uploaded per frame. An upload is a `write_texture` plus a CPU mip chain; several
/// large ones in a frame is a visible stall.
const MAX_UPLOADS_PER_FRAME: usize = 2;

/// Decoded pixels waiting for the frame thread.
struct Decoded {
    hash: String,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

enum State {
    /// Fetch or decode is running.
    Pending,
    Ready(TextureId, (u32, u32)),
    /// Tried and cannot be drawn. **Never retried**: a hash that does not decode this second
    /// will not decode next second either, and asking again once a frame for the life of the
    /// tab is how a missing picture becomes a performance bug.
    Undecodable,
}

pub struct ImageLayer {
    states: HashMap<String, State>,
    in_flight: usize,
    ready: Rc<RefCell<Vec<Decoded>>>,
    /// A blob's URL is `base` + its hash + `suffix`.
    ///
    /// Two halves rather than one, and the second half is not decoration: `velmd` accepts a
    /// bearer token in the query string as well as in a header, because a page's own module
    /// and wasm fetches cannot carry a header. So a blob behind a token is
    /// `/api/v1/blobs/<hash>?token=…` — a query that has to land *after* the hash, which a
    /// single prefix cannot express.
    base: String,
    suffix: String,
}

impl ImageLayer {
    pub fn new(base: impl Into<String>, suffix: impl Into<String>) -> Self {
        Self {
            states: HashMap::new(),
            in_flight: 0,
            ready: Rc::new(RefCell::new(Vec::new())),
            base: base.into(),
            suffix: suffix.into(),
        }
    }

    /// The texture for `hash`, starting a fetch if this is the first time it was asked for.
    ///
    /// `None` means "not this frame, ask again" — the same contract `vellum_app::Assets`
    /// offers, which is what lets every caller stay synchronous.
    pub fn texture(&mut self, hash: &str) -> Option<(TextureId, (u32, u32))> {
        match self.states.get(hash) {
            Some(State::Ready(id, source)) => return Some((*id, *source)),
            Some(State::Pending | State::Undecodable) => return None,
            None => {}
        }
        if self.in_flight >= MAX_IN_FLIGHT {
            return None;
        }
        self.states.insert(hash.to_owned(), State::Pending);
        self.in_flight += 1;
        self.start(hash.to_owned());
        None
    }

    fn start(&self, hash: String) {
        let url = format!("{}{hash}{}", self.base, self.suffix);
        let ready = Rc::clone(&self.ready);
        wasm_bindgen_futures::spawn_local(async move {
            match decode(&url).await {
                Ok((width, height, rgba)) => {
                    ready.borrow_mut().push(Decoded { hash, width, height, rgba });
                }
                Err(error) => {
                    log::warn!("image {hash} did not decode: {error}");
                    // Pushed with no pixels so the frame thread can mark it undecodable.
                    // Dropping it silently would leave the entry Pending for ever and hold a
                    // slot in the in-flight budget, which is a leak with a four-image fuse.
                    ready.borrow_mut().push(Decoded { hash, width: 0, height: 0, rgba: Vec::new() });
                }
            }
        });
    }

    /// Release textures the budget cannot afford, and forget what they were for.
    ///
    /// ⚠ Uploading does **not** evict — `TextureManager::upload` allocates, uploads a mip
    /// chain and inserts, and eviction is a separate call the caller has to drive. Without
    /// it, panning the reference board's 122 images uploads every one at up to 2048² plus
    /// mips and keeps them for the life of the tab: gigabytes against a budget nothing was
    /// enforcing, which on an iPad is a jetsam kill rather than a slow frame.
    ///
    /// Called **after** the draw list is built, so `mark` has already run for everything on
    /// screen this frame. `vellum-app` moved this call for exactly that reason: before the
    /// marks, the `last_marked` guard is vacuously true and eviction can take a texture the
    /// list being built still references.
    pub fn enforce_budget(&mut self, textures: &mut TextureManager) {
        for id in textures.evict_to_budget() {
            // The hash entry has to go with the texture, or the layer reports `Ready` for an
            // id the manager has released and every later frame draws nothing where the
            // picture was, for ever, with no way to ask again.
            self.states.retain(|_, state| !matches!(state, State::Ready(resident, _) if *resident == id));
        }
    }

    /// Upload what has arrived. Called once per frame, before the draw list is built.
    pub fn drain(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        textures: &mut TextureManager,
    ) {
        for _ in 0..MAX_UPLOADS_PER_FRAME {
            let Some(decoded) = self.ready.borrow_mut().pop() else { break };
            self.in_flight = self.in_flight.saturating_sub(1);
            if decoded.rgba.is_empty() {
                self.states.insert(decoded.hash, State::Undecodable);
                continue;
            }
            let source = ImageSource::new(decoded.width, decoded.height, &decoded.rgba);
            let dimensions = (decoded.width, decoded.height);
            match textures.upload(device, queue, &source) {
                Ok(id) => {
                    self.states.insert(decoded.hash, State::Ready(id, dimensions));
                }
                Err(error) => {
                    log::warn!("could not upload {}: {error}", decoded.hash);
                    self.states.insert(decoded.hash, State::Undecodable);
                }
            }
        }
    }

}

/// Fetch, decode and read back one image's pixels.
///
/// The decode happens in `createImageBitmap` — on the browser's threads, not ours — and the
/// only thing that crosses into wasm is the finished RGBA.
async fn decode(url: &str) -> Result<(u32, u32, Vec<u8>), String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|_| "fetch failed".to_owned())?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "bad response")?;
    if !response.ok() {
        return Err(format!("HTTP {}", response.status()));
    }
    let blob = wasm_bindgen_futures::JsFuture::from(
        response.blob().map_err(|_| "no body")?,
    )
    .await
    .map_err(|_| "could not read the body")?;
    let blob: web_sys::Blob = blob.dyn_into().map_err(|_| "not a blob")?;

    let bitmap = wasm_bindgen_futures::JsFuture::from(
        window
            .create_image_bitmap_with_blob(&blob)
            .map_err(|_| "createImageBitmap refused this data")?,
    )
    .await
    .map_err(|_| "could not decode the image")?;
    let bitmap: web_sys::ImageBitmap = bitmap.dyn_into().map_err(|_| "not a bitmap")?;

    let (width, height) = (bitmap.width(), bitmap.height());
    if width == 0 || height == 0 {
        return Err("decoded to nothing".to_owned());
    }

    // An OffscreenCanvas is the only way back to raw bytes. `willReadFrequently` is
    // deliberately not set: each of these is read exactly once.
    let canvas = web_sys::OffscreenCanvas::new(width, height)
        .map_err(|_| "cannot make an offscreen canvas")?;
    let ctx = canvas
        .get_context("2d")
        .map_err(|_| "no 2d context")?
        .ok_or("no 2d context")?
        .dyn_into::<web_sys::OffscreenCanvasRenderingContext2d>()
        .map_err(|_| "wrong context type")?;
    ctx.draw_image_with_image_bitmap(&bitmap, 0.0, 0.0)
        .map_err(|_| "could not draw the bitmap")?;
    // Closed explicitly rather than left to the collector: a bitmap holds decoded pixels
    // outside wasm memory, and on a board of two hundred that is real.
    bitmap.close();

    let data = ctx
        .get_image_data(0.0, 0.0, width as f64, height as f64)
        .map_err(|_| "could not read the pixels back")?;
    Ok((width, height, data.data().to_vec()))
}
