//! Translucent chrome — the "liquid glass" material of
//! `docs/05-design-language.md` §3a.
//!
//! For an infinite-canvas app this is not decoration. A floating toolbar occludes the
//! board, and that is a real cost paid on every frame the user is working; a
//! translucent one gives most of it back. §2 of the same document rules out
//! *decorative* glassmorphism in the same breath, and the distinction is enforced
//! here by what this module refuses to do: it blurs what is genuinely behind a
//! floating surface, it never stacks, and it turns itself off the moment the OS asks
//! it to.
//!
//! # What a panel is made of
//!
//! Five layers, in this order, all in one fragment:
//!
//! 1. the canvas behind it, blurred at quarter resolution ([`crate::backdrop`]),
//! 2. a saturation lift, so a wall of yellow stickies stays yellow instead of beige,
//! 3. the tint — `bone` at ~72% in light, ~68% in dark,
//! 4. a 1px `frost` hairline, drawn inside the edge like every other border here,
//! 5. **a 1px specular line along the top edge only, at ~14% white.**
//!
//! That fifth layer is the one worth arguing about. Without it the material is a
//! blurred rectangle and reads as a smear; with it the top edge catches light and the
//! panel reads as a physical pane sitting above the board. It costs one `mix` and an
//! analytic normal, and it is the difference between this and every generic frosted
//! panel.
//!
//! # Cost
//!
//! §3a budgets **under 0.5 ms per frame**, and `tests/glass_budget.rs` fails the
//! build if a realistic set of panels at 1440 × 900 exceeds it. Three things get it
//! there: only the rectangles behind panels are touched, they are touched at 1/16 the
//! fragments, and a panel over a board that has not changed costs **nothing at all**
//! — [`GlassStats::is_free`] is the assertion, not a hope.
//!
//! If it ever cannot be met, [`GlassQuality::Flat`] drops the blur and keeps the
//! tint. §3a is explicit that this is the trade to make: the frame rate is not
//! negotiable and the material is.
//!
//! # The frame this needs
//!
//! Unlike [`crate::Renderer`], this pass has to read what was already drawn, so the
//! canvas must land in a texture rather than straight on the swapchain:
//!
//! ```no_run
//! # use vellum_render::{Backdrop, DrawList, GlassPanel, GlassRenderer, Renderer, Rgba};
//! # fn frame(
//! #     device: &wgpu::Device,
//! #     queue: &wgpu::Queue,
//! #     renderer: &mut Renderer,
//! #     glass: &mut GlassRenderer,
//! #     canvas: &wgpu::TextureView,
//! #     surface: &wgpu::TextureView,
//! #     board: &DrawList,
//! #     chrome: &DrawList,
//! #     panels: &[GlassPanel],
//! #     revision: u64,
//! # ) {
//! let mut encoder = device.create_command_encoder(&Default::default());
//!
//! // 1. The board, into an offscreen texture.
//! renderer.prepare(device, queue, board);
//! # let mut pass = encoder.begin_render_pass(&Default::default());
//! # renderer.draw(&mut pass);
//! # drop(pass);
//!
//! // 2. The backdrop passes, outside any render pass of the caller's.
//! let stats = glass.prepare(
//!     device,
//!     queue,
//!     &mut encoder,
//!     &Backdrop { texture: canvas, width: 1440, height: 900, revision },
//!     panels,
//! );
//! debug_assert!(stats.panels == panels.len());
//!
//! // 3. The canvas, then the glass, then whatever sits on the glass.
//! renderer.prepare(device, queue, chrome);
//! # let mut pass = encoder.begin_render_pass(&Default::default());
//! glass.draw(&mut pass);
//! renderer.draw(&mut pass);
//! # drop(pass);
//! queue.submit(Some(encoder.finish()));
//! # }
//! ```
//!
//! `revision` is whatever the caller increments when the board's pixels change — a
//! camera move, an edit, an animation frame. Getting it wrong in the safe direction
//! (bumping too often) costs the blur; getting it wrong the other way shows a stale
//! backdrop, so it should be derived from the same state that decides whether to
//! redraw the canvas at all.

use crate::backdrop::{BackdropCache, Region, blur_reach};
use crate::buffer::GrowableBuffer;
use crate::color::Rgba;
use crate::pipeline::{self, PipelineDescriptor};
use crate::view::{View, Views};
use vellum_scene::ScreenSize;

