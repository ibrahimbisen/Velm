//! The translucent chrome material, driven on a real adapter and read back.
//!
//! Everything about this material is a claim about pixels, and every one of those
//! claims compiles and validates whether it is true or not: a blur that samples one
//! texel is still a blur, a specular on the wrong edge still draws a line, and an
//! "opaque" fallback that is 99% opaque still looks opaque in a screenshot. So each
//! test here states the property in a form that a plausible-looking wrong
//! implementation fails:
//!
//! - the blur is measured as a **drop in variance** with the mean preserved, because
//!   a blur that read black would drop variance too;
//! - the specular is asserted **present on the top edge and absent on the bottom and
//!   the sides**, because a shader that lit the whole ring would look fine alone;
//! - Reduce Transparency is checked by rendering the panel over two *different*
//!   canvases and demanding byte-identical pixels, which is the only statement of
//!   "genuinely opaque" that cannot be satisfied by a high alpha.

mod common;

use vellum_render::{
    Backdrop, GlassMaterial, GlassMode, GlassPanel, GlassQuality, GlassRenderer, GlassStats, Rgba,
};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// 256 × 4 bytes is exactly the 256-byte row alignment `copy_texture_to_buffer`
/// wants, so the readback needs no padding logic that could itself be wrong — the
/// same reasoning `tests/common` uses for its 64 px target.
const WIDTH: u32 = 256;
const HEIGHT: u32 = 192;

/// The panel every test places, well inside the canvas so its region is never
/// clipped by an edge.
const PANEL_ORIGIN: [f32; 2] = [64.0, 48.0];
const PANEL_SIZE: [f32; 2] = [128.0, 96.0];
const RADIUS: f32 = 6.0;

/// `docs/05-design-language.md` §1, dark mode: panels are `bone` over a `pearl`
/// canvas. Dark is used for the specular test because a 14% white catch over a light
/// surface is a four-value step in 8 bits, which measures nothing.
const DARK_BONE: Rgba = Rgba::new(0.169, 0.184, 0.204, 1.0);
const DARK_FROST: Rgba = Rgba::new(0.227, 0.251, 0.282, 1.0);
const LIGHT_BONE: Rgba = Rgba::new(0.957, 0.961, 0.965, 1.0);

/// A canvas to blur, a target to composite onto, and the readback that reads it.
///
/// The canvas is copied into the target before the pass rather than drawn, so these
/// tests depend on nothing but the glass pipelines themselves — a bug in the quad
/// pipeline cannot make one of them fail or, worse, pass.
struct Stage {
    canvas: wgpu::Texture,
    canvas_view: wgpu::TextureView,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    readback: wgpu::Buffer,
}

impl Stage {
    fn new(device: &wgpu::Device) -> Self {
        assert_eq!(WIDTH * 4 % 256, 0, "the readback must need no row padding");
        let extent = wgpu::Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 };
        let canvas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glass-test-canvas"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glass-test-target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Self {
            canvas_view: canvas.create_view(&Default::default()),
            target_view: target.create_view(&Default::default()),
            canvas,
            target,
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("glass-test-readback"),
                size: u64::from(WIDTH * HEIGHT * 4),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
        }
    }

    fn write_canvas(&self, queue: &wgpu::Queue, rgba: &[u8]) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.canvas,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(WIDTH * 4),
                rows_per_image: Some(HEIGHT),
            },
            wgpu::Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 },
        );
    }

    /// One frame: refresh the backdrops, copy the canvas onto the target, composite
    /// the panels over it, read the result back.
    fn frame(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glass: &mut GlassRenderer,
        panels: &[GlassPanel],
        revision: u64,
    ) -> (GlassStats, Vec<u8>) {
        let mut encoder = device.create_command_encoder(&Default::default());
        let stats = glass.prepare(
            device,
            queue,
            &mut encoder,
            &Backdrop {
                texture: &self.canvas_view,
                width: WIDTH,
                height: HEIGHT,
                revision,
            },
            panels,
        );

        let full = wgpu::Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 };
        fn whole(texture: &wgpu::Texture) -> wgpu::TexelCopyTextureInfo<'_> {
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            }
        }
        encoder.copy_texture_to_texture(whole(&self.canvas), whole(&self.target), full);

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glass-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            glass.draw(&mut pass);
        }

        encoder.copy_texture_to_buffer(
            whole(&self.target),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(WIDTH * 4),
                    rows_per_image: Some(HEIGHT),
                },
            },
            full,
        );
        queue.submit(Some(encoder.finish()));

        self.readback.map_async(wgpu::MapMode::Read, .., |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the readback map must complete");
        let pixels = self
            .readback
            .get_mapped_range(..)
            .expect("the readback buffer must be mapped")
            .to_vec();
        self.readback.unmap();
        (stats, pixels)
    }
}

fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * WIDTH + x) * 4) as usize;
    pixels[offset..offset + 4].try_into().expect("four channels")
}

/// Rec. 709, matching the weighting the saturation lift uses.
fn luma(px: [u8; 4]) -> f32 {
    0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2])
}

/// Mean and variance of the luma over a rectangle, in 0..255.
fn statistics(pixels: &[u8], rect: (u32, u32, u32, u32)) -> (f32, f32) {
    let (x0, y0, x1, y1) = rect;
    let mut values = Vec::new();
    for y in y0..y1 {
        for x in x0..x1 {
            values.push(luma(pixel(pixels, x, y)));
        }
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    let variance =
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / values.len() as f32;
    (mean, variance)
}

/// The panel's interior, inset far enough to exclude the border, the specular and the
/// antialiased corner arcs.
const INTERIOR: (u32, u32, u32, u32) = (
    PANEL_ORIGIN[0] as u32 + 12,
    PANEL_ORIGIN[1] as u32 + 12,
    (PANEL_ORIGIN[0] + PANEL_SIZE[0]) as u32 - 12,
    (PANEL_ORIGIN[1] + PANEL_SIZE[1]) as u32 - 12,
);

fn checkerboard(cell: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let on = ((x / cell) + (y / cell)).is_multiple_of(2);
            rgba.extend_from_slice(if on { &[255, 255, 255, 255] } else { &[0, 0, 0, 255] });
        }
    }
    rgba
}

fn flat(color: [u8; 4]) -> Vec<u8> {
    color.repeat((WIDTH * HEIGHT) as usize)
}

/// The blur has to actually blur.
///
/// Measured as a collapse in variance *with the mean preserved*. Variance alone would
/// be satisfied by a shader that sampled one texel, or the wrong texture, or nothing;
/// holding the mean to the source's says the pixels underneath are still the ones
/// being shown, only spread out.
#[test]
fn the_backdrop_blur_collapses_variance_and_keeps_the_mean() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glass blur check");
    };
    let stage = Stage::new(device);
    let source = checkerboard(8);
    stage.write_canvas(queue, &source);

    // Pure backdrop: no tint, no saturation change, no edges. Anything else would be
    // measuring the composite rather than the blur.
    let material = GlassMaterial::new(Rgba::WHITE.with_alpha(0.0), Rgba::TRANSPARENT)
        .with_saturation(1.0)
        .with_border(Rgba::TRANSPARENT, 0.0)
        .with_highlight(Rgba::TRANSPARENT, 0.0);
    let panel = GlassPanel::new(PANEL_ORIGIN, PANEL_SIZE, material).with_corner_radius(RADIUS);

    let mut glass = GlassRenderer::new(device, FORMAT);
    let (stats, pixels) = stage.frame(device, queue, &mut glass, &[panel], 1);
    assert_eq!(stats.refreshed, 1);
    assert_eq!(stats.passes, 3, "one capture and two Kawase passes");

    let (source_mean, source_variance) = statistics(&source, INTERIOR);
    let (mean, variance) = statistics(&pixels, INTERIOR);

    assert!(
        source_variance > 10_000.0,
        "the source must be high-frequency for this to mean anything: {source_variance}"
    );
    assert!(
        variance * 50.0 < source_variance,
        "variance only fell from {source_variance} to {variance}"
    );
    assert!(
        (mean - source_mean).abs() < 12.0,
        "the blur moved the mean from {source_mean} to {mean}"
    );
}

