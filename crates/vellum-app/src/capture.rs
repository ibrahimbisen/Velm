//! Rendering a board to an image rather than to the window.
//!
//! Two callers want the same thing: the board library wants a thumbnail for every
//! board (`docs/04-ui-reference.md` §5 shows one per row), and **Board ▸ Export ▸
//! PNG** wants the whole board at full size. Both are "point a camera at the content,
//! run the ordinary frame, read the pixels back", so both are this file.
//!
//! # It is the ordinary renderer
//!
//! `vellum_render::Renderer` takes a device and a target format and records into a
//! pass the caller opened — it has never known about windows. So a capture is not a
//! second renderer with its own bugs: it is the same [`crate::draw::Painter`], the
//! same glyph atlas and the same texture cache, pointed at an offscreen attachment.
//! A thumbnail that disagrees with the screen would be a bug in one of them; there is
//! only one of them.
//!
//! # Why the readback is synchronous
//!
//! It blocks the calling thread on `Device::poll`. That is acceptable *because of
//! when it happens*: a thumbnail is taken when a board is closed or the app quits,
//! and an export is an explicit action with a progress-free expectation. It is never
//! on the frame path. `docs/01-architecture.md` §3's budget is about frames, and this
//! is not one.
//!
//! # Rows are padded
//!
//! `copy_texture_to_buffer` requires each row to start on a 256-byte boundary, so a
//! 300px-wide capture writes 1200 bytes of pixels into a 1280-byte row. Forgetting
//! the padding produces an image that shears progressively to the left, which looks
//! like a rendering bug rather than a buffer-layout one.

use anyhow::{Context, Result};
use vellum_render::{DrawList, Renderer};

/// Longest edge of a board-library thumbnail, in pixels.
///
/// The library's grid cards are about 180 points wide, so 512 covers a 2× display
/// with room to spare and still encodes to a few tens of kilobytes.
pub const THUMBNAIL_SIZE: u32 = 512;

/// What one capture produced: 8-bit straight RGBA, tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Capture {
    /// Encodes to PNG.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let image =
            image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
                .context("the capture's byte count does not match its size")?;
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .context("encoding the capture as PNG")?;
        Ok(png)
    }
}

/// Where a capture is drawn, and with what.
///
/// A struct rather than a parameter list because six of the seven are the *same*
/// values a frame already has — device, queue, renderer, format, clear — and naming
/// them at the call site is what stops a capture being taken with a renderer built
/// for a different target format, which is a validation error rather than a colour
/// difference.
pub struct Target<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub renderer: &'a mut Renderer,
    /// Must be the format `renderer` was built for.
    pub format: wgpu::TextureFormat,
    pub clear: wgpu::Color,
    pub width: u32,
    pub height: u32,
}

/// Draws `list` into an offscreen texture and reads it back.
pub fn render(target: Target<'_>, list: &DrawList) -> Result<Capture> {
    let Target { device, queue, renderer, format, clear, .. } = target;
    let (width, height) = capture_size(target.width, target.height);

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vellum-capture"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    // The board's five pipelines are multisampled (`vellum_render::BOARD_SAMPLES`), and a
    // pipeline's sample count must match the attachment it draws into — so a thumbnail and a
    // PNG export need the same multisampled attachment the live canvas uses, resolving into
    // the texture that is read back. Not merely a compatibility obligation: an exported PNG
    // of a board full of pen strokes is one of the places jagged ink is most visible, because
    // it is looked at outside the app and often printed.
    let multisampled = device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("vellum-capture-msaa"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: vellum_render::BOARD_SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default());

    renderer.prepare(device, queue, list);
    let mut encoder = device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("vellum-capture") });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vellum-capture"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &multisampled,
                depth_slice: None,
                resolve_target: Some(&view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
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
    queue.submit(Some(encoder.finish()));

    read_back(device, queue, &texture, format, width, height)
}

