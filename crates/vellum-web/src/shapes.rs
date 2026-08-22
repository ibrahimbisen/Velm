//! The 41 catalogue shapes — the silhouette, not the box it fits in.
//!
//! Every other kind on this board is honestly approximated by its bounding rectangle: a
//! sticky *is* a filled box, a frame is a box with a hairline, a card is a box with a
//! picture in it. A shape is the one kind where the rectangle is the *negation* of the
//! drawing. `push_scene_item` gave an ellipse, a diamond, a cylinder and all twenty
//! flowchart forms the same filled box, which is not a coarse rendering of a diamond so
//! much as a picture of the thing a diamond is drawn to avoid being. This module is
//! `draw.rs`'s `ItemKind::Shape` arm, reduced to what a reader needs and no further.
//!
//! # Two paths, and `vellum-shapes` decides which
//!
//! [`Shape::sdf_params`] answers `Some` for everything whose parameters survive a
//! non-uniform scale — boxes, rounded boxes, ellipses, and *any polygon*, a star included,
//! because the polygon form falls back to the outline's own vertices. Those go to
//! [`DrawList::push_shape`] as one instanced quad with the border falling out of the same
//! distance value: resolution-independent at any zoom, no triangles, no cache.
//!
//! The handful that answer `None` — cloud, heart, cylinder, speech bubble, the document
//! wave, arcs and wedges — carry curves whose control points move when the box is stretched,
//! so they are tessellated and cached. That is `docs/01-architecture.md` §3's dividing line
//! and this module does not re-decide it; it asks.
//!
//! # ⚠ World units, not screen pixels
//!
//! Everything below goes to the list in **camera-relative world units**, because the board
//! view's clip transform already carries the zoom. Multiplying by `camera.zoom()` here
//! applies it twice, and the symptom is not subtle-but-wrong: at a fitted 6% every shape
//! would be drawn at 6% of its own box. The image arm in `lib.rs` carries the same warning
//! with the measurement that produced it.
//!
//! # What is deliberately not here
//!
//! **A shape's label.** `crate::layout::text_slot` already gives `ItemKind::Shape` a centred,
//! inset box and `crate::text` shapes it in the screen view — one derivation, and it is not
//! this one. Drawing words here would put a second copy of every label on the board at a
//! slightly different size, which reads as a font bug rather than as double-drawing.
//!
//! **A fallback box.** If a form cannot be tessellated this draws nothing and says so in the
//! console. Nothing is the honest answer; a rectangle is the bug this module exists to
//! remove, and re-introducing it on the error path would make the failure invisible.

use std::collections::{HashMap, HashSet};

use vellum_doc::ItemKind;
use vellum_project::look::lod_band;
use vellum_project::project::Projection;
use vellum_project::theme::{self, Theme};
use vellum_render::{DrawList, MeshTransform, ShapeStyle};
use vellum_scene::{Camera, ItemId as SceneId};
use vellum_shapes::{Shape, ShapeMesh, Size, TessellationOptions};

/// The outline width an item that names none is drawn with, in world units.
///
/// `draw.rs`'s `HAIRLINE`, which is private to that module: one device pixel at 100% zoom.
/// A shape's border is drawn *inside* its edge by both paths, so unlike a frame's chrome
/// hairline it is part of the drawing and takes no zoom compensation — it thickens with the
/// board exactly as the shape it outlines does.
const HAIRLINE: f64 = 1.0;

/// One tessellated silhouette, and what it was tessellated for.
struct CachedShape {
    generation: u64,
    band: i32,
    mesh: ShapeMesh,
}

/// The per-frame drawing of shapes, plus the triangles that survive between frames.
#[derive(Default)]
pub struct ShapeLayer {
    /// Only the forms an SDF cannot express. An analytic shape is an instance in the frame's
    /// list and nothing else, so it has nothing to cache and never appears here — which is
    /// why this map holds a handful of entries on a board of hundreds of shapes.
    meshes: HashMap<SceneId, CachedShape>,
}