/// The specular catch is on the **top edge only**.
///
/// §3a: *"a single specular highlight along the top edge only — a 1px inner line at
/// ~14% white. That top-edge catch is what separates the Apple material from a plain
/// blur."* A ring would look plausible in isolation and wrong beside the real thing,
/// so the bottom and the left are asserted to be free of it, not merely dimmer.
#[test]
fn the_specular_is_on_the_top_edge_and_nowhere_else() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glass specular check");
    };
    let stage = Stage::new(device);
    stage.write_canvas(queue, &flat([64, 64, 64, 255]));

    let material = GlassMaterial::new(DARK_BONE.with_alpha(0.68), DARK_FROST);
    let panel = GlassPanel::new(PANEL_ORIGIN, PANEL_SIZE, material).with_corner_radius(RADIUS);

    let mut glass = GlassRenderer::new(device, FORMAT);
    let (_, pixels) = stage.frame(device, queue, &mut glass, &[panel], 1);

    let centre_x = (PANEL_ORIGIN[0] + PANEL_SIZE[0] / 2.0) as u32;
    let centre_y = (PANEL_ORIGIN[1] + PANEL_SIZE[1] / 2.0) as u32;
    let top = PANEL_ORIGIN[1] as u32;
    let bottom = (PANEL_ORIGIN[1] + PANEL_SIZE[1]) as u32;
    let left = PANEL_ORIGIN[0] as u32;

    // Row 0 is the hairline; row 1 is the 1px specular line just inside it.
    let specular = luma(pixel(&pixels, centre_x, top + 1));
    let fill = luma(pixel(&pixels, centre_x, centre_y));
    let under_bottom_border = luma(pixel(&pixels, centre_x, bottom - 2));
    let inside_left_border = luma(pixel(&pixels, left + 1, centre_y));

    assert!(
        specular > fill + 12.0,
        "no catch on the top edge: specular {specular} vs fill {fill}"
    );
    assert!(
        (under_bottom_border - fill).abs() < 3.0,
        "the bottom edge is lit too: {under_bottom_border} vs fill {fill}"
    );
    assert!(
        (inside_left_border - fill).abs() < 3.0,
        "the left edge is lit too: {inside_left_border} vs fill {fill}"
    );

    // And it is one pixel, not a gradient bleeding into the panel.
    let two_in = luma(pixel(&pixels, centre_x, top + 3));
    assert!(
        (two_in - fill).abs() < 3.0,
        "the specular is wider than a pixel: {two_in} three rows in vs fill {fill}"
    );
}

/// Reduce Transparency has to produce pixels that are *genuinely* opaque.
///
/// The strongest available statement of that: render the same panel over a black
/// canvas and over a white one and demand the interiors be byte-identical. A material
/// at 99% opacity passes every eyeball test and fails this one. The blur is asserted
/// to be skipped entirely as well — an opaque panel that still pays for a backdrop is
/// a bug the user cannot see and the frame time can.
#[test]
fn reduce_transparency_yields_genuinely_opaque_pixels() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the reduce-transparency check");
    };
    let stage = Stage::new(device);

    let material = GlassMaterial::new(LIGHT_BONE.with_alpha(0.72), DARK_FROST);
    let panel = GlassPanel::new(PANEL_ORIGIN, PANEL_SIZE, material).with_corner_radius(RADIUS);

    let mut glass = GlassRenderer::new(device, FORMAT);
    glass.set_mode(GlassMode::Opaque);

    stage.write_canvas(queue, &flat([0, 0, 0, 255]));
    let (black_stats, over_black) = stage.frame(device, queue, &mut glass, &[panel], 1);
    stage.write_canvas(queue, &flat([255, 255, 255, 255]));
    let (white_stats, over_white) = stage.frame(device, queue, &mut glass, &[panel], 2);

    assert_eq!(black_stats.passes, 0, "an opaque panel must not blur anything");
    assert_eq!(white_stats.passes, 0);
    assert_eq!(black_stats.refreshed, 0);

    let (x0, y0, x1, y1) = INTERIOR;
    for y in y0..y1 {
        for x in x0..x1 {
            let (a, b) = (pixel(&over_black, x, y), pixel(&over_white, x, y));
            assert_eq!(a, b, "the canvas shows through at ({x},{y}): {a:?} vs {b:?}");
        }
    }
    // …and what shows is the tint at full strength, not a lucky average.
    let expected = LIGHT_BONE.pack();
    let actual = pixel(&over_black, INTERIOR.0 + 4, INTERIOR.1 + 4);
    for channel in 0..3 {
        assert!(
            actual[channel].abs_diff(expected[channel]) <= 2,
            "the opaque fill is not the tint: {actual:?} vs {expected:?}"
        );
    }

    // And the translucent material over the same two canvases genuinely differs, so
    // the assertion above is about opacity rather than about a panel that draws the
    // same thing whatever is underneath it.
    glass.set_mode(GlassMode::Translucent);
    stage.write_canvas(queue, &flat([0, 0, 0, 255]));
    let (_, translucent_black) = stage.frame(device, queue, &mut glass, &[panel], 3);
    stage.write_canvas(queue, &flat([255, 255, 255, 255]));
    let (_, translucent_white) = stage.frame(device, queue, &mut glass, &[panel], 4);
    let a = pixel(&translucent_black, INTERIOR.0 + 4, INTERIOR.1 + 4);
    let b = pixel(&translucent_white, INTERIOR.0 + 4, INTERIOR.1 + 4);
    assert!(luma(b) > luma(a) + 20.0, "the material is not translucent at all: {a:?} vs {b:?}");
}