/// Copies a texture the caller has already rendered into, and reads it back.
///
/// Split from [`render`] so `crate::surface` can share it: the screenshot path
/// composites the *whole* frame — board, glass and chrome — into its own target and
/// then wants exactly this. The copy is its own submission, which is what orders it
/// after whatever the caller submitted to draw the texture in the first place.
pub fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> Result<Capture> {
    let padded_row = padded_bytes_per_row(width);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vellum-capture-readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("vellum-readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit(Some(encoder.finish()));

    let (sender, receiver) = std::sync::mpsc::channel();
    readback
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            // A closed channel means the caller gave up; the map still happened.
            let _ = sender.send(result);
        });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|error| anyhow::anyhow!("waiting for the capture: {error:?}"))?;
    receiver
        .recv()
        .context("the capture's readback never completed")?
        .map_err(|error| anyhow::anyhow!("mapping the capture: {error}"))?;

    let rgba = {
        let mapped = readback
            .slice(..)
            .get_mapped_range()
            .map_err(|error| anyhow::anyhow!("reading the capture: {error}"))?;
        unpack(&mapped, width, height, padded_row, format)
    };
    readback.unmap();

    Ok(Capture { width, height, rgba })
}

/// A capture bigger than this is a mistake rather than an intention: 8192² of RGBA is
/// already 256 MB of readback buffer, and the reference board is 41282 px wide, so an
/// unclamped "export the whole board at 1:1" would ask for 2.9 GB.
const MAX_EDGE: u32 = 8192;

/// A ceiling on a capture's **area**, not just its longest edge.
///
/// # Why the edge cap stopped being enough
///
/// A capture now allocates two attachments: the single-sampled texture that is read back, and
/// the multisampled one the board is drawn into (`vellum_render::BOARD_SAMPLES`, added so an
/// exported PNG has the same smooth ink the screen does). That is `4 + 4×4 = 20` bytes per
/// pixel. At the edge cap alone, 8192² asks for **1.34 GB in one allocation** on a machine
/// with 8 GB that has kernel-panicked twice during ordinary compilation.
///
/// 20 megapixels holds that under ~400 MB and is still a generous export — larger than a
/// 5K display, and larger than anything that prints. Applied as an area so a wide, short board
/// is not punished for its shape: scaling both axes by the same factor is what keeps an export
/// from silently changing aspect ratio, which is worse than being smaller than asked for.
const MAX_PIXELS: u64 = 20_000_000;

/// Clamps a requested capture to something the GPU can actually allocate.
fn capture_size(width: u32, height: u32) -> (u32, u32) {
    let (mut w, mut h) = (width.clamp(1, MAX_EDGE), height.clamp(1, MAX_EDGE));
    let pixels = u64::from(w) * u64::from(h);
    if pixels > MAX_PIXELS {
        // One factor for both axes, so the picture is smaller and not distorted.
        let shrink = (MAX_PIXELS as f64 / pixels as f64).sqrt();
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped")]
        {
            w = ((f64::from(w) * shrink) as u32).max(1);
            h = ((f64::from(h) * shrink) as u32).max(1);
        }
        log::info!("capture clamped to {w}x{h}: {pixels} pixels is past the memory budget");
    }
    (w, h)
}

/// The size a capture of `content` should be, fitted inside `longest_edge` and never
/// upscaled past 1:1.
///
/// Returns `None` for a board with no content, which has no aspect ratio and would
/// otherwise produce a 1×1 image nobody wants to look at.
pub fn fitted_size(content: (f64, f64), longest_edge: u32) -> Option<(u32, u32)> {
    let (w, h) = content;
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (f64::from(longest_edge) / w.max(h)).min(1.0);
    let width = (w * scale).round().clamp(1.0, f64::from(MAX_EDGE)) as u32;
    let height = (h * scale).round().clamp(1.0, f64::from(MAX_EDGE)) as u32;
    Some((width, height))
}

/// wgpu's row alignment for a texture-to-buffer copy.
fn padded_bytes_per_row(width: u32) -> u32 {
    let unpadded = width * 4;
    unpadded.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
}

