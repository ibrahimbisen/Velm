//! Velm in a browser tab.
//!
//! # What this is, and deliberately is not
//!
//! A **read-only viewer**: it fetches a board's Loro snapshot over HTTP, projects it into a
//! scene, and draws what the camera can see through WebGPU. Pan and zoom work. Nothing here
//! can change a board, and that is a safety property rather than a scoping compromise —
//! `docs/08-web.md` records the reasoning. A browser can kill a background tab with no
//! warning and no chance to flush; a client that could edit would, at that moment, be
//! holding the only recent copy of a board that cannot be re-imported.
//!
//! # Why there is no SQLite here
//!
//! The server runs `vellum-store` natively and unchanged. All this needs from the document
//! layer is [`vellum_doc::Board::from_bytes`] — one pure function over a byte slice. That is
//! the single decision that makes a browser client tractable at all; every plan that puts
//! the database in the tab spends months reimplementing storage that already works.
//!
//! # Two things the browser forces that native did not
//!
//! **Adapter and device are `async`.** `vellum-app` wraps both in `pollster::block_on`, which
//! cannot exist here — blocking the browser's main thread is not slow, it is impossible. So
//! startup is a future, and nothing renders until it resolves.
//!
//! **`Instant::now()` panics on `wasm32-unknown-unknown`.** `web-time` provides it against
//! `performance.now()`. Native Velm hit the same wall and solved it the same way.

#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use vellum_doc::Board;
use vellum_project::project::Projection;
use vellum_render::{DrawList, Renderer, Rgba, View};
use vellum_scene::{Camera, ScreenSize, SceneItem};

/// How much slack a fitted board leaves around itself, as a fraction of its own extent.
///
/// The same 0.02 `vellum_app::FIT_MARGIN` uses. Duplicated rather than imported because
/// `vellum-app` does not compile for this target at all — and two bytes of constant is a
/// better dependency than a crate with SQLite and a native menu bar in it.
const FIT_MARGIN: f64 = 0.02;

mod badges;
mod board;
mod live;
mod widgets;
mod images;
mod layout;
mod input;
mod shapes;
mod strokes;
mod text;

/// Everything a frame needs, once startup has resolved.
struct Viewer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    /// The 4x multisampled colour attachment the board is drawn into, resolved to the canvas.
    ///
    /// Not optional. `vellum-render` builds every board pipeline at
    /// [`vellum_render::BOARD_SAMPLES`], and WebGPU rejects a pipeline whose sample count
    /// does not match the pass -- which is exactly how this was found: the GPU refused
    /// `vellum-quad` against a single-sampled pass and the board drew nothing at all.
    msaa: wgpu::TextureView,
    /// The document itself, kept rather than dropped after the first projection.
    ///
    /// ⚠ It used to be dropped: `boot` parsed the snapshot, built the projection and let the
    /// `Board` go, because nothing downstream needed it. **Sync is what needs it** —
    /// `Board::apply` is the only way an update from the server becomes a change on screen,
    /// and `version()` is what tells the server what this tab already has. A projection is
    /// derived state and cannot answer either question.
    board: Board,
    projection: Projection,
    camera: Camera,
    clear: Rgba,
    /// The board's own colour and pattern, read once from the document.
    ///
    /// ⚠ The desktop app prefers a **global** pattern and grid colour from its library
    /// sidecar, per the user's own instruction that the grid apply to every board. A browser
    /// has no sidecar, so it draws this — which is exactly the fallback the desktop app uses
    /// when no global choice has been made, so the two agree by default.
    background: vellum_doc::Background,
    text: text::TextLayer,
    images: images::ImageLayer,
    shapes: shapes::ShapeLayer,
    strokes: strokes::StrokeLayer,
    /// Tables, charts, mind maps and kanban boards. Without it all four draw as one flat
    /// coloured box, which is what a browser showed where the desktop app showed a table.
    widgets: widgets::WidgetLayer,
    /// The ↗ on a link card and the ▶ on a video card. Stateless.
    badges: badges::BadgeLayer,
    /// The poll loop that keeps this board in step with the server, or `None` for a static
    /// `board.bin` with no server behind it. Driven by its own timer, not by the frame loop —
    /// `requestAnimationFrame` stops when a tab is hidden, and a board that silently stops
    /// keeping up while you look at something else is the report this exists to prevent.
    live: Option<live::Live>,
}

thread_local! {
    static VIEWER: RefCell<Option<Rc<RefCell<Viewer>>>> = const { RefCell::new(None) };
}