/// The cache: a panel over a board nobody is touching costs **nothing**.
///
/// §3a asks for zero, not cheap, so this asserts no passes *and* no uploads. It also
/// pins the three ways the cache is allowed to miss — the board changed, the panel
/// moved, a panel was added — because a cache that never invalidates would pass the
/// first assertion and show a stale board.
#[test]
fn an_idle_panel_over_a_static_board_costs_nothing() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glass cache check");
    };
    let stage = Stage::new(device);
    stage.write_canvas(queue, &checkerboard(16));

    let material = GlassMaterial::new(LIGHT_BONE.with_alpha(0.72), DARK_FROST);
    let panel = GlassPanel::new(PANEL_ORIGIN, PANEL_SIZE, material).with_corner_radius(RADIUS);
    let mut glass = GlassRenderer::new(device, FORMAT);

    let (first, _) = stage.frame(device, queue, &mut glass, &[panel], 7);
    assert_eq!(first.refreshed, 1);
    assert_eq!(first.passes, 3);

    let (second, _) = stage.frame(device, queue, &mut glass, &[panel], 7);
    assert!(second.is_free(), "an unchanged frame did work: {second:?}");
    assert_eq!(second.refreshed, 0);
    assert_eq!(second.uploads, 0);

    // The board changed underneath it.
    let (edited, _) = stage.frame(device, queue, &mut glass, &[panel], 8);
    assert_eq!(edited.refreshed, 1, "a canvas revision must invalidate the backdrop");

    // The panel moved. Its slot is the same size, so nothing is repacked and only its
    // own region is recaptured.
    let moved = GlassPanel { origin: [PANEL_ORIGIN[0] + 8.0, PANEL_ORIGIN[1]], ..panel };
    let (dragged, _) = stage.frame(device, queue, &mut glass, &[moved], 8);
    assert_eq!(dragged.refreshed, 1);
    let (still, _) = stage.frame(device, queue, &mut glass, &[moved], 8);
    assert!(still.is_free(), "a panel that stopped moving did work: {still:?}");

    // A second panel: a new region, so the atlas is laid out again and both are
    // recaptured. That is the honest cost of the surface set changing.
    let second_panel = GlassPanel::new([16.0, 16.0], [32.0, 32.0], material).with_corner_radius(4.0);
    let (added, _) = stage.frame(device, queue, &mut glass, &[moved, second_panel], 8);
    assert_eq!(added.refreshed, 2);
    let (settled, _) = stage.frame(device, queue, &mut glass, &[moved, second_panel], 8);
    assert!(settled.is_free(), "two idle panels did work: {settled:?}");
}

