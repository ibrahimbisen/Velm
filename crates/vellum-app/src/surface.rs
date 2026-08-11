//! The window's GPU surface, and the one render pass a frame draws into.
//!
//! Everything that turns geometry into pixels lives in `vellum-render`, which takes
//! a `wgpu::Device` and a target format and knows nothing about windows — that is
//! what lets the same code drive a PNG export, a board thumbnail and the pixel
//! tests. What is left here is the part that genuinely belongs to a window:
//! adapter selection, swapchain configuration, and recovering from the half-dozen
//! ways a surface can become unusable while the app is running.

use std::sync::Arc;

use anyhow::{Context, Result};
use vellum_render::{Backdrop, DrawList, GlassPanel, GlassRenderer, GlassStats, Renderer};
use winit::window::Window;

use crate::chrome_pass::ChromePass;
use crate::compose::Compositor;

/// Everything one frame composites, in the order it reaches the screen.
///
/// A struct rather than six arguments because the *order* is the interesting part and
/// a positional list hides it: the board goes into a texture, the blur reads that
/// texture, and only then does anything reach the swapchain. See
/// [`crate::compose`] for why the frame has that shape at all.
pub struct Frame<'a> {
    /// The board, already culled and assembled by `crate::draw`.
    pub board: &'a DrawList,
    pub clear: wgpu::Color,
    pub glass: &'a mut GlassRenderer,
    /// Where the floating chrome is, in physical pixels, as `vellum-ui` reported it.
    pub panels: &'a [GlassPanel],
    /// Bumped whenever the board's pixels change. A stale value shows a stale blur;
    /// a value that changes every frame costs the blur its cache. See
    /// `vellum_render::glass`.
    pub revision: u64,
    /// egui's triangles, already prepared. Drawn last, over the glass.
    pub chrome: &'a ChromePass,
}

pub struct Surface {
    /// Held because the surface borrows the window for as long as it exists, and
    /// because presenting wants `pre_present_notify`.
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    /// The offscreen the board is drawn into, so the glass has something to blur.
    compositor: Compositor,
    adapter_info: wgpu::AdapterInfo,
    /// What the last frame's glass cost. Reported by the HUD.
    glass_stats: GlassStats,
}

impl Surface {
    /// Brings up the GPU for `window`.
    ///
    /// `vsync` off is for benchmarking only: it lets the frame rate report the
    /// renderer's actual headroom instead of the display's refresh rate.
    pub fn new(window: Arc<Window>, vsync: bool) -> Result<Self> {
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
        );

        let surface = instance
            .create_surface(window.clone())
            .context("could not create a presentation surface for the window")?;

        // `force_fallback_adapter: false` plus HighPerformance asks for the real
        // GPU; on a laptop with switchable graphics that is the discrete one.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no GPU adapter can present to this window")?;

        let adapter_info = adapter.get_info();
        log::info!(
            "GPU: {} ({:?}, {:?}) via {:?}",
            adapter_info.name,
            adapter_info.device_type,
            adapter_info.driver,
            adapter_info.backend
        );
        if adapter_info.device_type == wgpu::DeviceType::Cpu {
            // Not fatal — it still renders — but it invalidates every performance
            // number, so it must not pass unnoticed.
            log::warn!(
                "selected adapter is a software rasteriser; performance figures are meaningless"
            );
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vellum-device"),
            // The WebGPU baseline. Everything the renderer needs fits inside it, and
            // staying at the baseline is what keeps the eventual wasm viewer of
            // `docs/01-architecture.md` §1 a recompile rather than a port.
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .context("could not create a GPU device")?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("the adapter reports no usable surface configuration")?;
        config.format = preferred_format(&surface.get_capabilities(&adapter));
        config.present_mode = if vsync {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
        surface.configure(&device, &config);
        log::info!(
            "surface: {:?} {}x{}, {:?}",
            config.format,
            config.width,
            config.height,
            config.present_mode
        );

        let renderer = Renderer::new(&device, config.format);
        let compositor = Compositor::new(&device, config.format, config.width, config.height);

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            renderer,
            compositor,
            adapter_info,
            glass_stats: GlassStats::default(),
        })
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The device and queue together, for a caller that has to upload before the
    /// frame — the chrome does, because uploads may not happen inside a render pass.
    pub fn device_queue(&self) -> (&wgpu::Device, &wgpu::Queue) {
        (&self.device, &self.queue)
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// The offscreen the board was last drawn into. Readable, so a thumbnail or a
    /// PNG export can copy the frame that is already on screen.
    pub fn canvas(&self) -> &Compositor {
        &self.compositor
    }

    /// What the last frame's translucent chrome cost. `docs/05` §3a budgets 0.5 ms
    /// and asks for it to be measured rather than assumed.
    pub fn glass_stats(&self) -> GlassStats {
        self.glass_stats
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Device, queue and renderer at once, because a frame needs all three and
    /// borrowing them one at a time through `&mut self` does not compose.
    pub fn parts(&mut self) -> (&wgpu::Device, &wgpu::Queue, &mut Renderer) {
        (&self.device, &self.queue, &mut self.renderer)
    }

    /// Advances the renderer's residency clocks. Once per frame, before anything
    /// marks a texture or a glyph as used.
    pub fn begin_frame(&mut self) {
        self.renderer.begin_frame();
    }

    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter_info
    }

    /// Physical pixels. The single source of truth for the camera's viewport — see
    /// `crate::input` on why every screen-space value in the app is physical.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Reconfigures the swapchain after the window changed size.
    ///
    /// A zero dimension is not a resize but a minimise; configuring a zero-sized
    /// surface is a validation error, so it is dropped and the next non-zero resize
    /// (which the OS always sends on restore) does the work.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if self.config.width == width && self.config.height == height {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.compositor
            .resize(&self.device, self.config.format, width, height);
    }

