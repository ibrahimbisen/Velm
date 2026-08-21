//! Vellum's GPU renderer: everything a board contains, in a handful of draw calls.
//!
//! No windowing and no surface. [`Renderer::new`] takes a `wgpu::Device` and a target
//! format; [`Renderer::draw`] records into a render pass the caller opened. The same
//! code therefore drives the window, an offscreen texture for a PNG export or a board
//! thumbnail, and the pixel tests in `tests/pixels.rs` — and, per
//! `docs/01-architecture.md` §1, an eventual `wasm32` + WebGPU viewer is a recompile
//! rather than a port.
//!
//! # Five pipelines
//!
//! | Pipeline | Draws | Consumes |
//! |---|---|---|
//! | [`QuadInstance`] | Rounded rectangles with borders | stickies, frames, cards, chrome |
//! | [`ShapeInstance`] | Analytic shapes | [`vellum_shapes::SdfParams`] |
//! | [`ImageInstance`] | Textured quads with UV sub-rects | [`TextureManager`] |
//! | [`GlyphInstance`] | Glyph quads | [`vellum_text::GlyphImage`] via [`GlyphAtlas`] |
//! | [`MeshVertex`] | Indexed triangles | `vellum-ink`, `vellum-shapes`, `vellum-connect` |
//!
//! Each is instanced, so 44 stickies and 219 ink strokes cost one draw call each, not
//! 263. [`DrawList`] preserves the caller's paint order and coalesces consecutive
//! draws that can share a pipeline and a bind; [`DrawStats::draw_calls`] is the number
//! to watch.
//!
//! # A sixth pass, kept separate: translucent chrome
//!
//! [`GlassRenderer`] implements the "liquid glass" material of
//! `docs/05-design-language.md` §3a — a quarter-resolution blur of the canvas behind
//! each floating panel, a tint, a saturation lift, a hairline and a specular catch on
//! the top edge. It is **not** one of [`Renderer`]'s pipelines and does not share its
//! [`DrawList`], for a structural reason: it reads what has already been drawn, so it
//! runs between two render passes rather than inside one, and it needs the canvas in
//! a texture rather than on the swapchain. A caller that floats nothing over the
//! board — a PNG export, a thumbnail — never constructs it and pays nothing for it.
//!
//! # Coordinates
//!
//! Every position handed to this crate is `f32` **in the current view's space**, and
//! for board content that space is camera-relative world pixels — `f64` world
//! coordinates with the camera's origin already subtracted, by
//! [`Camera::to_camera_relative`](vellum_scene::Camera::to_camera_relative). The
//! reference board is 41282 × 17515 px and `f32` quantises coordinates that large
//! coarsely enough that a naive cast visibly jitters as you pan at deep zoom;
//! `vellum_scene::camera` measures it. Nothing here ever sees an absolute world
//! coordinate, and [`View`] is what says which space a run of draws is in.
//!
//! # Colour and blending
//!
//! The public surface is straight (non-premultiplied) [`Rgba`], matching
//! `vellum_scene::RenderPayload`. Every shader premultiplies on output and every
//! pipeline blends premultiplied, because that is the only form in which a fill
//! composited under a border, and a filtered or mipmapped texture, are both correct.
//!
//! # Memory
//!
//! `docs/01-architecture.md` §3 budgets 400 MB idle against 1.5 GB of decoded images
//! on the reference board alone. [`TextureManager`] downscales on upload, builds mip
//! chains and evicts stalest-and-furthest-first; [`GlyphAtlas`] packs onto shelves and
//! repacks when full. Both are required for the reference board to open, not
//! optimisations.
//!
//! Eviction alone is not enough, and the reference board proves it: at fit-zoom every
//! image is on screen, so nothing is stale and residency measured 785 MB against a
//! 268 MB budget with nothing to throw away. [`Renderer::prepare`] therefore measures
//! the size each image is *drawn* at and [`TextureManager::resolve_detail`] matches
//! its resolution to it — rebuilding a texture from its own mip chain on the GPU when
//! it is sharper than the screen can show, and dropping it for re-upload when it is
//! not sharp enough. [`DetailPolicy`] holds the thresholds; `crate::detail` explains
//! them, including why block compression is not also used.
//!
//! # A frame
//!
//! ```no_run
//! use vellum_render::{DrawList, QuadInstance, Renderer, Rgba, View};
//! use vellum_scene::{Camera, ScreenSize};
//!
//! # fn frame(device: &wgpu::Device, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass<'_>) {
//! let mut renderer = Renderer::new(device, wgpu::TextureFormat::Bgra8Unorm);
//! let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
//!
//! let mut list = DrawList::new();
//! list.view(View::board(&camera));
//! list.push_quad(
//!     QuadInstance::solid(
//!         camera.to_camera_relative(vellum_scene::WorldPoint::new(41_000.0, 17_000.0)),
//!         [199.0, 228.0],
//!         Rgba::from_hex(0xff_f79e),
//!     )
//!     .with_corner_radius(8.0),
//! );
//!
//! renderer.begin_frame();
//! renderer.prepare(device, queue, &list);
//! renderer.draw(pass);
//! # }
//! ```

mod backdrop;
mod buffer;
mod pipeline;

pub mod atlas;
pub mod color;
pub mod detail;
pub mod error;
pub mod glass;
pub mod image;
pub mod list;
pub mod mesh;
pub mod quad;
pub mod renderer;
pub mod shape;
pub mod text;
pub mod texture;
pub mod view;

pub use atlas::{AtlasConfig, AtlasKind, AtlasPage, AtlasSlot, GlyphAtlas};
pub use pipeline::BOARD_SAMPLES;
pub use color::Rgba;
pub use detail::DetailPolicy;
pub use error::RenderError;
pub use glass::{
    Backdrop, GlassMaterial, GlassMode, GlassPanel, GlassQuality, GlassRenderer, GlassStats,
};
pub use image::ImageInstance;
pub use list::{DrawList, DrawStats};
pub use mesh::{MeshBatch, MeshTransform, MeshVertex};
pub use quad::QuadInstance;
pub use renderer::Renderer;
pub use shape::{PolygonArena, ShapeInstance, ShapeStyle};
pub use text::GlyphInstance;
pub use texture::{ImageSource, Refinement, TextureBudget, TextureId, TextureManager, UvRect};
pub use view::View;
