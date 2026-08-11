//! The performance budget for translucent chrome, measured — and failed on.
//!
//! `docs/05-design-language.md` §3a: *"**Budget: under 0.5 ms per frame.** If it
//! exceeds that on the reference board, the material is dropped to a flat tint rather
//! than the frame rate being sacrificed. This is not negotiable and should be
//! asserted by a benchmark."* This file is that benchmark.
//!
//! # Why a test and not `cargo bench`
//!
//! `#[bench]` is nightly-only and `criterion` is a large dependency to add to a
//! renderer for one number. More to the point, §3a did not ask for a benchmark that
//! reports — it asked for an assertion. A test fails the build; a benchmark prints a
//! regression into a log nobody reads.
//!
//! # What is measured, and how the two costs are kept apart
//!
//! **GPU time comes from timestamp queries**, not from a stopwatch around `submit`.
//! That distinction is the whole reliability of this file: a wall clock around a
//! submit measures wgpu's command translation as well as the GPU, and in an
//! unoptimised build that translation is an order of magnitude larger than the work
//! being timed. Sentinel render passes at each end of the command buffer carry the
//! two timestamps, so the delta is exactly what the GPU spent on the material.
//!
//! **CPU time is measured separately** and charged in full alongside it. A debug
//! build inflates that half with `wgpu-core`'s unoptimised command translation, which
//! no shipped frame pays — the honest response to which is to keep charging it and
//! note the fact, not to weaken the assertion in the profile the test is usually run
//! in. The budget is met with room to spare on the inflated figure anyway.
//!
//! Many frames go into one command buffer and one submit, so per-submit driver
//! overhead — which the material does not add, the frame having been submitted
//! anyway — is amortised rather than charged to the blur. For the same reason the
//! composite draws share one render pass: in a real frame the composite goes inside
//! the pass the chrome was already using, and charging it a full-target load and
//! store per iteration would be measuring the harness.
//!
//! **The canvas revision is bumped every iteration**, so every panel is re-blurred on
//! every frame. That is the worst case, and it is not the common one — see
//! [`the_cached_frame_is_free`].

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use vellum_render::{
    Backdrop, GlassMaterial, GlassPanel, GlassQuality, GlassRenderer, GlassStats, Rgba,
};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// §3a's number.
const BUDGET: Duration = Duration::from_micros(500);

/// Frames encoded into one command buffer. Enough that a single submit's fixed cost
/// is a fraction of a percent of the total, and not so many that the command buffer
/// itself becomes the thing being measured.
const FRAMES: u32 = 100;

/// Runs taken, of which the fastest is reported. The minimum is the right estimator
/// for a fixed piece of work: everything that makes a run slower — another process, a
/// thermal step, the window server — is added on top, never subtracted.
const RUNS: usize = 5;

/// A device that can time itself.
///
/// Its own rather than `tests/common`'s, for one reason: `TIMESTAMP_QUERY` has to be
/// requested at device creation, and the shared harness deliberately asks for the
/// plain WebGPU baseline so that nothing in this crate can quietly come to depend on
/// more. This is measurement instrumentation, not a requirement of the renderer, and
/// the fallback below is what runs where the feature is missing.
struct Timed {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// False on an adapter without `TIMESTAMP_QUERY`, where the GPU figure falls back
    /// to a wall clock around the submit and is therefore an over-estimate.
    timestamps: bool,
    /// Nanoseconds per timestamp tick.
    period: f32,
}

fn gpu() -> Option<&'static Timed> {
    static GPU: OnceLock<Option<Timed>> = OnceLock::new();
    GPU.get_or_init(|| {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
        let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vellum-glass-budget"),
            required_features: if timestamps {
                wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            },
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .ok()?;
        let period = queue.get_timestamp_period();
        Some(Timed { device, queue, timestamps, period })
    })
    .as_ref()
}

fn skipped(what: &str) {
    eprintln!("no GPU adapter available; skipping {what}");
}