    /// Re-applies the current configuration. Used when the surface reports itself
    /// outdated or lost, which happens on monitor changes and GPU switches.
    pub fn reconfigure(&mut self) {
        self.surface.configure(&self.device, &self.config);
    }

    /// Draws a frame and presents it. Returns whether anything reached the screen.
    ///
    /// Three stages, in the order `vellum_render::glass` requires: the board into the
    /// offscreen canvas, the blur outside any pass of ours, then the swapchain — blit,
    /// glass, chrome. `crate::compose` explains why the board no longer goes straight
    /// to the screen.
    ///
    /// Every surface condition wgpu can report is recoverable — outdated, occluded,
    /// timed out — so they are handled in place and the frame is skipped rather than
    /// surfaced as an error the caller would only log. A genuine device loss arrives
    /// through wgpu's device-lost callback, not through this path.
    pub fn present(&mut self, frame: Frame<'_>) -> bool {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => texture,
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                // Usable this frame, but the swapchain no longer matches the window.
                self.reconfigure();
                texture
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.reconfigure();
                self.flush_staged_work();
                return false;
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.flush_staged_work();
                return false;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("surface acquisition raised a validation error; skipping frame");
                self.flush_staged_work();
                return false;
            }
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("vellum-frame") });
        self.compose(&mut encoder, &view, frame);

        self.queue.submit(Some(encoder.finish()));
        // Tells the compositor a frame is imminent, which measurably smooths
        // presentation on macOS and Wayland.
        self.window.pre_present_notify();
        self.queue.present(surface_texture);
        true
    }

    /// Flushes work already staged on the queue for a frame that will **not** be
    /// presented, and lets the device reclaim what the flush retires.
    ///
    /// # This is the bug that twice took the machine down
    ///
    /// `Queue::write_buffer` and `Queue::write_texture` do not write. They stage bytes
    /// into wgpu's pending-writes arena, and that arena is drained by the next
    /// **submit**. Everything a frame uploads — the chrome's vertices and indices every
    /// single frame, the egui font atlas whenever it grows, every image the painter
    /// decodes — is staged in `crate::app` *before* the swapchain is acquired here. So
    /// every early return above used to leave all of it sitting in that arena.
    ///
    /// **A window behind another window is `Occluded` on every frame.** That is not an
    /// edge case, it is the ordinary state of an app the user is not currently typing
    /// into, and it means the arena grew by a frame's worth of uploads sixty times a
    /// second for as long as something else was in front. Velm was seen at **14.24 GB**
    /// and then **15.06 GB**; no unattended run ever reproduced it, because an
    /// unattended run either has the window frontmost or uses
    /// [`Self::compose_offscreen`], and both of those submit.
    ///
    /// Measured directly in `vellum-render/tests/texture_churn.rs`: the same upload loop
    /// climbs 340 → 765 MB without a submit and sits flat at 55 MB with one.
    fn flush_staged_work(&mut self) {
        self.queue.submit(std::iter::empty());
        // Submitting hands the staging buffers back; polling is what lets the device
        // actually retire them rather than holding them until some later frame.
        let _ = self.device.poll(wgpu::PollType::Poll);
    }

    /// The same frame, into an offscreen texture that can be read back.
    ///
    /// This exists because **the swapchain cannot be checked**. A window behind
    /// another window is never presented to at all — `get_current_texture` answers
    /// `Occluded` and [`Self::present`] correctly does nothing — so an unattended run
    /// that only watches the frame counter cannot tell a working composite from one
    /// that draws an empty screen. Everything except the acquisition and the present
    /// is shared with [`Self::present`], so what this produces is the frame, not a
    /// second rendering of it.
    pub fn compose_offscreen(&mut self, frame: Frame<'_>) -> Result<crate::capture::Capture> {
        let (width, height) = self.compositor.size();
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vellum-screenshot"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vellum-screenshot"),
        });
        self.compose(&mut encoder, &view, frame);
        self.queue.submit(Some(encoder.finish()));

        crate::capture::read_back(
            &self.device,
            &self.queue,
            &target,
            self.config.format,
            width,
            height,
        )
    }

    /// The three stages of a frame, into whatever the caller is targeting.
    fn compose(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        frame: Frame<'_>,
    ) {
        self.renderer.prepare(&self.device, &self.queue, frame.board);

        // 1. The board, into the offscreen canvas — multisampled, then resolved.
        {
            let (view, resolve_target) = self.compositor.board_attachment();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vellum-board"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(frame.clear),
                        // **Discard, not Store.** The multisampled attachment exists only to
                        // be resolved; keeping it would write 4× the canvas back to memory
                        // every frame for bytes nothing ever reads. On a tile-based GPU —
                        // which is every Apple one — discarding is what lets the samples stay
                        // in tile memory and never reach RAM at all.
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.renderer.draw(&mut pass);
        }

        // 2. The blur, which encodes its own passes and therefore may not be inside
        //    one of ours.
        let (width, height) = self.compositor.size();
        self.glass_stats = frame.glass.prepare(
            &self.device,
            &self.queue,
            encoder,
            &Backdrop {
                texture: self.compositor.view(),
                width,
                height,
                revision: frame.revision,
            },
            frame.panels,
        );

        // 3. The target: the canvas, the glass over it, the chrome over that.
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vellum-composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The blit covers every pixel, so the clear is redundant for
                        // colour — but a load of undefined swapchain contents is not
                        // free on tiled GPUs, and `Clear` is the cheaper of the two.
                        load: wgpu::LoadOp::Clear(frame.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.compositor.blit(&mut pass);
            frame.glass.draw(&mut pass);
            frame.chrome.draw(&mut pass);
        }
    }
}

