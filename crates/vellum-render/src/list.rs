//! The frame's draw list, and the batching that turns it into a handful of draw
//! calls.
//!
//! # Ordering is the caller's, batching is ours
//!
//! Board content is blended, so paint order is meaning: a sticky's text must be drawn
//! after the sticky. The list therefore preserves the order things are pushed in —
//! which is the back-to-front order the scene layer's fractional z-index already
//! produces — and coalesces *consecutive* draws that can share a pipeline and a bind
//! into one call.
//!
//! That gives the target of a handful of draws per frame without any reordering that
//! could change what the user sees, because a board is naturally run-structured: all
//! the stickies, then all the shapes, then all the ink, then all the text. Where a
//! caller interleaves kinds it pays one draw per switch, which is the honest cost of
//! having asked for that order.
//!
//! # Views
//!
//! A frame draws in more than one space — camera-relative world pixels for the board,
//! physical pixels for chrome and for text. [`DrawList::view`] switches between them,
//! and a switch also ends the current batch, because the view is a dynamic offset into
//! the uniform buffer rather than a value inside an instance.

use crate::atlas::{AtlasPage, AtlasSlot, GlyphAtlas};
use crate::color::Rgba;
use crate::image::ImageInstance;
use crate::mesh::MeshBatch;
use crate::quad::QuadInstance;
use crate::shape::{PolygonArena, ShapeInstance, ShapeStyle};
use crate::text::GlyphInstance;
use crate::texture::TextureId;
use crate::view::View;
use vellum_scene::{Camera, RenderPayload, SceneItem};
use vellum_shapes::SdfParams;
use vellum_text::{Layout, Rgb};

/// What a batch draws, and what it has to bind to draw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchKind {
    Quads,
    Shapes,
    Images(TextureId),
    Glyphs(AtlasPage),
    Meshes,
}

/// One draw call: a pipeline, a view, and a contiguous run of its kind's array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Batch {
    pub view: u32,
    pub kind: BatchKind,
    /// Instances for the instanced pipelines, indices for the mesh pipeline.
    pub start: u32,
    pub end: u32,
}

/// Everything a frame draws, in order.
#[derive(Debug, Clone, Default)]
pub struct DrawList {
    views: Vec<View>,
    quads: Vec<QuadInstance>,
    shapes: Vec<ShapeInstance>,
    polygons: PolygonArena,
    images: Vec<ImageInstance>,
    glyphs: Vec<GlyphInstance>,
    meshes: MeshBatch,
    batches: Vec<Batch>,
    current_view: u32,
    /// Set once a draw has been dropped for want of a view, so the warning is not
    /// repeated for every item of a frame that has thousands.
    warned_about_view: bool,
}