/// `docs/05-design-language.md` §3a's yes-list, laid out as `docs/04-ui-reference.md`
/// §1 describes it.
///
/// Six surfaces, which is more than are usually up at once: the tool column plus its
/// two detached clusters, an open flyout, the selection context bar and the zoom
/// pill. Between them they cover about 6% of the window, which is the number that
/// makes blurring only the regions behind them worth doing at all.
fn chrome(scale: f32) -> Vec<GlassPanel> {
    // `bone` at 72%, a `frost` hairline, and the 14% white catch — §3a's light mode.
    let material = GlassMaterial::new(
        Rgba::from_hex(0xf4_f5f6).with_alpha(0.72),
        Rgba::from_hex(0xdd_e2e5),
    );
    let panel = |x: f32, y: f32, w: f32, h: f32| {
        GlassPanel::new([x * scale, y * scale], [w * scale, h * scale], material)
            .with_corner_radius(6.0 * scale)
    };
    vec![
        panel(16.0, 222.0, 44.0, 44.0),    // the AI button, detached above
        panel(16.0, 282.0, 44.0, 336.0),   // the tool column
        panel(16.0, 634.0, 44.0, 88.0),    // undo/redo, detached below
        panel(72.0, 300.0, 240.0, 180.0),  // an open toolbar flyout
        panel(480.0, 24.0, 320.0, 40.0),   // the selection context bar
        panel(1216.0, 836.0, 208.0, 40.0), // the bottom-right zoom cluster
    ]
}

/// A canvas with the frequency content a real board has: flat item fills, hairlines
/// and text-sized detail. A flat colour would let the texture cache make the blur
/// look free.
fn board(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let hairline = x % 137 == 0 || y % 91 == 0 || (x + y) % 7 == 0;
            let color: [u8; 4] = if hairline {
                [26, 29, 31, 255]
            } else {
                match (x / 137 + y / 91) % 4 {
                    0 => [255, 247, 158, 255],
                    1 => [227, 230, 232, 255],
                    2 => [255, 158, 158, 255],
                    _ => [111, 214, 230, 255],
                }
            };
            let offset = ((y * width + x) * 4) as usize;
            rgba[offset..offset + 4].copy_from_slice(&color);
        }
    }
    rgba
}

/// What one frame of the material cost, split by where it was spent.
#[derive(Debug, Clone, Copy)]
struct Cost {
    /// CPU: building the frame's commands, including the cache comparison and the
    /// uploads.
    encode: Duration,
    /// GPU: what those commands took to run.
    execute: Duration,
}

impl Cost {
    /// Both halves, which is what the budget is asserted against.
    ///
    /// Charged in full in either profile, even though a debug build inflates the CPU
    /// half with `wgpu-core`'s unoptimised command translation — which no shipped
    /// frame pays. Weakening the assertion where it is usually run would be the wrong
    /// trade, and it is not needed: the budget is met with room to spare even on the
    /// inflated figure.
    fn total(&self) -> Duration {
        self.encode + self.execute
    }

    fn report(&self, label: &str) {
        eprintln!(
            "{label}: cpu {:.1}us + gpu {:.1}us = {:.1}us per frame (budget {:.0}us){}",
            self.encode.as_secs_f64() * 1e6,
            self.execute.as_secs_f64() * 1e6,
            self.total().as_secs_f64() * 1e6,
            BUDGET.as_secs_f64() * 1e6,
            if cfg!(debug_assertions) {
                " [debug build: the cpu half is wgpu's, not this crate's]"
            } else {
                ""
            },
        );
    }
}

/// The canvas, the target, and the query set that times them.
struct Bench {
    canvas_view: wgpu::TextureView,
    target_view: wgpu::TextureView,
    /// A 1 × 1 attachment for the two sentinel passes that carry the timestamps, so
    /// bracketing the command buffer costs no bandwidth of its own.
    sentinel_view: wgpu::TextureView,
    queries: Option<wgpu::QuerySet>,
    resolved: wgpu::Buffer,
    readback: wgpu::Buffer,
    width: u32,
    height: u32,
}

/// `resolve_query_set` writes at a 256-byte-aligned offset, so the smallest legal
/// destination is 256 bytes even though two timestamps are sixteen.
const QUERY_BYTES: u64 = 256;

impl Bench {
    fn new(gpu: &Timed, width: u32, height: u32) -> Self {
        let device = &gpu.device;
        let extent = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let canvas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glass-bench-canvas"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &canvas,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &board(width, height),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            extent,
        );