/// Regions of different panels sit side by side in one atlas, and the blur must not
/// drag one into the other.
///
/// The failure this catches is subtle and would look like a plausible material: a
/// panel over a dark part of the board picking up a wash of light from a neighbour
/// that happens to have been packed next to it.
#[test]
fn one_panels_backdrop_does_not_bleed_into_the_next() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the region bleed check");
    };
    let stage = Stage::new(device);

    // Left half black, right half white — and a panel wholly inside each half.
    let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for _ in 0..HEIGHT {
        for x in 0..WIDTH {
            rgba.extend_from_slice(if x < WIDTH / 2 {
                &[0, 0, 0, 255]
            } else {
                &[255, 255, 255, 255]
            });
        }
    }
    stage.write_canvas(queue, &rgba);

    let material = GlassMaterial::new(Rgba::WHITE.with_alpha(0.0), Rgba::TRANSPARENT)
        .with_saturation(1.0)
        .with_border(Rgba::TRANSPARENT, 0.0)
        .with_highlight(Rgba::TRANSPARENT, 0.0);
    // Both are far enough from the seam that the blur itself cannot reach across it.
    let dark = GlassPanel::new([8.0, 40.0], [72.0, 72.0], material).with_corner_radius(4.0);
    let light = GlassPanel::new([176.0, 40.0], [72.0, 72.0], material).with_corner_radius(4.0);

    let mut glass = GlassRenderer::new(device, FORMAT);
    let (stats, pixels) = stage.frame(device, queue, &mut glass, &[dark, light], 1);
    assert_eq!(stats.refreshed, 2);

    let dark_centre = luma(pixel(&pixels, 44, 76));
    let light_centre = luma(pixel(&pixels, 212, 76));
    assert!(dark_centre < 6.0, "the dark panel picked up light: {dark_centre}");
    assert!(light_centre > 249.0, "the light panel picked up dark: {light_centre}");
}

/// The degradation §3a names: the material drops to a flat tint rather than the frame
/// rate being sacrificed.
///
/// It has to remain *translucent* — that is the whole point of the material for a
/// canvas app — while costing no passes at all.
#[test]
fn the_flat_fallback_is_translucent_and_encodes_no_passes() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the flat fallback check");
    };
    let stage = Stage::new(device);
    stage.write_canvas(queue, &flat([0, 0, 0, 255]));

    let material = GlassMaterial::new(LIGHT_BONE.with_alpha(0.72), DARK_FROST);
    let panel = GlassPanel::new(PANEL_ORIGIN, PANEL_SIZE, material).with_corner_radius(RADIUS);

    let mut glass = GlassRenderer::new(device, FORMAT);
    glass.set_quality(GlassQuality::Flat);
    let (stats, pixels) = stage.frame(device, queue, &mut glass, &[panel], 1);

    assert_eq!(stats.passes, 0, "the flat fallback must not blur");
    assert_eq!(stats.refreshed, 0);

    // 72% of `bone` over black.
    let interior = pixel(&pixels, INTERIOR.0 + 4, INTERIOR.1 + 4);
    let expected = (LIGHT_BONE.r * 0.72 * 255.0) as u8;
    assert!(
        interior[0].abs_diff(expected) <= 3,
        "the flat tint is not 72% bone over black: {interior:?} vs {expected}"
    );
    assert!(interior[0] < 250, "the flat fallback went opaque");
}

/// A panel hanging off the edge of the window still gets a material rather than a
/// clipped one — the capture edge-extends the canvas instead of sampling past it.
///
/// This is the frame a flyout has while it is animating in from the left, so it is
/// not a corner case; a dark seam along that edge would be visible every time.
#[test]
fn a_panel_overhanging_the_window_edge_is_not_darkened() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the window-edge check");
    };
    let stage = Stage::new(device);
    stage.write_canvas(queue, &flat([200, 200, 200, 255]));

    let material = GlassMaterial::new(Rgba::WHITE.with_alpha(0.0), Rgba::TRANSPARENT)
        .with_saturation(1.0)
        .with_border(Rgba::TRANSPARENT, 0.0)
        .with_highlight(Rgba::TRANSPARENT, 0.0);
    let panel = GlassPanel::new([-24.0, 40.0], [96.0, 72.0], material).with_corner_radius(4.0);

    let mut glass = GlassRenderer::new(device, FORMAT);
    let (stats, pixels) = stage.frame(device, queue, &mut glass, &[panel], 1);
    assert_eq!(stats.refreshed, 1, "an overhanging panel still gets a backdrop");

    // Every visible column of the panel reproduces the flat canvas. A region clipped
    // to the window would have averaged in whatever the clamp reached instead.
    for x in 1..70 {
        let sample = pixel(&pixels, x, 76);
        assert!(
            sample[0].abs_diff(200) <= 3,
            "column {x} is {sample:?}, not the canvas it is standing on"
        );
    }
}