impl DrawList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Empties everything, keeping the allocations. Called once per frame.
    pub fn clear(&mut self) {
        self.views.clear();
        self.quads.clear();
        self.shapes.clear();
        self.polygons.clear();
        self.images.clear();
        self.glyphs.clear();
        self.meshes.clear();
        self.batches.clear();
        self.current_view = 0;
        self.warned_about_view = false;
    }

    /// Selects the coordinate space subsequent draws are in, adding it if it is not
    /// already present. Returns its index.
    pub fn view(&mut self, view: View) -> u32 {
        let index = match self.views.iter().position(|v| *v == view) {
            Some(index) => index as u32,
            None => {
                self.views.push(view);
                self.views.len() as u32 - 1
            }
        };
        self.current_view = index;
        index
    }

    /// Re-selects a view added earlier. Out-of-range indices are ignored, because
    /// silently drawing in the wrong space is worse than drawing in the last one.
    pub fn use_view(&mut self, index: u32) -> bool {
        if (index as usize) < self.views.len() {
            self.current_view = index;
            return true;
        }
        false
    }

    pub fn push_quad(&mut self, quad: QuadInstance) {
        if quad.is_invisible() || !self.has_view() {
            return;
        }
        self.open(BatchKind::Quads, self.quads.len() as u32);
        self.quads.push(quad);
    }

    pub fn push_quads(&mut self, quads: impl IntoIterator<Item = QuadInstance>) {
        for quad in quads {
            self.push_quad(quad);
        }
    }

    /// Draws an item straight out of `vellum_scene`'s culling query, doing the `f64`
    /// → camera-relative `f32` rebasing on the way.
    ///
    /// The bridge lives here rather than in the caller because that subtraction is the
    /// single easiest thing in the renderer to get wrong: casting the world coordinate
    /// first and subtracting in `f32` compiles, looks correct at 100%, and shimmers
    /// only once someone zooms in at the far end of a large board.
    ///
    /// The current view must be the board view for the same camera.
    pub fn push_scene_item(&mut self, item: &SceneItem, camera: &Camera) {
        let bounds = item.bounds;
        let origin = camera.to_camera_relative(bounds.min);
        // Sizes stay in `f32` without rebasing: a size is a difference, so it never
        // carries the board's absolute extent, and the largest frame on the reference
        // board is four figures of pixels across.
        let size = [bounds.width() as f32, bounds.height() as f32];
        match item.payload {
            RenderPayload::SolidQuad { color } => {
                self.push_quad(QuadInstance::solid(origin, size, color.into()));
            }
        }
    }

    pub fn push_shape_instance(&mut self, shape: ShapeInstance) {
        if shape.is_invisible() || !self.has_view() {
            return;
        }
        self.open(BatchKind::Shapes, self.shapes.len() as u32);
        self.shapes.push(shape);
    }

    /// Draws whatever `vellum_shapes::Shape::sdf_params` produced, centred on
    /// `centre` in the current view's space.
    pub fn push_shape(&mut self, params: &SdfParams, centre: [f32; 2], style: &ShapeStyle) {
        if style.is_invisible() || !self.has_view() {
            return;
        }
        let instance = ShapeInstance::from_params(params, centre, style, &mut self.polygons);
        self.push_shape_instance(instance);
    }

    pub fn push_image(&mut self, texture: TextureId, image: ImageInstance) {
        if image.is_invisible() || !self.has_view() {
            return;
        }
        self.open(BatchKind::Images(texture), self.images.len() as u32);
        self.images.push(image);
    }

    /// One glyph, already looked up in the atlas.
    pub fn push_glyph(&mut self, slot: &AtlasSlot, pen: (i32, i32), color: Rgba) {
        if slot.width == 0 || slot.height == 0 || color.is_invisible() || !self.has_view() {
            return;
        }
        self.open(BatchKind::Glyphs(slot.page), self.glyphs.len() as u32);
        self.glyphs.push(GlyphInstance::from_slot(slot, pen, color));
    }

    /// Every glyph of a laid-out block.
    ///
    /// **The current view must be a screen-pixel view** ([`View::screen`]) and
    /// `origin` the block's top-left corner *in device pixels* — see [`crate::text`]
    /// for why text is not drawn through the camera transform. `scale` is device
    /// pixels per logical pixel, i.e. the camera's zoom times the DPI factor, and must
    /// be the same value the atlas was prepared with.
    ///
    /// Returns the number of glyphs skipped for want of an atlas slot — spaces
    /// excluded, since those legitimately have none. Anything but zero means
    /// [`GlyphAtlas::prepare`] was not given this layout at this scale, and the text on
    /// screen is missing characters.
    pub fn push_layout(
        &mut self,
        atlas: &GlyphAtlas,
        layout: &Layout,
        origin: [f32; 2],
        scale: f32,
        color: Rgba,
    ) -> usize {
        let mut missing = 0;
        for glyph in layout.glyphs() {
            let physical = glyph.physical((origin[0], origin[1]), scale);
            let Some(slot) = atlas.slot(physical.key) else {
                if !atlas.is_blank(physical.key) {
                    missing += 1;
                }
                continue;
            };
            // A span's own colour wins over the block's, which is what carries Miro's
            // per-run text colour through to the GPU.
            let color = match glyph.color {
                Some(Rgb { r, g, b }) => Rgba::from_rgb8(r, g, b).with_alpha(color.a),
                None => color,
            };
            self.push_glyph(slot, (physical.x, physical.y), color);
        }
        missing
    }

    /// The batch's triangles. Fill it through [`Self::meshes_mut`] first; this only
    /// records that a run of its indices is drawn at this point in the order.
    ///
    /// Splitting the two is what lets the geometry outlive the frame: a caller keeps
    /// one [`MeshBatch`] across frames, rewrites only the transforms as the camera
    /// moves, and calls this once per frame to place it in the paint order.
    pub fn push_meshes(&mut self, indices: std::ops::Range<u32>) {
        if indices.is_empty() || !self.has_view() {
            return;
        }
        // Mesh runs are addressed by index rather than appended to, so a run that
        // does not continue the previous one has to start its own batch.
        let contiguous = matches!(
            self.batches.last(),
            Some(batch)
                if batch.kind == BatchKind::Meshes
                    && batch.view == self.current_view
                    && batch.end == indices.start
        );
        if contiguous {
            self.batches.last_mut().expect("checked above").end = indices.end;
        } else {
            self.batches.push(Batch {
                view: self.current_view,
                kind: BatchKind::Meshes,
                start: indices.start,
                end: indices.end,
            });
        }
    }

    /// Records the whole mesh batch as one run.
    pub fn push_all_meshes(&mut self) {
        self.push_meshes(0..self.meshes.indices().len() as u32);
    }

    pub fn meshes(&self) -> &MeshBatch {
        &self.meshes
    }

    pub fn meshes_mut(&mut self) -> &mut MeshBatch {
        &mut self.meshes
    }

    pub fn polygons(&self) -> &PolygonArena {
        &self.polygons
    }

    pub(crate) fn views(&self) -> &[View] {
        &self.views
    }

    pub(crate) fn quad_instances(&self) -> &[QuadInstance] {
        &self.quads
    }

    pub(crate) fn shape_instances(&self) -> &[ShapeInstance] {
        &self.shapes
    }

    pub(crate) fn image_instances(&self) -> &[ImageInstance] {
        &self.images
    }

    pub(crate) fn glyph_instances(&self) -> &[GlyphInstance] {
        &self.glyphs
    }

    pub(crate) fn batches(&self) -> &[Batch] {
        &self.batches
    }

    /// What the frame costs, for a HUD or a regression test.
    pub fn stats(&self) -> DrawStats {
        DrawStats {
            draw_calls: self.batches.len(),
            views: self.views.len(),
            quads: self.quads.len(),
            shapes: self.shapes.len(),
            images: self.images.len(),
            glyphs: self.glyphs.len(),
            triangles: self.meshes.triangle_count(),
            polygon_vertices: self.polygons.vertices().len(),
        }
    }

    /// Whether there is a coordinate space to draw in at all.
    ///
    /// Pushing before the first [`Self::view`] would record a batch pointing at a
    /// uniform slot that was never written — geometry projected by whatever happened
    /// to be in the buffer, which reads as a blank canvas with no explanation. Dropping
    /// the draw and saying so once is the only outcome that leads anywhere.
    fn has_view(&mut self) -> bool {
        if !self.views.is_empty() {
            return true;
        }
        if !self.warned_about_view {
            self.warned_about_view = true;
            log::warn!("a draw was dropped: DrawList::view must be called before pushing anything");
        }
        false
    }

    /// Extends the open batch, or starts a new one.
    fn open(&mut self, kind: BatchKind, start: u32) {
        if let Some(batch) = self.batches.last_mut()
            && batch.kind == kind
            && batch.view == self.current_view
        {
            batch.end += 1;
            return;
        }
        self.batches.push(Batch {
            view: self.current_view,
            kind,
            start,
            end: start + 1,
        });
    }
}