/// Chooses the surface format.
///
/// A non-sRGB `Unorm` format is preferred so a colour written as `0xFFF79E / 255`
/// reaches the display as `#fff79e`. Miro stores colours as sRGB hex and blends in
/// sRGB, as browsers do; matching that keeps an imported board looking like the
/// original instead of subtly lighter. Falls back to whatever the surface offers
/// first when no linear format is available.
fn preferred_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    caps.formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .or_else(|| caps.formats.first().copied())
        .unwrap_or(wgpu::TextureFormat::Bgra8Unorm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason the format is chosen rather than taken: an sRGB surface
    /// would make every imported Miro colour land lighter than the original.
    #[test]
    fn format_choice_prefers_linear_over_srgb() {
        assert!(wgpu::TextureFormat::Bgra8UnormSrgb.is_srgb());
        assert!(!wgpu::TextureFormat::Bgra8Unorm.is_srgb());

        let caps = |formats: Vec<wgpu::TextureFormat>| wgpu::SurfaceCapabilities {
            formats,
            ..Default::default()
        };
        assert_eq!(
            preferred_format(&caps(vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Bgra8Unorm,
            ])),
            wgpu::TextureFormat::Bgra8Unorm
        );
        // An adapter that offers only sRGB still has to render something.
        assert_eq!(
            preferred_format(&caps(vec![wgpu::TextureFormat::Bgra8UnormSrgb])),
            wgpu::TextureFormat::Bgra8UnormSrgb
        );
        assert_eq!(preferred_format(&caps(vec![])), wgpu::TextureFormat::Bgra8Unorm);
    }
}