pub use crate::backdrop::Backdrop;

/// How much resolution the blur is allowed.
///
/// Two working settings and one escape hatch. The escape hatch is not a placeholder:
/// §3a says in as many words that if the budget cannot be met the material drops to a
/// flat tint rather than the frame rate being sacrificed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlassQuality {
    /// Quarter resolution per axis — 1/16 the fragments. The specified setting, and
    /// wide enough that the remaining detail is well below what the blur keeps.
    #[default]
    Quarter,
    /// Half resolution per axis. Four times the capture cost of [`Self::Quarter`] for
    /// a difference visible only if you go looking; here because a fractional display
    /// scale can make the quarter-resolution grid land awkwardly on very small
    /// panels, not because it is generally better.
    Half,
    /// No backdrop at all: the tint alone, blended flat over whatever is underneath.
    /// Costs one draw call and no passes.
    Flat,
}

impl GlassQuality {
    /// The reduction factor at a scale factor of 1, or `None` when there is no
    /// backdrop to reduce. See [`GlassRenderer::set_scale_factor`] for why the
    /// working factor is a multiple of this.
    pub const fn factor(self) -> Option<u32> {
        match self {
            Self::Quarter => Some(4),
            Self::Half => Some(2),
            Self::Flat => None,
        }
    }

    /// The reduction factor on a display of `scale` physical pixels per point.
    ///
    /// The blur's width is fixed in *atlas texels* — see `crate::backdrop` — so the
    /// reduction factor is what decides how wide it is on screen. Holding the factor
    /// constant across displays would make the material half as blurred on a Retina
    /// panel as on a 1x one, which is not a smaller version of the same material but
    /// a different one. Scaling it keeps the blur a fixed number of *points* and, as
    /// a side effect, keeps the atlas and the per-frame cost the same size too.
    pub fn factor_at(self, scale: f32) -> Option<u32> {
        let steps = if scale.is_finite() { scale.round().max(1.0) as u32 } else { 1 };
        // Always even, because the capture's box filter is `factor / 2` bilinear taps
        // per axis and half a tap does not exist.
        self.factor().map(|factor| factor * steps)
    }
}

/// Whether the material is permitted at all.
///
/// A hard switch rather than a fade, because that is what it is for: macOS *Reduce
/// Transparency* and *Increase Contrast* are accessibility settings, and a user who
/// turns one on wants the panel opaque on the next frame, not a cheaper blur.
/// §3a also requires this to be detected live rather than read once at launch, so the
/// caller is expected to set it whenever the OS reading changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlassMode {
    /// The material as specified.
    #[default]
    Translucent,
    /// Fully opaque fill, no blur, no specular. Every panel becomes its tint colour
    /// at full strength, and no backdrop work is encoded at all.
    Opaque,
}

/// The material a floating surface is made of.
///
/// Colours are the caller's: `docs/05-design-language.md` §1 requires every colour to
/// resolve through a palette token, and the palette lives in `vellum-ui`, which this
/// crate deliberately does not depend on. The *numbers* — the saturation lift, the
/// 14% specular, the 1px widths — are properties of the material rather than of the
/// theme, so they have defaults here and are stated in §3a.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassMaterial {
    /// `rgb` is the tint colour — `bone` in both modes. **`a` is how much of it is
    /// laid over the blurred backdrop**, not the panel's own alpha: 0.72 light, 0.68
    /// dark. An alpha of 1 makes the panel opaque and skips the blur entirely, which
    /// is exactly what the Reduce Transparency fallback wants.
    pub tint: Rgba,
    /// Multiplier on the backdrop's distance from grey. 1.0 leaves it alone.
    pub saturation: f32,
    /// The hairline. `frost`, per §3.
    pub border: Rgba,
    pub border_width: f32,
    /// The specular catch along the **top edge only**. White at about 14%.
    pub highlight: Rgba,
    pub highlight_width: f32,
    /// Scales the whole panel, for a surface fading in. Not the translucency.
    pub opacity: f32,
}