/// A frame's cost, in the terms `docs/01-architecture.md` sets budgets in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrawStats {
    /// The number that has to stay small. One per pipeline switch, view switch,
    /// texture change or atlas page change — nothing else.
    pub draw_calls: usize,
    pub views: usize,
    pub quads: usize,
    pub shapes: usize,
    pub images: usize,
    pub glyphs: usize,
    pub triangles: usize,
    pub polygon_vertices: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::MeshTransform;
    use crate::texture::UvRect;
    use vellum_scene::{Camera, ScreenSize};
    use vellum_shapes::{Shape, Size};

    fn board_view() -> View {
        View::board(&Camera::new(ScreenSize::new(1600.0, 900.0)))
    }

    fn quad(x: f32) -> QuadInstance {
        QuadInstance::solid([x, 0.0], [10.0, 10.0], Rgba::WHITE)
    }

    /// Two distinct texture handles. A real one is minted by `TextureManager`, which
    /// needs a device; batching only ever compares them, so the identity is all that
    /// matters here.
    const PHOTO: TextureId = TextureId(0);
    const LOGO: TextureId = TextureId(1);

    #[test]
    fn a_run_of_one_kind_is_a_single_draw_call() {
        let mut list = DrawList::new();
        list.view(board_view());
        for i in 0..1000 {
            list.push_quad(quad(i as f32));
        }
        assert_eq!(list.stats().draw_calls, 1);
        assert_eq!(list.stats().quads, 1000);
    }

    /// The claim the whole design rests on: a realistic board — stickies, shapes,
    /// images, ink, text — costs a handful of draws, not one per widget.
    #[test]
    fn a_mixed_board_costs_a_handful_of_draw_calls() {
        let mut list = DrawList::new();
        let board = list.view(board_view());

        for i in 0..44 {
            list.push_quad(quad(i as f32 * 220.0).with_corner_radius(8.0));
        }
        let diamond = Shape::Diamond.sdf_params(Size::new(200.0, 120.0)).unwrap();
        for i in 0..47 {
            list.push_shape(&diamond, [i as f32 * 300.0, 500.0], &ShapeStyle::filled(Rgba::WHITE));
        }
        for i in 0..100 {
            list.push_image(
                PHOTO,
                ImageInstance::new([i as f32 * 400.0, 900.0], [380.0, 240.0], UvRect::FULL),
            );
        }
        list.push_image(LOGO, ImageInstance::new([0.0, 0.0], [64.0, 64.0], UvRect::FULL));

        let transform = list.meshes_mut().push_transform(MeshTransform::IDENTITY);
        for _ in 0..219 {
            list.meshes_mut().push_indexed(
                &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                &[0, 1, 2],
                Rgba::BLACK,
                transform,
            );
        }
        list.push_all_meshes();

        list.use_view(board);
        // Quads, shapes, one batch per texture, meshes.
        assert_eq!(list.stats().draw_calls, 5, "{:?}", list.stats());
        assert_eq!(list.stats().triangles, 219);
    }

    #[test]
    fn interleaving_kinds_costs_one_draw_call_per_switch() {
        let mut list = DrawList::new();
        list.view(board_view());
        let diamond = Shape::Diamond.sdf_params(Size::SQUARE).unwrap();
        for i in 0..3 {
            list.push_quad(quad(i as f32));
            list.push_shape(&diamond, [i as f32, 0.0], &ShapeStyle::filled(Rgba::WHITE));
        }
        assert_eq!(list.stats().draw_calls, 6);
    }

    #[test]
    fn a_view_switch_ends_the_batch() {
        let mut list = DrawList::new();
        let board = list.view(board_view());
        list.push_quad(quad(0.0));
        list.view(View::screen(ScreenSize::new(1600.0, 900.0)));
        list.push_quad(quad(1.0));
        list.use_view(board);
        list.push_quad(quad(2.0));

        assert_eq!(list.stats().draw_calls, 3);
        assert_eq!(list.stats().views, 2, "the board view is reused, not duplicated");
    }

    #[test]
    fn an_unknown_view_index_is_refused_rather_than_drawn_in_the_wrong_space() {
        let mut list = DrawList::new();
        let board = list.view(board_view());
        assert!(list.use_view(board));
        assert!(!list.use_view(9));
    }

    #[test]
    fn a_texture_change_ends_the_batch() {
        let mut list = DrawList::new();
        list.view(board_view());
        let instance = ImageInstance::new([0.0; 2], [10.0; 2], UvRect::FULL);
        list.push_image(PHOTO, instance);
        list.push_image(PHOTO, instance);
        list.push_image(LOGO, instance);
        list.push_image(PHOTO, instance);
        assert_eq!(list.stats().draw_calls, 3);
    }

    /// Filtering invisible work out here is what keeps a board full of transparent
    /// frames from costing anything at all.
    #[test]
    fn invisible_draws_never_reach_a_batch() {
        let mut list = DrawList::new();
        list.view(board_view());
        list.push_quad(QuadInstance::solid([0.0; 2], [10.0; 2], Rgba::TRANSPARENT));
        list.push_shape(
            &Shape::Diamond.sdf_params(Size::SQUARE).unwrap(),
            [0.0, 0.0],
            &ShapeStyle::default(),
        );
        list.push_image(
            PHOTO,
            ImageInstance::new([0.0; 2], [10.0; 2], UvRect::FULL).with_opacity(0.0),
        );
        assert_eq!(list.stats().draw_calls, 0);
        assert_eq!(list.stats().quads, 0);
        assert!(list.polygons().is_empty(), "an invisible shape interns no vertices");
    }

    /// Two consecutive mesh runs that continue one another are one draw, but a gap
    /// between them is not — the indices in between belong to something drawn
    /// elsewhere in the order.
    #[test]
    fn adjacent_mesh_runs_merge_and_a_gap_does_not() {
        let mut list = DrawList::new();
        list.view(board_view());
        list.push_meshes(0..30);
        list.push_meshes(30..60);
        assert_eq!(list.stats().draw_calls, 1);
        list.push_meshes(90..120);
        assert_eq!(list.stats().draw_calls, 2);
        list.push_meshes(0..0);
        assert_eq!(list.stats().draw_calls, 2, "an empty run draws nothing");
    }

    /// A draw with no view would project geometry through a uniform slot nobody wrote,
    /// which renders as a blank canvas and gives the reader nothing to go on.
    #[test]
    fn pushing_before_a_view_is_set_draws_nothing() {
        let mut list = DrawList::new();
        list.push_quad(quad(0.0));
        list.push_image(PHOTO, ImageInstance::new([0.0; 2], [10.0; 2], UvRect::FULL));
        list.push_meshes(0..3);
        assert_eq!(list.stats(), DrawStats::default());

        list.view(board_view());
        list.push_quad(quad(0.0));
        assert_eq!(list.stats().quads, 1);
    }

    /// The `f64` → camera-relative bridge, at the far corner of the real board. The
    /// instance must carry the offset from the camera, not the world coordinate.
    #[test]
    fn a_scene_item_is_rebased_onto_the_camera() {
        use vellum_scene::{WorldPoint, WorldRect};

        let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        camera.set_center(WorldPoint::new(41_282.89, 17_515.36));

        let item = vellum_scene::SceneItem::solid_quad(
            1,
            WorldRect::from_origin_size(WorldPoint::new(41_182.89, 17_415.36), 199.0, 228.0),
            0,
            [1.0, 0.97, 0.62, 1.0],
        );

        let mut list = DrawList::new();
        list.view(View::board(&camera));
        list.push_scene_item(&item, &camera);

        assert_eq!(list.stats().quads, 1);
        let quad = list.quad_instances()[0];
        assert!((quad.origin[0] + 100.0).abs() < 1e-3, "{:?}", quad.origin);
        assert!((quad.origin[1] + 100.0).abs() < 1e-3, "{:?}", quad.origin);
        assert_eq!(quad.size, [199.0, 228.0]);
        assert_eq!(quad.fill, Rgba::new(1.0, 0.97, 0.62, 1.0));
    }

    #[test]
    fn clearing_resets_everything_including_the_view() {
        let mut list = DrawList::new();
        list.view(View::screen(ScreenSize::new(100.0, 100.0)));
        list.push_quad(quad(0.0));
        list.meshes_mut().push_transform(MeshTransform::IDENTITY);
        list.clear();
        assert_eq!(list.stats(), DrawStats::default());
        assert!(list.meshes().transforms().is_empty());
    }
}