impl ShapeLayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Draw one item's form, if it is a shape. Returns whether anything was pushed.
    ///
    /// The list must be in the **board** view: the centre is camera-relative and the extent
    /// is in world units, so a screen-view list would place every shape at the board's
    /// absolute coordinates in physical pixels.
    ///
    /// ⚠ **`false` is not a cue to draw the item some other way.** It means the item was not
    /// a shape, or its form produced no geometry, or its fill and border are both invisible —
    /// and the honest drawing of all three is nothing. The caller dispatches on the item's
    /// *kind*; it must not fall back to `push_scene_item`, which is what painted every
    /// ellipse on the board as a rectangle in the first place.
    pub fn push(
        &mut self,
        list: &mut DrawList,
        camera: &Camera,
        id: SceneId,
        projection: &Projection,
        theme: &Theme,
    ) -> bool {
        let Some(projected) = projection.get(id) else { return false };
        let ItemKind::Shape { form, .. } = &projected.item.kind else { return false };
        let shape = decode(form);

        // ⚠ `rect()`, never `bounds`. `bounds` is the axis-aligned box the rotated item
        // *encloses*, which is what the R-tree culls and hit-tests against and is strictly
        // larger than the item for anything turned off the axis. Both paths below take the
        // unrotated box and spin it themselves, so feeding them the enclosing box would draw
        // a rotated diamond too big and centred correctly, which looks like a scale bug.
        let (origin, (width, height)) = projected.rect();
        let position = camera.to_camera_relative(origin);
        let size = Size::new(width as f32, height as f32);
        let rotation = projected.rotation();
        let opacity = projected.opacity();

        // A shape keeps its interior on `Style::fill` and its outline on `Style::stroke` —
        // `inspect::stroke_home` in the desktop app is where that split is named. Neither is
        // the item's own ink: a shape merely *has* an outline where a pen stroke *is* one.
        let fill = projected.item.style.fill.map_or(theme.surface, theme::convert);
        let border = projected.item.style.stroke.map_or(theme.border, theme::convert);
        let border_width = projected.item.style.stroke_width.unwrap_or(HAIRLINE) as f32;

        if let Some(params) = shape.sdf_params(size) {
            let style = ShapeStyle { fill, border, border_width, rotation, opacity };
            if style.is_invisible() {
                return false;
            }
            // `sdf_params` returns the shape centred on the item, so this is where the unit
            // box's origin lands rather than where the item's top-left does.
            list.push_shape(
                &params,
                [position[0] + size.width / 2.0, position[1] + size.height / 2.0],
                &style,
            );
            return true;
        }

        let band = lod_band(camera.zoom());
        let generation = projected.generation;
        let entry = self.meshes.entry(id).or_insert_with(|| CachedShape {
            generation,
            band,
            mesh: tessellate(shape, size, band),
        });
        if entry.generation != generation || entry.band != band {
            entry.generation = generation;
            entry.band = band;
            entry.mesh = tessellate(shape, size, band);
        }

        // The unit box maps onto the item's rect, so the mesh carries no size of its own and
        // a resize is a new transform rather than a new tessellation.
        let transform = list.meshes_mut().push_transform(MeshTransform::unit_box(
            position,
            [size.width, size.height],
            rotation,
        ));
        let start = list.meshes().indices().len() as u32;
        list.meshes_mut().push_shape_fill(
            &entry.mesh.fill,
            fill.with_alpha(fill.a * opacity),
            transform,
        );
        // The stroke mesh stores each vertex's position on the path plus an offset normal,
        // so a border width is a multiplication here rather than a re-tessellation. It is in
        // unit-box space, hence the division — and by the *longer* side, matching `draw.rs`,
        // which is a deliberate approximation: the unit box scales anisotropically, so an
        // exact border on a 3:1 box would need a per-axis normal the mesh does not carry.
        let unit_width = border_width / size.width.max(size.height).max(1.0);
        list.meshes_mut().push_shape_stroke(
            &entry.mesh.stroke,
            unit_width,
            border.with_alpha(border.a * opacity),
            transform,
        );
        let end = list.meshes().indices().len() as u32;
        if end == start {
            // A transform was pushed and nothing referenced it, which costs 32 bytes in the
            // uniform and draws nothing. Pushing an empty range instead would open a mesh
            // batch for zero triangles and split the batch either side of it.
            return false;
        }
        list.push_meshes(start..end);
        true
    }

    /// Drop the triangles of anything no longer on screen.
    ///
    /// The same argument `StrokeLayer::retain_visible` makes, and the same shape of leak if
    /// it is skipped: a mesh held is the one tessellated at the **finest band ever visited**,
    /// so a cylinder seen once at 8× keeps that mesh for the life of the tab, in a linear
    /// wasm memory that never returns anything to the OS. `keep` is a superset of the ids
    /// here by construction, so the retain is cheap as well as correct.
    ///
    /// ⚠ **No size guard.** The count of cached meshes and the count of visible items measure
    /// different populations — one entry per *tessellated shape* against one per visible item
    /// of every kind — so a `self.meshes.len() <= on_screen.len()` early-out would fire on
    /// essentially every frame and release nothing. That guard was in `strokes.rs` and was
    /// removed for exactly this reason; it is not re-introduced here.
    pub fn retain_visible(&mut self, on_screen: &[SceneId]) {
        if self.meshes.is_empty() {
            return;
        }
        let keep: HashSet<SceneId> = on_screen.iter().copied().collect();
        self.meshes.retain(|id, _| keep.contains(id));
    }
}

