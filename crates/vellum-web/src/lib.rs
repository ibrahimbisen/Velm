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

mod images;
mod input;
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
    projection: Projection,
    camera: Camera,
    clear: Rgba,
    text: text::TextLayer,
    images: images::ImageLayer,
    strokes: strokes::StrokeLayer,
}

thread_local! {
    static VIEWER: RefCell<Option<Rc<RefCell<Viewer>>>> = const { RefCell::new(None) };
}

/// Entry point. Called by the page once the canvas exists.
///
/// `board_url` names a file of Loro snapshot bytes — `velmd`'s `/snapshot` route, or a
/// static file during development. Errors land in the console *and* in the DOM, because a
/// wasm panic that only reaches the console is invisible to anyone holding an iPad.
#[wasm_bindgen]
pub fn start(canvas_id: String, board_url: String) {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(error) = boot(&canvas_id, &board_url).await {
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

async fn boot(canvas_id: &str, board_url: &str) -> Result<(), String> {
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
    if let Some(bounds) = projection.content_bounds() {
        // ⚠ **A fraction of the rect, not a margin in pixels.** This shipped as `40.0`,
        // meaning eighty-one times the board's own size, which drove the fit below
        // `MIN_ZOOM` and clamped — so every board opened at exactly 1.0% with its content
        // in a small clump in the middle, and the round number is the tell. Native's
        // `FIT_MARGIN` is 0.02 and this matches it, so a board opens the same way in both.
        camera.fit_to_rect(bounds, FIT_MARGIN);
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
    let clear = vellum_project::theme::Theme::LIGHT.canvas;

    let items = projection.len();
    let viewer = Rc::new(RefCell::new(Viewer {
        device,
        queue,
        surface,
        config,
        renderer,
        msaa,
        projection,
        camera,
        clear,
        text: text::TextLayer::new()?,
        images: images::ImageLayer::new("./blobs/"),
        strokes: strokes::StrokeLayer::new(),
    }));

    // Prove the board actually drew, rather than trusting that it did.
    //
    // A screenshot cannot answer this: a GPU canvas captures black in a headless browser, and
    // a locked machine photographs its lock screen. So the client renders one frame into an
    // offscreen texture, reads the pixels back, and counts how many differ from the clear
    // colour. That is an assertion rather than a photograph, it runs on the device the user
    // is actually holding, and it is the same discipline `--demo` fixtures use natively:
    // report a measured number with a verdict, never an intention.
    let drawn = viewer.borrow_mut().self_check().await;

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
                " · {:.1}% · board {:.0}x{:.0}",
                viewer.camera.zoom() * 100.0,
                rect.width(),
                rect.height()
            ),
            None => String::new(),
        }
    };
    report(&match drawn {
        Some(painted) if painted > 0 => {
            format!("{items} items · {painted} pixels painted{view}")
        }
        Some(_) => format!("{items} items · NOTHING PAINTED — the board is not drawing{view}"),
        None => format!("{items} items · could not read the frame back{view}"),
    });
    schedule_frame();
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

impl Viewer {
    /// Render one frame offscreen and count the pixels that are not the background.
    ///
    /// Returns `None` if the read-back could not complete, which is a different answer from
    /// zero and must not be reported as a failure: some browsers and power modes decline to
    /// map a buffer without a live frame loop, and calling that "the board is broken" would
    /// be the probe page's mistake repeated.
    async fn self_check(&mut self) -> Option<u32> {
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
            camera.fit_to_rect(bounds, 8.0);
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
        // On the web the device is ticked by the browser, so there is nothing to poll; the
        // callback arrives when the queue drains.
        match recv.await {
            Ok(Ok(())) => {}
            _ => return None,
        }
        let Ok(data) = readback.slice(..).get_mapped_range() else { return None };
        // Cleared to black, so anything not black is something the renderer drew.
        let painted = data.chunks_exact(4).filter(|p| p[0] > 4 || p[1] > 4 || p[2] > 4).count();
        drop(data);
        readback.unmap();
        Some(painted as u32)
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

        let mut list = DrawList::new();
        // Both views are registered up front and flipped between, exactly as `draw.rs` does.
        // Quads live in camera-relative world pixels; **glyphs live in physical screen
        // pixels**, because they are rasterised at a physical size and positioning them in
        // world units would resample every one. CLAUDE.md names this flip as the reason text
        // ends the quad batch once per item.
        let board = list.view(View::board(&self.camera));
        let screen = list.view(View::screen(self.camera.viewport()));
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
        let stroke_colour = vellum_project::theme::Theme::LIGHT.stroke;
        let mut on_screen = Vec::with_capacity(visible.len());

        // **One loop, dispatching per kind.** The order these are pushed in *is* the paint
        // order, so a second pass over the same items draws above every quad regardless of
        // z — which is why the pictures and the strokes cannot be their own loops. `draw.rs`
        // is one loop for exactly this reason.
        for item in &visible {
            on_screen.push(item.id);
            let Some(projected) = self.projection.get(item.id) else {
                list.push_scene_item(item, &self.camera);
                continue;
            };
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
                vellum_doc::ItemKind::Connector { .. } => {
                    self.strokes.push_connector(
                        &mut list,
                        &self.camera,
                        item.id,
                        &self.projection,
                        stroke_colour,
                    );
                }
                _ => {
                    list.push_scene_item(item, &self.camera);
                    let hash = match &projected.item.kind {
                        vellum_doc::ItemKind::Image { asset_id, .. } => Some(asset_id.as_str()),
                        vellum_doc::ItemKind::LinkPreview { thumbnail: Some(h), .. } => {
                            Some(h.as_str())
                        }
                        _ => None,
                    };
                    if let Some(hash) = hash
                        && let Some(texture) = self.images.texture(hash)
                    {
                        let origin = self.camera.to_camera_relative(projected.bounds.min);
                        let size = [
                            (projected.bounds.width() as f32) * zoom,
                            (projected.bounds.height() as f32) * zoom,
                        ];
                        list.push_image(
                            texture,
                            vellum_render::ImageInstance::new(
                                origin,
                                size,
                                vellum_render::UvRect::FULL,
                            ),
                        );
                    }
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
        for item in &visible {
            let Some(projected) = self.projection.get(item.id) else { continue };
            let Some(styled) = projected.item.kind.text() else { continue };
            let top_left = self.camera.world_to_screen(projected.bounds.min);
            // ⚠ The box is in **world** units, not screen pixels, and this is the whole bug
            // the first version had. Shaping with a world-unit font size against a
            // screen-pixel wrap width means the wrap width moves with the zoom while the
            // font size does not, so the text re-wraps at every zoom level and the block
            // visibly changes size and position as you scroll. Shape once in world space;
            // `push_layout`'s `scale` then magnifies the finished layout uniformly, which is
            // how `vellum-app` has always done it.
            let size = [
                projected.bounds.width() as f32,
                projected.bounds.height() as f32,
            ];
            let font_size = projected
                .item
                .style
                .font_size
                .map(|s| s as f32)
                .unwrap_or(vellum_text::DEFAULT_FONT_SIZE);
            let colour = vellum_project::theme::Theme::LIGHT.text;
            // ⚠ `vellum_doc::StyledText` and `vellum_text::StyledText` are different types:
            // the document's spans carry Miro's rich-text model, the engine's carry what
            // cosmic-text needs. Flattening to plain here is a **known loss** -- bold, links
            // and per-run colour do not survive it -- and it is the honest first step rather
            // than a finished one. `draw.rs` does the real conversion per item kind; that
            // work belongs with the painter.
            let flattened = vellum_text::StyledText::plain(
                styled.spans().iter().map(|s| s.text.as_str()).collect::<String>(),
            );
            self.text.queue(
                item.id,
                projected.generation,
                &flattened,
                [top_left.x as f32, top_left.y as f32],
                size,
                font_size,
                zoom,
                colour,
            );
        }
        list.use_view(screen);
        self.text.flush(&self.device, &self.queue, self.renderer.atlas_mut(), &mut list);
        self.text.retain_visible(&on_screen);
        self.strokes.retain_visible(&on_screen);

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