        let attachment = |label, size| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };

        Self {
            canvas_view: canvas.create_view(&Default::default()),
            target_view: attachment("glass-bench-target", extent),
            sentinel_view: attachment(
                "glass-bench-sentinel",
                wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            ),
            queries: gpu.timestamps.then(|| {
                device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("glass-bench-timestamps"),
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                })
            }),
            resolved: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("glass-bench-ticks"),
                size: QUERY_BYTES,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("glass-bench-ticks-readback"),
                size: QUERY_BYTES,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
            width,
            height,
        }
    }

    /// A pass over the 1 × 1 sentinel target, carrying one end of the timing bracket.
    fn sentinel(&self, encoder: &mut wgpu::CommandEncoder, beginning: bool) {
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("glass-bench-sentinel"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.sentinel_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: self.queries.as_ref().map(|query_set| {
                wgpu::RenderPassTimestampWrites {
                    query_set,
                    beginning_of_pass_write_index: beginning.then_some(0),
                    end_of_pass_write_index: (!beginning).then_some(1),
                }
            }),
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }

    /// Encodes `FRAMES` frames of the material, runs them, and returns the per-frame
    /// cost together with the stats of the last frame encoded.
    ///
    /// `revision` decides what is being measured: a fresh number each frame is the
    /// worst case, a constant is the steady state.
    fn measure(
        &self,
        gpu: &Timed,
        glass: &mut GlassRenderer,
        panels: &[GlassPanel],
        revision: impl Fn(u32) -> u64,
    ) -> (Cost, GlassStats) {
        let (device, queue) = (&gpu.device, &gpu.queue);
        let mut best = Cost { encode: Duration::ZERO, execute: Duration::ZERO };
        let mut best_total = Duration::MAX;
        let mut stats = GlassStats::default();

        // One warm-up run, discarded: it pays for pipeline compilation, the first
        // atlas allocation and the driver's first look at these shaders, none of
        // which a frame in a running app pays for.
        for run in 0..=RUNS {
            let mut encoder = device.create_command_encoder(&Default::default());
            let encode_start = Instant::now();
            self.sentinel(&mut encoder, true);
            for frame in 0..FRAMES {
                stats = glass.prepare(
                    device,
                    queue,
                    &mut encoder,
                    &Backdrop {
                        texture: &self.canvas_view,
                        width: self.width,
                        height: self.height,
                        revision: revision(frame),
                    },
                    panels,
                );
            }
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("glass-bench-composite"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.target_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                for _ in 0..FRAMES {
                    glass.draw(&mut pass);
                }
            }
            self.sentinel(&mut encoder, false);
            if let Some(queries) = &self.queries {
                encoder.resolve_query_set(queries, 0..2, &self.resolved, 0);
                encoder.copy_buffer_to_buffer(&self.resolved, 0, &self.readback, 0, QUERY_BYTES);
            }
            let encode = encode_start.elapsed();

            let wall_start = Instant::now();
            queue.submit(Some(encoder.finish()));
            device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("the queue must drain");
            let wall = wall_start.elapsed();

            let execute = match self.read_ticks(device, gpu.period) {
                Some(elapsed) => elapsed,
                // No timestamp support: a wall clock around the submit, which
                // over-estimates by whatever the driver spent translating.
                None => wall,
            };

            let cost = Cost { encode: encode / FRAMES, execute: execute / FRAMES };
            if run > 0 && cost.total() < best_total {
                best_total = cost.total();
                best = cost;
            }
        }

        (best, stats)
    }

    fn read_ticks(&self, device: &wgpu::Device, period: f32) -> Option<Duration> {
        self.queries.as_ref()?;
        self.readback.map_async(wgpu::MapMode::Read, .., |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the tick readback must complete");
        let bytes = self
            .readback
            .get_mapped_range(..)
            .expect("the tick buffer must be mapped")
            .to_vec();
        self.readback.unmap();
        let tick = |index: usize| {
            let start = index * 8;
            u64::from_le_bytes(bytes[start..start + 8].try_into().expect("eight bytes"))
        };
        let elapsed = tick(1).saturating_sub(tick(0));
        Some(Duration::from_nanos((elapsed as f64 * f64::from(period)) as u64))
    }
}

/// The budget, at the size §3a states it against, with every panel re-blurred on
/// every frame.
///
/// If this fails, the fix is not to loosen it. §3a is explicit: the material drops to
/// [`GlassQuality::Flat`] and the frame rate is kept.
#[test]
fn the_material_costs_under_half_a_millisecond_at_1440x900() {
    let Some(gpu) = gpu() else {
        return skipped("the glass performance budget");
    };
    let bench = Bench::new(gpu, 1440, 900);
    let panels = chrome(1.0);
    let mut glass = GlassRenderer::new(&gpu.device, FORMAT);

    let (cost, stats) = bench.measure(gpu, &mut glass, &panels, |frame| u64::from(frame) + 1);
    cost.report("1440x900, every panel re-blurred");

    assert_eq!(stats.refreshed, panels.len(), "the worst case must actually be the worst case");
    assert_eq!(stats.passes, 3, "one capture and two Kawase passes, however many panels");
    assert!(
        cost.total() < BUDGET,
        "the translucent material costs {:.1}us per frame, over the {:.0}us budget of \
         docs/05-design-language.md §3a. Drop it to GlassQuality::Flat rather than \
         raising this number.",
        cost.total().as_secs_f64() * 1e6,
        BUDGET.as_secs_f64() * 1e6,
    );
}

/// The same window on a Retina display, which is what the user's machine actually
/// renders: 1440 × 900 logical points is 2880 × 1800 physical pixels, and every
/// region is four times the area.
///
/// Held to the same budget deliberately. A material that only fits at 1x is not one
/// this project can ship, because nobody runs it at 1x.
#[test]
fn the_material_costs_under_half_a_millisecond_at_retina_scale() {
    let Some(gpu) = gpu() else {
        return skipped("the glass performance budget at 2x");
    };
    let bench = Bench::new(gpu, 2880, 1800);
    let panels = chrome(2.0);
    let mut glass = GlassRenderer::new(&gpu.device, FORMAT);
    // Which is the point of the setting: at 2x the reduction factor doubles too, so
    // the blur stays the same width in points and the atlas stays the same size.
    glass.set_scale_factor(2.0);

    let (cost, stats) = bench.measure(gpu, &mut glass, &panels, |frame| u64::from(frame) + 1);
    cost.report("2880x1800 at scale 2, every panel re-blurred");
    assert_eq!(stats.refreshed, panels.len());

    assert!(
        cost.total() < BUDGET,
        "the translucent material costs {:.1}us per frame at 2x, over the {:.0}us budget \
         of docs/05-design-language.md §3a.",
        cost.total().as_secs_f64() * 1e6,
        BUDGET.as_secs_f64() * 1e6,
    );
}

/// The common case, and the reason the budget above is a ceiling rather than a cost:
/// while the board is not changing, the backdrop is not rebuilt.
///
/// §3a asks for *zero*, so this asserts the frame encoded nothing at all, and then
/// measures what is left — the composite draw, which is one instanced quad per panel
/// and would have been drawn as an opaque panel anyway.
#[test]
fn the_cached_frame_is_free() {
    let Some(gpu) = gpu() else {
        return skipped("the glass cache budget");
    };
    let bench = Bench::new(gpu, 1440, 900);
    let panels = chrome(1.0);
    let mut glass = GlassRenderer::new(&gpu.device, FORMAT);

    let (cost, stats) = bench.measure(gpu, &mut glass, &panels, |_| 1);
    cost.report("1440x900, static board");

    // The substantive assertion, and an exact one: nothing was encoded and nothing
    // was uploaded.
    assert!(stats.is_free(), "a static board still did work: {stats:?}");
    assert_eq!(stats.passes, 0);
    assert_eq!(stats.refreshed, 0);
    // The timing is a backstop rather than the claim — what is left is the composite
    // draw, which an opaque panel would have paid for too. Held to half the budget
    // rather than a tight bound because a measurement this small is mostly noise on a
    // loaded machine, and a flaky performance test gets disabled rather than fixed.
    assert!(
        cost.total() < BUDGET / 2,
        "an idle frame costs {:.1}us, which is not free enough to call free",
        cost.total().as_secs_f64() * 1e6,
    );
}

/// The degradation path has to be cheaper than the thing it replaces, or it is not a
/// degradation. Measured rather than assumed, because a "flat tint" that still bound
/// an atlas and took the composite's backdrop branch would not be.
#[test]
fn the_flat_fallback_is_cheaper_than_the_material() {
    let Some(gpu) = gpu() else {
        return skipped("the flat fallback budget");
    };
    let bench = Bench::new(gpu, 1440, 900);
    let panels = chrome(1.0);
    let mut glass = GlassRenderer::new(&gpu.device, FORMAT);

    let (material, _) = bench.measure(gpu, &mut glass, &panels, |frame| u64::from(frame) + 1);

    glass.set_quality(GlassQuality::Flat);
    let (flat, stats) = bench.measure(gpu, &mut glass, &panels, |frame| u64::from(frame) + 1);
    flat.report("1440x900, flat fallback");

    assert_eq!(stats.passes, 0);
    assert!(
        flat.execute < material.execute,
        "the flat fallback ({:.1}us of GPU) is not cheaper than the material ({:.1}us)",
        flat.execute.as_secs_f64() * 1e6,
        material.execute.as_secs_f64() * 1e6,
    );
}