impl GlassMaterial {
    /// A *slight* lift, as §3a asks for. Enough that a wall of `#fff79e` stickies
    /// still reads as yellow through the toolbar; not so much that the board turns
    /// into a poster behind it. Anything past about 1.5 starts to posterise, which
    /// reads as a filter rather than as a material.
    pub const SATURATION: f32 = 1.3;

    /// The specular's strength: §3a's "~14% white".
    pub const HIGHLIGHT_ALPHA: f32 = 0.14;

    /// The material over `tint`, with the specified edge treatment.
    ///
    /// `tint`'s alpha carries the mode's opacity — `Rgba::from_hex(0xf4_f5f6).with_alpha(0.72)`
    /// for light, the dark `bone` at 0.68 for dark.
    pub fn new(tint: Rgba, border: Rgba) -> Self {
        Self {
            tint,
            saturation: Self::SATURATION,
            border,
            border_width: 1.0,
            highlight: Rgba::WHITE.with_alpha(Self::HIGHLIGHT_ALPHA),
            highlight_width: 1.0,
            opacity: 1.0,
        }
    }

    /// The opaque fallback: `fill` at full strength, a hairline, and no specular.
    ///
    /// Used for Reduce Transparency and high contrast, and for any surface that is
    /// not over the canvas — §3a's *"glass never stacks"*. Costs no backdrop work,
    /// because [`Self::needs_backdrop`] is false for it.
    pub fn opaque(fill: Rgba, border: Rgba) -> Self {
        Self {
            tint: fill.with_alpha(1.0),
            saturation: 1.0,
            border,
            border_width: 1.0,
            // Dropped rather than kept: a specular on an opaque panel is exactly the
            // "3D-ish depth" §2 rules out, and the material it belonged to is gone.
            highlight: Rgba::TRANSPARENT,
            highlight_width: 0.0,
            opacity: 1.0,
        }
    }

    pub fn with_saturation(mut self, saturation: f32) -> Self {
        self.saturation = saturation;
        self
    }

    pub fn with_border(mut self, color: Rgba, width: f32) -> Self {
        self.border = color;
        self.border_width = width;
        self
    }

    pub fn with_highlight(mut self, color: Rgba, width: f32) -> Self {
        self.highlight = color;
        self.highlight_width = width;
        self
    }

    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    /// The opaque form of this material — what Reduce Transparency turns it into.
    pub fn flattened(self) -> Self {
        Self::opaque(self.tint, self.border)
            .with_border(self.border, self.border_width)
            .with_opacity(self.opacity)
    }

    /// Whether this material has anything to see through. A fully opaque tint, or a
    /// panel faded out entirely, needs no blur and is charged for none.
    pub fn needs_backdrop(&self) -> bool {
        self.tint.a < 1.0 && self.opacity > 0.0
    }
}

/// One floating surface.
///
/// Screen space, physical pixels — the same space [`View::screen`] describes, which
/// is also the space `vellum-ui` lays chrome out in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassPanel {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// CSS order: top-left, top-right, bottom-right, bottom-left. §2 holds these to
    /// 4–6px; translucency does not license pillowy corners.
    pub corner_radii: [f32; 4],
    pub material: GlassMaterial,
}

impl GlassPanel {
    pub fn new(origin: [f32; 2], size: [f32; 2], material: GlassMaterial) -> Self {
        Self { origin, size, corner_radii: [0.0; 4], material }
    }

    pub fn with_corner_radius(mut self, radius: f32) -> Self {
        self.corner_radii = [radius; 4];
        self
    }

    pub fn with_corner_radii(mut self, radii: [f32; 4]) -> Self {
        self.corner_radii = radii;
        self
    }
}

/// What one frame of the material cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GlassStats {
    pub panels: usize,
    /// Regions whose blurred copy was rebuilt. Zero over a board that did not change.
    pub refreshed: usize,
    /// Render passes encoded. Three when anything was rebuilt — one capture and two
    /// Kawase — and zero otherwise, however many panels there are.
    pub passes: usize,
    /// Quarter-resolution texels written across those passes. The number the budget
    /// is actually about.
    pub texels: u64,
    /// Buffer writes. Counted because "the cost is zero" has to mean uploads too.
    pub uploads: usize,
}