/// Entry point. Called by the page once the canvas exists.
///
/// `board_url` names a source of Loro snapshot bytes — `velmd`'s
/// `/api/v1/boards/{id}/snapshot`, or a static `board.bin` during development —
/// and a picture's URL is `blob_base` + its BLAKE3 hash + `blob_suffix`. All three are
/// given by the page rather than built here, so the same wasm serves a `velmd` origin and a
/// directory of static files with no build flag telling the two apart. The suffix exists
/// because a token rides in the query string, which has to land after the hash.
///
/// Errors land in the console *and* in the DOM, because a wasm panic that only reaches the
/// console is invisible to anyone holding an iPad.
#[wasm_bindgen]
pub fn start(canvas_id: String, board_url: String, blob_base: String, blob_suffix: String) {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(error) = boot(&canvas_id, &board_url, &blob_base, &blob_suffix).await {
            let message = error.to_string();
            log::error!("velm: {message}");
            report(&message);
        }
    });
}

fn report(message: &str) {
    if let Some(document) = web_sys::window().and_then(|w| w.document())
        && let Some(slot) = document.get_element_by_id("velm-status")
    {
        slot.set_text_content(Some(message));
    }
    // Also send it back to whatever served the page.
    //
    // A headless browser will not reliably hand back the DOM at the moment an async GPU
    // handshake finishes, so reading the verdict out of the page is a race. A request is
    // not: it either arrives in the server's log or it does not. This is the same reason
    // `--demo` fixtures print a verdict line rather than leaving the answer on screen.
    if let Some(window) = web_sys::window() {
        let url = format!("/velm-report?{}", js_sys::encode_uri_component(message));
        let _ = window.fetch_with_str(&url);
    }
}

/// Zoom about the middle of the canvas — the bar's `+`/`−` buttons and the `+`/`-` keys.
///
/// The anchor is the viewport's centre rather than a pointer, because a button press has no
/// pointer worth aiming at: the same split `PasteAim` draws natively between `⌘V` and an
/// import. `Camera::viewport` is already in physical pixels — trap 4 — so unlike `input.rs`
/// there is no device ratio to apply here; that module converts because a DOM event hands it
/// CSS pixels and this one has no event.
///
/// A no-op, never a throw, when there is no board yet or a frame holds the viewer: the page
/// disables its own controls until `camera_report` first answers, and a panic here would be
/// an unhandled rejection inside a DOM listener rather than a message anybody sees.
#[wasm_bindgen]
pub fn zoom_by(factor: f64) {
    VIEWER.with(|slot| {
        if let Some(viewer) = slot.borrow().as_ref()
            && let Ok(mut viewer) = viewer.try_borrow_mut()
        {
            let size = viewer.camera.viewport();
            let middle = vellum_scene::ScreenPoint::new(size.width / 2.0, size.height / 2.0);
            viewer.camera.zoom_by(factor, middle);
        }
    });
}

/// Fit the whole board on screen — the bar's middle button and the `0` key.
///
/// Literally the two lines `boot` runs, so "fit" means exactly the view the board opened at
/// rather than something close to it. A board with no content bounds is left where it is.
#[wasm_bindgen]
pub fn fit_board() {
    VIEWER.with(|slot| {
        if let Some(viewer) = slot.borrow().as_ref()
            && let Ok(mut viewer) = viewer.try_borrow_mut()
            && let Some(bounds) = viewer.projection.content_bounds()
        {
            viewer.camera.fit_to_rect(bounds, FIT_MARGIN);
        }
    });
}

/// The camera, as a string, for a fixture to read.
///
/// The one hop a test outside the wasm module cannot otherwise see. `input.rs` is driven by
/// DOM events, so the only honest way to check a gesture is to dispatch real events at the
/// real listeners and then ask what the camera did — which needs a reader, and this is it.
/// Trap 9's lesson in a second place: a fixture that calls the handler directly starts
/// downstream of everything that can go wrong between the browser and the handler.
#[wasm_bindgen]
pub fn camera_report() -> String {
    VIEWER.with(|slot| match slot.borrow().as_ref() {
        Some(viewer) => match viewer.try_borrow() {
            Ok(viewer) => {
                let centre = viewer.camera.center();
                format!("{:.6} {:.3} {:.3}", viewer.camera.zoom(), centre.x, centre.y)
            }
            Err(_) => "busy".to_owned(),
        },
        None => "none".to_owned(),
    })
}