/// The shape a document token names.
///
/// **A token this build does not understand becomes a rectangle**, not an error and not a
/// missing item — the same degradation `vellum-app/src/shapes.rs` performs, and it has to
/// stay identical or a board opens differently in a tab than it does on the desktop. The
/// *encoding* is not duplicated: it is the `Deserialize` derive on `Shape`, so a shape added
/// to the catalogue costs nothing here. Only the fallback decision is written twice, and the
/// durable fix is moving that module down into `vellum-project`, which both crates already
/// depend on.
fn decode(token: &str) -> Shape {
    serde_json::from_str(token).unwrap_or_else(|error| {
        log::warn!("unknown shape `{token}` ({error}); drawing a rectangle");
        Shape::Rectangle
    })
}

/// Flatten one shape's curves for the zoom band it is being drawn at.
///
/// ⚠ **The band is used, and on the desktop it is not — deliberately, and it is not a
/// mistake to be tidied back.** `Painter`'s own doc comment says the shape cache is *"keyed
/// on the LOD band as well as the item ... the mesh's flattening tolerance is chosen from
/// the drawn size, so a zoom changes what it should be"*, and its code then calls
/// `TessellationOptions::for_size` with the raw **world** size, which no zoom can move — so
/// the band multiplies the cache and changes nothing that comes out of it. `for_size` keeps
/// the flattening error at a quarter of whatever unit it is handed, and the unit worth a
/// quarter of is a *device pixel*, not a world unit: at 8× a 400-unit shape drawn to a
/// quarter-world-unit tolerance is two visible pixels off its own curve.
///
/// So the drawn size is `world × 2^band`, and the band is `ceil(log2(zoom))` — the octave,
/// which is what keeps this to one re-tessellation per doubling rather than one per frame of
/// a pinch. At band 0 this is byte for byte what the desktop produces; below it is cheaper,
/// because `for_size` caps the tolerance at 0.01 and a fitted board's shapes are a few
/// pixels across; above it is what the desktop's comment already promised.
///
/// `placement.scale` needs no separate factor the way an ink stroke's does: `rect()` reports
/// `scaled_size()`, so the scale is already inside `size`, and the vertices are magnified by
/// the transform rather than by the GPU's view matrix.
fn tessellate(shape: Shape, size: Size, band: i32) -> ShapeMesh {
    let drawn = size.width.max(size.height) * 2f32.powi(band);
    shape
        .tessellate(size.aspect(), TessellationOptions::for_size(drawn))
        .unwrap_or_else(|error| {
            // Logged, not swallowed. A shape that fails to tessellate draws nothing, and
            // "nothing" is exactly what a screenshot of an unwired feature looks like — so
            // the one line that tells the two apart is worth a console write on a path that
            // does not normally run. `strokes.rs` and `draw.rs` both warn here for this.
            log::warn!("shape tessellation failed: {error}");
            ShapeMesh::default()
        })
}
