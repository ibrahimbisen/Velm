//! The GPU harness the integration tests share.
//!
//! Every item is used by at least one test binary but no binary uses all of them, and
//! Rust compiles a test module separately into each — hence the blanket allow rather
//! than a scatter of them.
#![allow(dead_code)]

use std::sync::OnceLock;

use vellum_render::{DrawList, Renderer};

/// Side of the offscreen target. 64 px because 64 × 4 bytes is exactly the 256-byte
/// row alignment `copy_texture_to_buffer` requires, so the readback needs no padding
/// logic that could itself be wrong.
pub const TARGET: u32 = 64;
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// One device per test binary. Creating one per test serialises the suite behind
/// driver initialisation for no benefit.
pub fn gpu() -> Option<&'static (wgpu::Device, wgpu::Queue)> {
    static GPU: OnceLock<Option<(wgpu::Device, wgpu::Queue)>> = OnceLock::new();
    GPU.get_or_init(|| {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vellum-render-tests"),
            // The WebGPU baseline, exactly as `vellum-app` asks for. If anything in
            // this crate needed more, the wasm viewer would stop being a recompile.
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .ok()
    })
    .as_ref()
}

/// Says a test is being skipped for want of an adapter, which is the honest outcome
/// on a headless box without a software fallback — the alternative is a suite that
/// fails for reasons unconnected to the code.
pub fn skipped(what: &str) {
    eprintln!("no GPU adapter available; skipping {what}");
}

/// Prepares `list` and draws it into a `TARGET`×`TARGET` texture, returning the
/// framebuffer.
pub fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    list: &DrawList,
) -> Vec<u8> {
    renderer.prepare(device, queue, list);
    redraw(device, queue, renderer)
}

/// Draws whatever the last [`Renderer::prepare`] recorded, without preparing again.
/// Used to check the incremental paths that deliberately skip a full upload.
pub fn redraw(device: &wgpu::Device, queue: &wgpu::Queue, renderer: &Renderer) -> Vec<u8> {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test-target"),
        size: wgpu::Extent3d { width: TARGET, height: TARGET, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    // The board's pipelines are multisampled (`vellum_render::BOARD_SAMPLES`), and a
    // pipeline's sample count must match its attachment — so the harness renders exactly the
    // way `vellum_app::surface` does: into a multisampled attachment that resolves into the
    // single-sampled texture these tests read back. Deliberately the *same* configuration as
    // the app rather than a single-sampled one with the count turned down for the tests: a
    // harness that renders differently from production is a harness that can be green while
    // the app cannot draw a frame.
    let multisampled = device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("test-target-msaa"),
            size: wgpu::Extent3d { width: TARGET, height: TARGET, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: vellum_render::BOARD_SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default());
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test-readback"),
        size: u64::from(TARGET * TARGET * 4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &multisampled,
                depth_slice: None,
                resolve_target: Some(&view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        renderer.draw(&mut pass);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TARGET * 4),
                rows_per_image: Some(TARGET),
            },
        },
        wgpu::Extent3d { width: TARGET, height: TARGET, depth_or_array_layers: 1 },
    );
    queue.submit(Some(encoder.finish()));

    readback.map_async(wgpu::MapMode::Read, .., |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the readback map must complete");
    let pixels = readback
        .get_mapped_range(..)
        .expect("the readback buffer must be mapped")
        .to_vec();
    readback.unmap();
    pixels
}

pub fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * TARGET + x) * 4) as usize;
    pixels[offset..offset + 4].try_into().expect("four channels")
}

pub fn assert_near(actual: [u8; 4], expected: [u8; 4], tolerance: u8, what: &str) {
    let close = actual.iter().zip(&expected).all(|(a, b)| a.abs_diff(*b) <= tolerance);
    assert!(close, "{what}: {actual:?} is not within {tolerance} of {expected:?}");
}