/// Post a fixture's verdict back to whatever served the page.
///
/// Same channel as [`report`], and for the same reason: reading a verdict out of the DOM of
/// a headless browser is a race, and a request either arrives in the server's log or it does
/// not. A fixture running on the user's own iPad reports through this too.
#[wasm_bindgen]
pub fn verdict(line: &str) {
    log::info!("velm fixture: {line}");
    report(line);
}

async fn boot(
    canvas_id: &str,
    board_url: &str,
    blob_base: &str,
    blob_suffix: &str,
) -> Result<(), String> {
    // Reported in the status line beside the item count. Cold start is the headline metric
    // this port is worst at — `docs/01` budgets 300ms for the desktop app and a tab will not
    // meet it — so it is measured on the device rather than estimated on this one.
    let began = web_time::Instant::now();
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let canvas: web_sys::HtmlCanvasElement = document
        .get_element_by_id(canvas_id)
        .ok_or_else(|| format!("no element with id {canvas_id:?}"))?
        .dyn_into()
        .map_err(|_| "that element is not a <canvas>".to_owned())?;

    report("Asking for a GPU…");

    // `Backends::all()` includes BROWSER_WEBGPU on wasm. No WebGL fallback is requested and
    // none would work: `vellum-render` binds a storage buffer to the vertex stage, which
    // WebGL2 does not allow at all. The probe page measures that limit before any of this.
    let instance = wgpu::Instance::default();
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(|e| format!("cannot draw on that canvas: {e}"))?;

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            ..Default::default()
        })
        .await
        .map_err(|_| {
            "This browser has WebGPU but would not give the page a GPU. On an iPad that \
             usually means iPadOS 25 or older — Safari only gained WebGPU in 26."
                .to_owned()
        })?;

    // Exactly what `vellum-app/src/surface.rs` asks for: the WebGPU baseline, and no
    // features at all. Staying here is the whole reason the renderer needed no changes.
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("velm"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        })
        .await
        .map_err(|e| format!("this GPU cannot meet the WebGPU baseline: {e}"))?;

    report("Fetching the board…");
    let bytes = fetch(board_url).await?;
    let board = Board::from_bytes(&bytes).map_err(|e| format!("that is not a Velm board: {e}"))?;

    let mut projection = Projection::new();
    projection
        .rebuild(&board)
        .map_err(|e| format!("cannot lay the board out: {e}"))?;

    let (width, height) = size_of(&canvas, &window);
    let mut camera = Camera::new(ScreenSize::new(width as f64, height as f64));
    // A camera from the URL, for comparing this against the desktop app at a matched view.
    // `?zoom=1&cx=…&cy=…` is the browser's `--zoom` and it exists for the same reason: two
    // screenshots of the same board at different cameras cannot be compared, and eyeballing
    // "about the same place" is how an hour goes into a difference that was never there.
    let override_camera = camera_from_url(&window);
    if let Some(bounds) = projection.content_bounds() {
        // ⚠ **A fraction of the rect, not a margin in pixels.** This shipped as `40.0`,
        // meaning eighty-one times the board's own size, which drove the fit below
        // `MIN_ZOOM` and clamped — so every board opened at exactly 1.0% with its content
        // in a small clump in the middle, and the round number is the tell. Native's
        // `FIT_MARGIN` is 0.02 and this matches it, so a board opens the same way in both.
        camera.fit_to_rect(bounds, FIT_MARGIN);
    }
    if let Some((zoom, centre)) = override_camera {
        let middle = vellum_scene::ScreenPoint::new(width as f64 / 2.0, height as f64 / 2.0);
        camera.set_zoom_about(zoom, middle);
        if let Some((x, y)) = centre {
            camera.set_center(vellum_scene::WorldPoint::new(x, y));
        }
    }

    let format = surface
        .get_capabilities(&adapter)
        .formats
        .first()
        .copied()
        .unwrap_or(wgpu::TextureFormat::Bgra8Unorm);
    let mut config = surface
        .get_default_config(&adapter, width, height)
        .ok_or("this GPU cannot present to that canvas")?;
    config.format = format;
    config.present_mode = wgpu::PresentMode::AutoVsync;
    surface.configure(&device, &config);

    let renderer = Renderer::new(&device, format);
    let msaa = make_msaa(&device, format, width, height);
    let background = board.background();
    let clear = board::clear_colour(&background, vellum_project::theme::Theme::LIGHT.canvas);

    let items = projection.len();
    // Derived from the URL the snapshot came back through, rather than from a new parameter:
    // the page built that string with its own encoding and its own token, and it is known to
    // work because the board on screen arrived through it. `None` for `./board.bin`, which is
    // a static file with nothing to sync with — that falls out of the parse, not a flag.
    let live = live::from_snapshot_url(board_url, live::DEFAULT_PERIOD_MS);
    if live.is_none() {
        log::info!("velm sync: this board came from a static file, so there is nothing to poll");
    }
    let viewer = Rc::new(RefCell::new(Viewer {
        device,
        queue,
        surface,
        config,
        renderer,
        msaa,
        board,
        projection,
        camera,
        clear,
        background,
        text: text::TextLayer::new()?,
        images: images::ImageLayer::new(blob_base, blob_suffix),
        shapes: shapes::ShapeLayer::new(),
        strokes: strokes::StrokeLayer::new(),
        widgets: widgets::WidgetLayer::new(),
        badges: badges::BadgeLayer::new(),
        live,
    }));

    // Prove the board actually drew, rather than trusting that it did.
    //
    // A screenshot cannot answer this: a GPU canvas captures black in a headless browser, and
    // a locked machine photographs its lock screen. So the client renders one frame into an
    // offscreen texture, reads the pixels back, and counts how many differ from the clear
    // colour. That is an assertion rather than a photograph, it runs on the device the user
    // is actually holding, and it is the same discipline `--demo` fixtures use natively:
    // report a measured number with a verdict, never an intention.
    let pending = viewer.borrow_mut().begin_self_check();
    let drawn = match pending {
        Some(readback) => readback.count().await,
        None => None,
    };

    input::attach(&canvas, Rc::clone(&viewer));
    VIEWER.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&viewer)));

    // The status line names the camera as well as the count, and that is not decoration:
    // every screenshot taken of this page afterwards is self-describing. A board that draws
    // in one small clump is either a bad fit or a wide-but-empty extent, and those two look
    // identical in a photograph and are told apart by one number.
    let view = {
        let viewer = viewer.borrow();
        let bounds = viewer.projection.content_bounds();
        match bounds {
            Some(rect) => format!(
                " · {:.1}% · board {:.0}x{:.0} at ({:.0}, {:.0})",
                viewer.camera.zoom() * 100.0,
                rect.width(),
                rect.height(),
                viewer.camera.center().x,
                viewer.camera.center().y
            ),
            None => String::new(),
        }
    };
    let ms = began.elapsed().as_millis();
    report(&match drawn {
        Some(painted) if painted > 0 => {
            format!("{items} items · {painted} pixels painted{view} · {ms}ms")
        }
        Some(_) => {
            format!("{items} items · NOTHING PAINTED — the board is not drawing{view} · {ms}ms")
        }
        None => format!("{items} items · could not read the frame back{view} · {ms}ms"),
    });
    schedule_frame();
    // ⚠ **After `VIEWER` is installed above** — `live::tick` reads it, and a timer armed
    // before it would spend its first ticks finding nothing. This one line is the entire
    // reachability of `crate::live`: without it the file compiles, its logic is right, and
    // no board ever asks the server anything. That is this repository's signature defect,
    // found nine times by its own count, so the call is commented rather than merely present.
    live::drive();
    Ok(())
}