impl GlassStats {
    /// True when the frame did no GPU work whatsoever — no passes and no uploads.
    ///
    /// §3a: *"while a panel is idle over a static board, the cost is zero."* This is
    /// that sentence in a form a test can fail on.
    pub fn is_free(&self) -> bool {
        self.passes == 0 && self.uploads == 0
    }
}

/// One panel's composite draw. Mirrored by `GlassInstance` in `shaders/glass.wgsl`
/// and by [`ATTRIBUTES`]; all three change together.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct GlassInstance {
    origin: [f32; 2],
    size: [f32; 2],
    /// The panel's own rectangle inside the backdrop atlas: `min.xy, max.zw`.
    uv: [f32; 4],
    tint: Rgba,
    border: Rgba,
    highlight: Rgba,
    corner_radii: [f32; 4],
    /// `border_width, highlight_width, saturation, fill_alpha`.
    style: [f32; 4],
    /// `sample_backdrop, opacity, padding, padding`.
    flags: [f32; 4],
}

const ATTRIBUTES: [wgpu::VertexAttribute; 9] = wgpu::vertex_attr_array![
    0 => Float32x2,  // origin
    1 => Float32x2,  // size
    2 => Float32x4,  // uv
    3 => Float32x4,  // tint
    4 => Float32x4,  // border
    5 => Float32x4,  // highlight
    6 => Float32x4,  // corner_radii
    7 => Float32x4,  // style
    8 => Float32x4,  // flags
];

fn layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<GlassInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRIBUTES,
    }
}

/// The glass pass: the backdrop cache, the composite pipeline, and the frame-to-frame
/// state that lets an unchanged frame do nothing.
///
/// Deliberately separate from [`crate::Renderer`] rather than folded into it. The two
/// have different lifecycles — this one has to run *between* two render passes,
/// because it reads what the first drew — and a caller that never floats anything
/// over the canvas (a PNG export, a board thumbnail) should not pay for three
/// pipelines it will not use against a 300 ms cold-start budget.
pub struct GlassRenderer {
    views: Views,
    viewport: Option<(u32, u32)>,
    composite: wgpu::RenderPipeline,
    instances: GrowableBuffer,
    /// The instance array currently on the GPU. Compared against, so a frame that
    /// changed nothing uploads nothing.
    uploaded: Vec<GlassInstance>,
    staged: Vec<GlassInstance>,
    backdrop: BackdropCache,
    regions: Vec<Region>,
    /// Which region, if any, each panel's backdrop came from.
    region_of: Vec<Option<usize>>,
    mode: GlassMode,
    quality: GlassQuality,
    scale: f32,
}

impl GlassRenderer {
    /// Brings up the material for a target of `format` — the same non-sRGB `Unorm`
    /// format [`crate::Renderer::new`] documents, for the same reason.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let views = Views::new(device);
        let backdrop = BackdropCache::new(device);
        let composite = pipeline::build(
            device,
            &PipelineDescriptor {
                label: "vellum-glass",
                source: pipeline::shader_source!("shaders/glass.wgsl"),
                vertex_entry: "vs_glass",
                fragment_entry: "fs_glass",
                format,
                buffers: &[layout()],
                bind_group_layouts: &[views.layout(), backdrop.source_layout()],
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                // Deliberately **not** multisampled, unlike the five board pipelines: glass
                // is composited onto the window, and the swapchain has one sample. Its own
                // edges are analytic anyway (`glass.wgsl` uses `coverage`), so there is
                // nothing MSAA would add here even if the target could take it.
                samples: 1,
            },
        );