/// Strips the row padding and puts the channels in RGBA order.
///
/// The surface format is chosen by the adapter and is `Bgra8Unorm` on Metal — see
/// `crate::surface::preferred_format`, which prefers a non-sRGB format so an imported
/// Miro colour reaches the display as the hex Miro stored. That leaves the channel
/// order to sort out here, and getting it wrong swaps red and blue in every
/// thumbnail, which reads as a colour-management bug and is not one.
fn unpack(
    bytes: &[u8],
    width: u32,
    height: u32,
    padded_row: u32,
    format: wgpu::TextureFormat,
) -> Vec<u8> {
    let swizzle = matches!(
        format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let row_bytes = (width * 4) as usize;
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        let Some(row) = bytes.get(start..start + row_bytes) else { break };
        if swizzle {
            for texel in row.chunks_exact(4) {
                out.extend_from_slice(&[texel[2], texel[1], texel[0], texel[3]]);
            }
        } else {
            out.extend_from_slice(row);
        }
    }
    out.resize(row_bytes * height as usize, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_padded_to_wgpus_alignment() {
        assert_eq!(padded_bytes_per_row(64), 256);
        assert_eq!(padded_bytes_per_row(65), 512);
        // The case that shears an image if it is missed: 300 × 4 = 1200, not a
        // multiple of 256.
        assert_eq!(padded_bytes_per_row(300), 1280);
        assert_eq!(padded_bytes_per_row(512), 2048);
    }

    /// The unpack has to drop exactly the padding and nothing else, or every row
    /// after the first is offset and the image shears.
    #[test]
    fn unpacking_drops_the_row_padding() {
        let width = 2;
        let height = 2;
        let padded = padded_bytes_per_row(width);
        let mut bytes = vec![0u8; padded as usize * height as usize];
        // Two rows of two RGBA texels, with junk in the padding.
        bytes[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes[8..16].copy_from_slice(&[99; 8]);
        let second = padded as usize;
        bytes[second..second + 8].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        let rgba = unpack(&bytes, width, height, padded, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(rgba, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    /// Metal hands out `Bgra8Unorm`. A thumbnail with red and blue swapped is the
    /// single most likely way for this file to be wrong and still produce an image.
    #[test]
    fn a_bgra_capture_comes_back_in_rgba_order() {
        let padded = padded_bytes_per_row(1);
        let mut bytes = vec![0u8; padded as usize];
        // #FF9E9E — the one non-yellow sticky on the reference board — as BGRA.
        bytes[0..4].copy_from_slice(&[0x9E, 0x9E, 0xFF, 0xFF]);

        let rgba = unpack(&bytes, 1, 1, padded, wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(rgba, vec![0xFF, 0x9E, 0x9E, 0xFF]);

        let straight = unpack(&bytes, 1, 1, padded, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(straight, vec![0x9E, 0x9E, 0xFF, 0xFF]);
    }

    /// A truncated readback must produce a full-sized image rather than an
    /// `RgbaImage::from_raw` that returns `None` two calls later.
    #[test]
    fn a_short_readback_is_padded_rather_than_truncated() {
        let padded = padded_bytes_per_row(4);
        let bytes = vec![7u8; padded as usize]; // one row where two were asked for
        let rgba = unpack(&bytes, 4, 2, padded, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(rgba.len(), 4 * 4 * 2);
    }

    #[test]
    fn a_thumbnail_fits_inside_its_longest_edge_and_keeps_its_shape() {
        // The reference board: 41282 × 17515.
        let (w, h) = fitted_size((41_282.0, 17_515.0), THUMBNAIL_SIZE).unwrap();
        assert_eq!(w, THUMBNAIL_SIZE);
        assert!(h < THUMBNAIL_SIZE);
        let ratio = f64::from(w) / f64::from(h);
        assert!((ratio - 41_282.0 / 17_515.0).abs() < 0.02, "aspect drifted: {ratio}");

        // A tall board fits the other way round.
        let (w, h) = fitted_size((400.0, 4_000.0), THUMBNAIL_SIZE).unwrap();
        assert_eq!(h, THUMBNAIL_SIZE);
        assert!(w < THUMBNAIL_SIZE);
    }

    /// Never upscale: a 100px board rendered into a 512px thumbnail would be four
    /// times blurrier than the board actually is.
    #[test]
    fn a_small_board_is_captured_at_one_to_one() {
        assert_eq!(fitted_size((120.0, 80.0), THUMBNAIL_SIZE), Some((120, 80)));
    }

    #[test]
    fn a_board_with_no_extent_has_no_capture_size() {
        assert_eq!(fitted_size((0.0, 0.0), THUMBNAIL_SIZE), None);
        assert_eq!(fitted_size((f64::NAN, 10.0), THUMBNAIL_SIZE), None);
        assert_eq!(fitted_size((-5.0, 10.0), THUMBNAIL_SIZE), None);
    }

    /// A full-size export of the reference board would ask for gigabytes. The clamp
    /// is what makes "Export PNG" on a huge board slow rather than fatal.
    #[test]
    fn an_enormous_export_is_clamped_rather_than_attempted() {
        let (w, h) = fitted_size((200_000.0, 100_000.0), u32::MAX).unwrap();
        assert!(w <= MAX_EDGE && h <= MAX_EDGE, "{w}x{h}");
    }
}