/// The multisampled attachment the board pass draws into.
///
/// `RENDER_ATTACHMENT` only, and the pass stores `Discard`: on a tile-based GPU -- every
/// Apple one, which is what an iPad is -- the samples then never reach memory at all, so 4x
/// MSAA costs bandwidth it never spends. That reasoning is `vellum-render`'s, not new here.
fn make_msaa(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("velm-msaa"),
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: vellum_render::BOARD_SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

/// `?zoom=`, `?cx=`, `?cy=` — a camera, for comparing against the desktop app.
fn camera_from_url(window: &web_sys::Window) -> Option<(f64, Option<(f64, f64)>)> {
    let search = window.location().search().ok()?;
    let get = |name: &str| -> Option<f64> {
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.parse::<f64>().ok())
    };
    let zoom = get("zoom=")?;
    let centre = match (get("cx="), get("cy=")) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    };
    Some((zoom, centre))
}

/// The canvas in physical pixels.
///
/// Screen coordinates are physical pixels everywhere in Velm — trap 4 in `CLAUDE.md` — and
/// mixing them with CSS pixels makes zoom-at-cursor drift by exactly the device ratio.
fn size_of(canvas: &web_sys::HtmlCanvasElement, window: &web_sys::Window) -> (u32, u32) {
    let ratio = window.device_pixel_ratio().max(1.0);
    let rect = canvas.get_bounding_client_rect();
    let width = ((rect.width() * ratio).round() as u32).max(1);
    let height = ((rect.height() * ratio).round() as u32).max(1);
    canvas.set_width(width);
    canvas.set_height(height);
    (width, height)
}