        Self {
            views,
            viewport: None,
            composite,
            instances: GrowableBuffer::new(
                device,
                "vellum-glass-instances",
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                // A screenful of chrome is under a dozen surfaces; this is room for
                // thirty-two before the first and only resize.
                32 * size_of::<GlassInstance>() as wgpu::BufferAddress,
            ),
            uploaded: Vec::new(),
            staged: Vec::new(),
            backdrop,
            regions: Vec::new(),
            region_of: Vec::new(),
            mode: GlassMode::default(),
            quality: GlassQuality::default(),
            scale: 1.0,
        }
    }

    pub fn mode(&self) -> GlassMode {
        self.mode
    }

    /// Switches the material on or off. Call it whenever the OS reading changes —
    /// §3a requires Reduce Transparency to be reacted to live, not read at launch.
    ///
    /// Switching to [`GlassMode::Opaque`] drops every cached backdrop, so turning it
    /// back on cannot show a copy of the board as it looked before.
    pub fn set_mode(&mut self, mode: GlassMode) {
        if self.mode != mode {
            self.mode = mode;
            self.backdrop.forget();
        }
    }

    pub fn quality(&self) -> GlassQuality {
        self.quality
    }

    /// Changes the blur's resolution, or drops it. See [`GlassQuality::Flat`].
    pub fn set_quality(&mut self, quality: GlassQuality) {
        if self.quality != quality {
            self.quality = quality;
            self.backdrop.forget();
        }
    }

    pub fn scale_factor(&self) -> f32 {
        self.scale
    }

    /// Tells the material how many physical pixels a point is — `winit`'s
    /// `Window::scale_factor`.
    ///
    /// Without it the blur would be a fixed number of *pixels* and would therefore be
    /// half as wide, in points, on a Retina display as on a 1x one. Panels and their
    /// radii are already handed over in physical pixels by the caller; this is the
    /// one number the material cannot infer from them, because a 44-point toolbar and
    /// an 88-pixel one look identical from here.
    ///
    /// Costs nothing: a higher scale raises the reduction factor rather than the
    /// amount of work, so the atlas and the pass cost stay where they were.
    pub fn set_scale_factor(&mut self, scale: f32) {
        if self.scale != scale {
            self.scale = scale;
            self.backdrop.forget();
        }
    }

    /// The atlas the blurred backdrops are packed into, in texels. One square
    /// texture, and it only ever grows.
    pub fn backdrop_atlas_size(&self) -> u32 {
        self.backdrop.atlas_size()
    }

    /// Refreshes whatever backdrops are stale and records the composite draw.
    ///
    /// Encodes its own render passes into `encoder`, so it must be called **outside**
    /// any pass of the caller's, and after the canvas has been drawn into
    /// `backdrop.texture`.
    ///
    /// Returns without touching the GPU at all when nothing has changed — see
    /// [`GlassStats::is_free`].
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        backdrop: &Backdrop<'_>,
        panels: &[GlassPanel],
    ) -> GlassStats {
        let mut stats = GlassStats { panels: panels.len(), ..Default::default() };
        let viewport = (backdrop.width, backdrop.height);
        if self.viewport != Some(viewport) {
            self.views.write(
                device,
                queue,
                &[View::screen(ScreenSize::new(
                    f64::from(viewport.0),
                    f64::from(viewport.1),
                ))],
            );
            self.viewport = Some(viewport);
            stats.uploads += 1;
        }

        let factor = match self.mode {
            GlassMode::Translucent => self.quality.factor_at(self.scale),
            GlassMode::Opaque => None,
        };

        // Which panels want a backdrop, and the rectangle each needs captured.
        self.regions.clear();
        self.region_of.clear();
        if let Some(factor) = factor {
            let margin = blur_reach(factor);
            for panel in panels {
                if panel.material.needs_backdrop() {
                    self.region_of.push(Some(self.regions.len()));
                    self.regions
                        .push(Region::around(panel.origin, panel.size, margin, factor));
                } else {
                    self.region_of.push(None);
                }
            }
        } else {
            self.backdrop.forget();
            self.region_of.resize(panels.len(), None);
        }

        if let Some(factor) = factor {
            let refresh =
                self.backdrop
                    .refresh(device, queue, encoder, backdrop, &self.regions, factor);
            stats.refreshed = refresh.refreshed;
            stats.passes = refresh.passes;
            stats.texels = refresh.texels;
            stats.uploads += refresh.uploads;
        }

        self.stage(panels);
        if self.staged != self.uploaded {
            self.instances
                .write(device, queue, bytemuck::cast_slice(&self.staged));
            self.uploaded.clear();
            self.uploaded.extend_from_slice(&self.staged);
            stats.uploads += 1;
        }
        stats
    }

    /// Builds this frame's composite instances into [`Self::staged`].
    fn stage(&mut self, panels: &[GlassPanel]) {
        let atlas = self.backdrop.atlas_size().max(1) as f32;
        let factor = self.quality.factor_at(self.scale).unwrap_or(1) as f32;

        self.staged.clear();
        for (index, panel) in panels.iter().enumerate() {
            let material = match self.mode {
                GlassMode::Translucent => panel.material,
                GlassMode::Opaque => panel.material.flattened(),
            };

            // A panel gets the material only if it asked for it, the quality setting
            // allows it, and the atlas actually had room. The last one is why this is
            // resolved per panel rather than per frame: a set of surfaces too large
            // to pack degrades to a flat tint instead of vanishing.
            let slot = self
                .region_of
                .get(index)
                .copied()
                .flatten()
                .filter(|_| material.needs_backdrop())
                .and_then(|region| self.backdrop.slot(region).map(|slot| (region, slot)));

            let (uv, sample) = match slot {
                Some((region, slot)) => {
                    let region = self.regions[region];
                    let u = |v: f32, origin: i32, base: u32| {
                        (base as f32 + (v - origin as f32) / factor) / atlas
                    };
                    (
                        [
                            u(panel.origin[0], region.x, slot[0]),
                            u(panel.origin[1], region.y, slot[1]),
                            u(panel.origin[0] + panel.size[0], region.x, slot[0]),
                            u(panel.origin[1] + panel.size[1], region.y, slot[1]),
                        ],
                        1.0,
                    )
                }
                // No backdrop: the fill is the tint itself, and its own alpha does
                // the blending against whatever is already in the target.
                None => ([0.0; 4], 0.0),
            };

            self.staged.push(GlassInstance {
                origin: panel.origin,
                size: panel.size,
                uv,
                tint: material.tint,
                border: material.border,
                highlight: material.highlight,
                corner_radii: panel.corner_radii,
                style: [
                    material.border_width,
                    material.highlight_width,
                    material.saturation,
                    if sample > 0.5 { 1.0 } else { material.tint.a },
                ],
                flags: [sample, material.opacity, 0.0, 0.0],
            });
        }
    }

    /// Draws the panels recorded by the last [`Self::prepare`] into `pass`.
    ///
    /// One draw call for every panel on screen. The pass belongs to the caller, so
    /// this goes wherever the chrome's z-order puts it: after the canvas, before
    /// whatever sits *on* the glass.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.uploaded.is_empty() {
            return;
        }
        pass.set_pipeline(&self.composite);
        pass.set_bind_group(0, self.views.bind_group(), &[self.views.offset(0)]);
        pass.set_bind_group(1, self.backdrop.result_bind_group(), &[]);
        pass.set_vertex_buffer(0, self.instances.buffer().slice(..));
        pass.draw(0..4, 0..self.uploaded.len() as u32);
    }

    /// How many panels the last [`Self::prepare`] recorded.
    pub fn panels(&self) -> usize {
        self.uploaded.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BONE: Rgba = Rgba::new(0.957, 0.961, 0.965, 1.0);
    const FROST: Rgba = Rgba::new(0.867, 0.886, 0.898, 1.0);

    /// The layout is stated in three places — this struct, [`ATTRIBUTES`] and the
    /// WGSL. A mismatch garbles the material rather than failing, so pin every
    /// offset the way [`crate::quad`] does.
    #[test]
    fn the_instance_layout_matches_the_vertex_attributes() {
        assert_eq!(size_of::<GlassInstance>(), 128);
        let offsets: Vec<_> = ATTRIBUTES.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 16, 32, 48, 64, 80, 96, 112]);
        for (i, attribute) in ATTRIBUTES.iter().enumerate() {
            assert_eq!(attribute.shader_location, i as u32);
        }
        assert_eq!(layout().array_stride, 128);
    }

    /// The material's own numbers, as §3a states them. They are here rather than in
    /// the palette because they describe the material, not the theme.
    #[test]
    fn the_material_carries_the_specified_edge_treatment() {
        let material = GlassMaterial::new(BONE.with_alpha(0.72), FROST);
        assert_eq!(material.tint.a, 0.72, "the tint's alpha is the mix, not the panel's");
        assert_eq!(material.border_width, 1.0);
        assert_eq!(material.highlight, Rgba::WHITE.with_alpha(0.14));
        assert_eq!(material.highlight_width, 1.0);
        assert!(material.saturation > 1.0, "colour beneath must stay alive");
        assert!(material.needs_backdrop());
    }

    /// The Reduce Transparency fallback. It has to be *free*, not merely opaque:
    /// a fully opaque tint means no region is requested at all.
    #[test]
    fn the_opaque_fallback_needs_no_backdrop() {
        let opaque = GlassMaterial::opaque(BONE, FROST);
        assert_eq!(opaque.tint.a, 1.0);
        assert!(opaque.highlight.is_invisible(), "no specular on an opaque panel");
        assert_eq!(opaque.saturation, 1.0);
        assert!(!opaque.needs_backdrop());
    }

    /// Flattening a translucent material has to reach exactly the same place as
    /// building the opaque one directly, or the two routes to the fallback — the
    /// global switch and a per-surface `Backing::Panel` — would look different.
    #[test]
    fn flattening_a_material_is_the_opaque_fallback() {
        let glass = GlassMaterial::new(BONE.with_alpha(0.68), FROST);
        let flat = glass.flattened();
        assert_eq!(flat, GlassMaterial::opaque(BONE.with_alpha(0.68), FROST));
        assert!(!flat.needs_backdrop());
        assert_eq!(flat.border, FROST);
        assert_eq!(flat.border_width, glass.border_width);
    }

    /// A panel faded out is not worth blurring behind, and a fade-in animation would
    /// otherwise re-blur on every frame of a 160 ms transition for pixels nobody sees.
    #[test]
    fn a_fully_faded_panel_needs_no_backdrop() {
        let material = GlassMaterial::new(BONE.with_alpha(0.72), FROST).with_opacity(0.0);
        assert!(!material.needs_backdrop());
        assert!(material.with_opacity(0.01).needs_backdrop());
    }

    #[test]
    fn quality_maps_onto_a_reduction_factor() {
        assert_eq!(GlassQuality::default(), GlassQuality::Quarter);
        assert_eq!(GlassQuality::Quarter.factor(), Some(4));
        assert_eq!(GlassQuality::Half.factor(), Some(2));
        assert_eq!(GlassQuality::Flat.factor(), None);
    }

    /// The blur is a fixed number of atlas texels, so the reduction factor is what
    /// decides its width in points. Doubling the display scale must double the
    /// factor, or the material is half as blurred on a Retina panel.
    #[test]
    fn the_reduction_factor_tracks_the_display_scale() {
        assert_eq!(GlassQuality::Quarter.factor_at(1.0), Some(4));
        assert_eq!(GlassQuality::Quarter.factor_at(2.0), Some(8));
        assert_eq!(GlassQuality::Half.factor_at(2.0), Some(4));
        assert_eq!(GlassQuality::Flat.factor_at(2.0), None);
        // Every factor stays even, because the capture's box filter is `factor / 2`
        // bilinear taps per axis.
        for scale in [1.0, 1.5, 2.0, 3.0] {
            let factor = GlassQuality::Quarter.factor_at(scale).expect("a factor");
            assert_eq!(factor % 2, 0, "scale {scale} gave an odd factor {factor}");
        }
    }

    /// A scale factor of zero, or a NaN out of a display that has just been
    /// unplugged, must not produce a reduction factor of zero — which would divide
    /// the region arithmetic by nothing at all.
    #[test]
    fn a_nonsense_scale_falls_back_to_one() {
        for bad in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            assert_eq!(GlassQuality::Quarter.factor_at(bad), Some(4), "scale {bad}");
        }
    }

    #[test]
    fn a_frame_that_encoded_nothing_is_free() {
        assert!(GlassStats { panels: 4, ..Default::default() }.is_free());
        assert!(!GlassStats { passes: 3, ..Default::default() }.is_free());
        assert!(!GlassStats { uploads: 1, ..Default::default() }.is_free());
    }

    #[test]
    fn a_panel_defaults_to_square_corners_and_takes_the_scales_radii() {
        let panel = GlassPanel::new(
            [16.0, 120.0],
            [56.0, 320.0],
            GlassMaterial::new(BONE.with_alpha(0.72), FROST),
        );
        assert_eq!(panel.corner_radii, [0.0; 4]);
        assert_eq!(panel.with_corner_radius(6.0).corner_radii, [6.0; 4]);
        assert_eq!(
            panel.with_corner_radii([6.0, 6.0, 0.0, 0.0]).corner_radii,
            [6.0, 6.0, 0.0, 0.0]
        );
    }
}