async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|_| format!("could not reach {url}"))?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| "bad response")?;
    if !response.ok() {
        return Err(format!("{url} answered {}", response.status()));
    }
    let buffer = wasm_bindgen_futures::JsFuture::from(
        response.array_buffer().map_err(|_| "no body")?,
    )
    .await
    .map_err(|_| "could not read the body")?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

fn schedule_frame() {
    let closure = Closure::<dyn FnMut()>::once(move || {
        VIEWER.with(|slot| {
            if let Some(viewer) = slot.borrow().as_ref() {
                viewer.borrow_mut().frame();
            }
        });
        schedule_frame();
    });
    if let Some(window) = web_sys::window() {
        let _ = window.request_animation_frame(closure.as_ref().unchecked_ref());
    }
    // Handed to the browser, which calls it once and releases it. Dropping it here would
    // cancel the frame instead.
    closure.forget();
}

/// A frame the GPU has been asked to hand back, waiting to be counted.
struct Readback {
    buffer: wgpu::Buffer,
    done: futures_channel::oneshot::Receiver<Result<(), wgpu::BufferAsyncError>>,
}

impl Readback {
    /// How many pixels are not the background.
    ///
    /// Returns `None` if the read-back could not complete, which is a different answer from
    /// zero and must not be reported as a failure: some browsers and power modes decline to
    /// map a buffer without a live frame loop, and calling that "the board is broken" would
    /// be the probe page's mistake repeated.
    async fn count(self) -> Option<u32> {
        // On the web the device is ticked by the browser, so there is nothing to poll; the
        // callback arrives when the queue drains.
        match self.done.await {
            Ok(Ok(())) => {}
            _ => return None,
        }
        let Ok(data) = self.buffer.slice(..).get_mapped_range() else { return None };
        // Cleared to black, so anything not black is something the renderer drew.
        let painted = data.chunks_exact(4).filter(|p| p[0] > 4 || p[1] > 4 || p[2] > 4).count();
        drop(data);
        self.buffer.unmap();
        Some(painted as u32)
    }
}

impl Viewer {
    /// Render one frame offscreen and ask the GPU to hand the pixels back.
    ///
    /// ⚠ **Split from the counting on purpose, and the seam is where the `await` is.** The
    /// caller holds the `Viewer` in a `RefCell`, and awaiting while that borrow is live is a
    /// panic waiting for a second borrower — today there is none, because this runs before
    /// the listeners are attached and before the frame loop starts, which is exactly the
    /// kind of "safe because of what happens to be true elsewhere" this file should not
    /// rely on. So the borrow ends when this returns, and [`Readback::count`] awaits with
    /// nothing borrowed at all.
    fn begin_self_check(&mut self) -> Option<Readback> {
        const SIDE: u32 = 256;
        // ⚠ The renderer's pipelines are built once, for the surface's format. A readback
        // target in any other format is rejected -- WebGPU matches the whole attachment
        // state, not just the sample count, and the board silently draws nothing. Found
        // exactly that way: the sample count was fixed first and the format failed next.
        let format = self.config.format;
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("velm-self-check"),
            size: wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let msaa = make_msaa(&self.device, format, SIDE, SIDE);

        // The same camera the canvas uses, squared to the readback target so the fitted
        // board lands inside it.
        let mut camera = self.camera;
        camera.set_viewport(ScreenSize::new(SIDE as f64, SIDE as f64));
        if let Some(bounds) = self.projection.content_bounds() {
            // The same fraction the canvas fits with. It read `8.0` here, meaning seventeen
            // times the board — enough to drive the fit past `MIN_ZOOM` and clamp, so this
            // probe was answering about a view nobody would ever see.
            camera.fit_to_rect(bounds, FIT_MARGIN);
        }

        let mut list = DrawList::new();
        list.view(View::board(&camera));
        let mut visible: Vec<&SceneItem> = Vec::new();
        self.projection.scene().collect_visible(&camera, &mut visible);
        visible.sort_by_key(|item| item.z);
        for item in visible {
            list.push_scene_item(item, &camera);
        }

        self.renderer.begin_frame();
        self.renderer.prepare(&self.device, &self.queue, &list);

        let row = SIDE * 4;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("velm-self-check-readback"),
            size: (row * SIDE) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("velm-self-check"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &msaa,
                    resolve_target: Some(&view),
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                ..Default::default()
            });
            self.renderer.draw(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(SIDE),
                },
            },
            wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));

        let (send, recv) = futures_channel::oneshot::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = send.send(result);
            });
        Some(Readback { buffer: readback, done: recv })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || (width == self.config.width && height == self.config.height)
        {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        // Rebuilt with the surface. An MSAA attachment left at the old size makes every
        // frame after a resize a validation error, and the board silently stops drawing.
        self.msaa = make_msaa(&self.device, self.config.format, width, height);
        self.camera
            .set_viewport(ScreenSize::new(width as f64, height as f64));
    }

    fn frame(&mut self) {
        // The same arms `vellum-app/src/surface.rs` handles, and for the same reasons. The
        // one that matters most on the web is `Occluded`: a backgrounded tab is never
        // presented to, so a frame counter alone cannot tell a working composite from a
        // blank one.
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => texture,
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                // Usable this frame; the swapchain no longer matches the canvas.
                self.surface.configure(&self.device, &self.config);
                texture
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Upload whatever finished decoding since the last frame, before the list is built,
        // so a picture that arrived is drawn this frame rather than the next one.
        self.images
            .drain(&self.device, &self.queue, self.renderer.textures_mut());
        // Every distinct zoom mints a whole new set of glyph bitmaps and the engine's cache
        // has no eviction of its own, so a pinch would otherwise leave one set per frame
        // resident for the life of the tab.
        self.text.note_scale(self.camera.zoom() as f32);

        let mut list = DrawList::new();
        // Both views are registered up front and flipped between, exactly as `draw.rs` does.
        // Quads live in camera-relative world pixels; **glyphs live in physical screen
        // pixels**, because they are rasterised at a physical size and positioning them in
        // world units would resample every one. CLAUDE.md names this flip as the reason text
        // ends the quad batch once per item.
        let board = list.view(View::board(&self.camera));
        let screen = list.view(View::screen(self.camera.viewport()));
        // The board's own surface, before anything on it. In the screen view, because a dot
        // sized in world units is a smear at a fitted 4% and a disc at 8x.
        list.use_view(screen);
        board::push_grid(
            &mut list,
            &self.camera,
            &self.background,
            vellum_project::theme::Theme::LIGHT.grid,
        );
        list.use_view(board);

        // Only what the camera can see. This is the project's whole thesis -- frame cost
        // scales with what is on screen, not with what exists -- and it is the R-tree that
        // makes it true, on this target exactly as on the desktop.
        let mut visible: Vec<&SceneItem> = Vec::new();
        self.projection
            .scene()
            .collect_visible(&self.camera, &mut visible);
        // Painter's order. Frames take a negative z band so they draw behind everything,
        // which is decided once in `vellum_project::project` rather than here.
        visible.sort_by_key(|item| item.z);
        let zoom = self.camera.zoom() as f32;
        let theme = &vellum_project::theme::Theme::LIGHT;
        let stroke_colour = theme.stroke;
        let mut on_screen = Vec::with_capacity(visible.len());

        // **One loop, dispatching per kind.** The order these are pushed in *is* the paint
        // order, so a second pass over the same items draws above every quad regardless of
        // z — which is why the pictures and the strokes cannot be their own loops. `draw.rs`
        // is one loop for exactly this reason.
        for item in &visible {
            // ⚠ **After the push, never before.** `on_screen` is what the four caches are
            // pruned against, so guarding above it would drop a clipped item's shaped text
            // and tessellated ink — and re-derive both the moment a frame drag momentarily
            // separated from a child. `draw.rs` prunes on projection membership and never on
            // clipping, and this keeps that property identical on both front ends.
            on_screen.push(item.id);
            let Some(projected) = self.projection.get(item.id) else {
                list.push_scene_item(item, &self.camera);
                continue;
            };
            // An item whose frame no longer contains it is not drawn. Miro's rule, and the
            // desktop app's since feedback 39 — without it the two applications draw
            // different boards, which is worse than either drawing less.
            if vellum_project::frame::clipped_by_frame(projected, &self.projection) {
                continue;
            }
            match &projected.item.kind {
                // ⚠ **Triangles, and deliberately no quad behind them.**
                // `push_scene_item` draws one solid box per item in the item's dominant
                // colour — the scene layer's honest fallback for a kind the caller has not
                // taught the renderer about. For a sticky that is nearly the drawing; for a
                // pen stroke it is a filled rectangle the size of the stroke's bounding box,
                // which is what the browser was painting where the desktop paints a line.
                vellum_doc::ItemKind::Ink { .. } => {
                    self.strokes.push_ink(
                        &mut list,
                        &self.camera,
                        item.id,
                        &self.projection,
                        stroke_colour,
                    );
                }
                // ⚠ **And no quad behind it**, for the reason the ink arm above records: a
                // shape's `push_scene_item` payload is a solid box over its whole bounding
                // rectangle, and the silhouette lands in a later batch — so an ellipse, a
                // diamond or a star would show a filled rectangle through every corner it
                // does not fill. For a shape the box is the negation of the drawing.
                vellum_doc::ItemKind::Shape { .. } => {
                    self.shapes.push(&mut list, &self.camera, item.id, &self.projection, theme);
                }
                vellum_doc::ItemKind::Connector { .. } => {
                    self.strokes.push_connector(
                        &mut list,
                        &self.camera,
                        item.id,
                        &self.projection,
                        stroke_colour,
                    );
                }
                // ⚠ **And no quad behind these either.** The four structured widgets are the
                // last kinds that were falling through to `push_scene_item`, which drew each
                // of them as one flat coloured rectangle — a table with no grid, a chart with
                // no bars, a kanban with no columns. Their own geometry replaces the box
                // rather than sitting on it, for the ink arm's reason: the box is drawn in a
                // batch that lands underneath, so it would show through every gap the drawing
                // deliberately leaves.
                vellum_doc::ItemKind::Table { .. }
                | vellum_doc::ItemKind::Chart { .. }
                | vellum_doc::ItemKind::MindMap { .. }
                | vellum_doc::ItemKind::Kanban { .. } => {
                    self.widgets.push(
                        &mut list,
                        &self.camera,
                        item.id,
                        &self.projection,
                        theme,
                        self.text.engine_mut(),
                    );
                }
                _ => {
                    list.push_scene_item(item, &self.camera);
                    // A picture goes in the box `layout` gives it, which for a link card is a
                    // band at the top rather than the whole card. Drawing it over the card was
                    // both the stretch and most of the blur: a landscape photo squeezed into a
                    // portrait rectangle is smeared on one axis by however far the two aspects
                    // are apart.
                    if let Some(slot) = layout::picture(projected)
                        && let Some((texture, source)) = self.images.texture(&slot.hash)
                    {
                        let origin = self.camera.to_camera_relative(slot.rect.min);
                        // ⚠ **World units, not screen pixels.** The board view already
                        // carries the zoom in its clip transform, so multiplying here applies
                        // it twice: at a fitted 6% every picture was drawn at 6% of its own
                        // box, which is a handful of pixels and reads as "the images do not
                        // load". It is also why they were *blurry* rather than merely small —
                        // `Renderer::observe_detail` derives the demanded texels from
                        // `size / units_per_pixel`, and the board view's `units_per_pixel` is
                        // `1/zoom`, so a size already multiplied by the zoom asks for zoom²
                        // times too few texels and `resolve_detail` dutifully demotes the
                        // texture to its floor. One wrong multiplication, both symptoms.
                        let size = [slot.rect.width() as f32, slot.rect.height() as f32];
                        let uv = if slot.cover {
                            layout::cover_uv(source, (slot.rect.width(), slot.rect.height()))
                        } else {
                            vellum_render::UvRect::FULL
                        };
                        // Marked so the renderer's own residency knows it is on screen. The
                        // budget's eviction guard is `last_marked < frame`, so an unmarked
                        // texture is indistinguishable from one nobody has looked at in
                        // minutes.
                        self.renderer.textures_mut().mark(texture, 0.0);
                        list.push_image(
                            texture,
                            vellum_render::ImageInstance::new(origin, size, uv),
                        );
                    }
                    // The card's ↗ and its ▶, after the picture so they sit on top of the
                    // poster they overlap. Answers `false` for anything that is not a card
                    // with an address a browser can open — which is not a cue to draw the
                    // item some other way, it is the honest drawing of a card with nowhere
                    // to go.
                    self.badges.push(
                        &mut list,
                        &self.camera,
                        item.id,
                        &self.projection,
                        theme,
                    );
                }
            }
        }

        // Shape and queue every visible item's words.
        //
        // Queued here and flushed once, after the loop, rather than drawn in place. That
        // puts every block in front of every box — including a box that is in front of it on
        // the board. It is the one deliberate departure from `draw.rs`'s ordering, and it is
        // the cheap half of a trade: glyphs live in the **screen** view while quads live in
        // the board view, so drawing text in place ends the quad batch twice per item.
        let text_colour = vellum_project::theme::Theme::LIGHT.text;
        let muted = vellum_project::theme::Theme::LIGHT.text_muted;
        for item in &visible {
            let Some(projected) = self.projection.get(item.id) else { continue };
            // The same guard the geometry loop applies, and it has to be here too: without
            // it a clipped item's words draw with no box under them, which is feedback 35's
            // sibling rule exactly. `draw.rs` checks in both of its passes for this reason.
            if vellum_project::frame::clipped_by_frame(projected, &self.projection) {
                continue;
            }
            // ⚠ **A widget's labels are many and are not `kind.text()`.** A table's cells, a
            // kanban's cards and a mind map's nodes live inside the item's own token, so the
            // `text()` accessor answers `None` for all four and their words would simply
            // never be drawn. Queued first, and with a real slot per label: the layout cache
            // keys on `(item, slot, …)`, and without the slot two cells with the same
            // geometry resolve to one entry and the second draws the first one's words.
            for label in self.widgets.text_slots(item.id, &self.projection, theme) {
                let top_left = self.camera.world_to_screen(label.rect.min);
                self.text.queue(
                    item.id,
                    label.slot,
                    projected.generation,
                    &label.text,
                    [top_left.x as f32, top_left.y as f32],
                    [label.rect.width() as f32, label.rect.height() as f32],
                    Some(label.font_size),
                    zoom,
                    label.color,
                    label.anchor,
                );
            }
            let Some(styled) = projected.item.kind.text() else { continue };
            // ⚠ **Not the item's own rectangle.** A sticky's words are inset by Miro's own
            // 8% and centred; a frame's name is small and sits *above* the frame; a card's
            // words start under its picture band. Using the box for all of them is what put a
            // sticky's text against its edges and a frame's name enormous across its middle.
            let Some(slot) = layout::text_slot(projected, text_colour, muted) else { continue };
            let top_left = self.camera.world_to_screen(slot.rect.min);
            // ⚠ The box is in **world** units, not screen pixels, and this is the whole bug
            // the first version had. Shaping with a world-unit font size against a
            // screen-pixel wrap width means the wrap width moves with the zoom while the
            // font size does not, so the text re-wraps at every zoom level and the block
            // visibly changes size and position as you scroll. Shape once in world space;
            // `push_layout`'s `scale` then magnifies the finished layout uniformly, which is
            // how `vellum-app` has always done it.
            let size = [slot.rect.width() as f32, slot.rect.height() as f32];
            // Real runs, not one flattened string. `vellum_project::runs::convert` is the
            // same conversion `vellum-app`'s painter makes, in the crate both front ends
            // share — so a bold span is bold in a browser for the same reason it is on the
            // Mac, and trap 10's guard (cosmic-text does not fall back to a family's regular
            // face, so a missing weight silently changes *typeface*) is applied once.
            let converted = vellum_project::runs::convert(styled);
            self.text.queue(
                item.id,
                // Slot 0: everything on this path has exactly one block. The structured
                // widgets are the only items with several, and they queue their own below.
                0,
                projected.generation,
                &converted,
                [top_left.x as f32, top_left.y as f32],
                size,
                slot.font_size,
                zoom,
                slot.color,
                slot.anchor,
            );
        }
        list.use_view(screen);
        self.text.flush(&self.device, &self.queue, self.renderer.atlas_mut(), &mut list);
        // Separate from the flush above, and it must stay separate: a fitted board is
        // entirely greeked, so folding this into a function that returns early when nothing
        // was shaped makes the one case it exists for the one case it never runs in.
        self.text.flush_greeked(&mut list);
        self.text.retain_visible(&on_screen);
        self.strokes.retain_visible(&on_screen);
        self.shapes.retain_visible(&on_screen);
        self.widgets.retain_visible(&on_screen);
        // ⚠ **After** the list is built, never before. Eviction spares what was marked this
        // frame, and the marks happen while the list is built — so running it first makes
        // that guard vacuously true and lets it take a texture the list already references.
        // `vellum-app` moved this call for exactly that reason.
        self.images.enforce_budget(self.renderer.textures_mut());

        self.renderer.begin_frame();
        self.renderer.prepare(&self.device, &self.queue, &list);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("velm") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("board"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa,
                    resolve_target: Some(&view),
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(vellum_project::theme::clear_color(self.clear)),
                        // Discard, not Store: the resolve is what is kept, and on a tile-based
                        // GPU the samples never leave tile memory.
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                ..Default::default()
            });
            self.renderer.draw(&mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
    }
}
