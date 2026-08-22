//! Pen strokes and connectors — everything the browser draws as triangles.
//!
//! Quads, shapes, images and glyphs all resolve their coverage analytically in a shader.
//! These two do not: an ink stroke is a variable-width ribbon and a connector is a routed
//! path with arrowheads, and both arrive as tessellated geometry through
//! [`vellum_render::MeshBatch`]. They share a module because they share that pipeline, the
//! `push_transform` → `push_*` → `push_meshes` sequence, and the one thing that makes either
//! affordable — a cache keyed on the zoom band, so panning re-tessellates nothing.
//!
//! # Why the band, and why the item's scale is in it
//!
//! A stroke is tessellated in its own local space and then magnified on the GPU by the
//! item's `placement.scale`, so what a viewer actually sees is `zoom × scale`. Deriving the
//! tolerance from the zoom alone tessellates an imported Miro stroke at scale 2 exactly
//! twice too coarsely — and Velm's own strokes carry scale 1.0, which is why that bug
//! survived a long time natively before feedback 27 found it. The band here is the same
//! product, computed by the same `ceil`, for the same reason. This is not a port of the
//! arithmetic so much as a second caller of the same rule; `draw.rs` states it at length.
//!
//! # What is deliberately not here
//!
//! Obstacle avoidance for orthogonal connector routes. `draw.rs` passes an empty obstacle
//! slice too, so this matches rather than degrades — but say it, because a route that dodges
//! natively and not here would be a difference nobody could account for.
//!
//! Agent-link styling. `draw.rs`'s connector arm overrides the colour, the dash cadence and
//! the thickness of a line whose two ends are agent nodes — checked against the tree, that
//! `if color.is_none()` branch is *entirely* about the Agent Canvas, which a browser tab does
//! not have (`docs/08-web.md`). So the omission is the layer being absent, not a rule being
//! dropped: the colour resolution below is what native does for every ordinary connector.

use std::collections::HashMap;

use vellum_doc::ItemKind;
use vellum_ink::{Lod, Stroke};
use vellum_project::look::lod_band;
use vellum_project::project::Projection;
use vellum_render::{DrawList, MeshTransform, Rgba};
use vellum_scene::{Camera, ItemId as SceneId, WorldPoint};

/// One stroke's triangles and the zoom band they were tessellated for.
struct CachedInk {
    generation: u64,
    band: i32,
    mesh: vellum_ink::Mesh,
}

/// The per-frame drawing of ink and connectors, plus what survives between frames.
#[derive(Default)]
pub struct StrokeLayer {
    ink: HashMap<SceneId, CachedInk>,
    router: vellum_connect::Router,
}

impl StrokeLayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Draw one item's ink, if it is ink. Returns whether anything was pushed.
    ///
    /// The list must be in the **board** view: the transform is camera-relative and the
    /// vertices are in stroke-local units, so a screen-view list would draw the stroke at
    /// the board's absolute extent in screen pixels — off the edge of the world.
    pub fn push_ink(
        &mut self,
        list: &mut DrawList,
        camera: &Camera,
        id: SceneId,
        projection: &Projection,
        stroke_colour: Rgba,
    ) -> bool {
        let Some(projected) = projection.get(id) else { return false };
        let ItemKind::Ink { color, thickness, points } = &projected.item.kind else {
            return false;
        };
        let colour = color.map_or(stroke_colour, vellum_project::theme::convert);
        let opacity = projected.opacity();
        let band = lod_band(camera.zoom() * projected.item.placement.scale);
        let generation = projected.generation;

        let entry = self.ink.entry(id).or_insert_with(|| CachedInk {
            generation,
            band,
            mesh: tessellate(points, *thickness, band),
        });
        if entry.generation != generation || entry.band != band {
            entry.generation = generation;
            entry.band = band;
            entry.mesh = tessellate(points, *thickness, band);
        }
        if entry.mesh.indices.is_empty() {
            return false;
        }

        // The stroke's points are relative to the item's centre, which is also what the
        // placement is — so the transform is the centre and the vertices never carry the
        // board's absolute extent. At 41,282 world units wide that distinction is the
        // difference between f32 holding the geometry and not.
        let centre = WorldPoint::new(projected.item.placement.x, projected.item.placement.y);
        let transform = list.meshes_mut().push_transform(MeshTransform::scale_rotate_at(
            projected.item.placement.scale as f32,
            projected.rotation(),
            camera.to_camera_relative(centre),
        ));
        let start = list.meshes().indices().len() as u32;
        list.meshes_mut()
            .push_ink(&entry.mesh, colour.with_alpha(colour.a * opacity), transform);
        let end = list.meshes().indices().len() as u32;
        list.push_meshes(start..end);
        true
    }

    /// Draw one item's connector, if it is one. Returns whether anything was pushed.
    ///
    /// Unlike ink this is **not** cached: a connector's route is resolved from the live
    /// bounds of whatever it joins, so storing it would produce a line that lies the moment
    /// either end moves. `vellum-export` reaches the same conclusion for the same reason.
    pub fn push_connector(
        &self,
        list: &mut DrawList,
        camera: &Camera,
        id: SceneId,
        projection: &Projection,
        stroke_colour: Rgba,
    ) -> bool {
        let Some(projected) = projection.get(id) else { return false };
        let ItemKind::Connector { color, .. } = &projected.item.kind else { return false };
        let colour = color.map_or(stroke_colour, vellum_project::theme::convert);
        let opacity = projected.opacity();

        let Some(routed) = vellum_project::connector::route(
            &projected.item.kind,
            &projected.item.placement,
            |target| projection.placement_of(target),
            &self.router,
            &[],
        ) else {
            return false;
        };
        let options = vellum_connect::TessellationOptions {
            // A curve only has to be smooth to the pixel a viewer can see. Tessellating a
            // board-spanning bezier to 0.05 world units when it is forty pixels on screen is
            // thousands of triangles nobody can tell from four.
            tolerance: (0.5 / camera.zoom()).clamp(0.05, 64.0),
        };
        let Ok(mesh) = vellum_connect::tessellate(&routed.path, &routed.style, &options) else {
            return false;
        };
        let transform = list.meshes_mut().push_transform(MeshTransform::at(
            camera.to_camera_relative(WorldPoint::new(mesh.origin.x, mesh.origin.y)),
        ));
        let first = list.meshes().indices().len() as u32;
        list.meshes_mut()
            .push_connector(&mesh, colour.with_alpha(colour.a * opacity), transform);
        let last = list.meshes().indices().len() as u32;
        list.push_meshes(first..last);
        true
    }

    /// Drop the tessellation of anything no longer on screen.
    ///
    /// Without this the cache is a leak with a nice name: pan across a board of 219 strokes
    /// and every one of them keeps its triangles for the life of the tab, in a linear memory
    /// that never returns to the OS. The same argument `TextLayer::retain_visible` makes.
    /// ⚠ No size guard. It used to skip while `ink.len() <= on_screen.len()`, and the two
    /// count different populations: `ink` holds one entry per *stroke*, `on_screen` one per
    /// visible item of every kind. On the reference board that is 219 against 596, so the
    /// guard fired on essentially every frame and nothing was ever released — while the mesh
    /// held is the one tessellated at the **finest band ever visited**, so a stroke seen once
    /// at 8× keeps that mesh for the life of the tab. `keep` is a superset of the ink ids by
    /// construction, so the retain was always cheap and always correct.
    pub fn retain_visible(&mut self, on_screen: &[SceneId]) {
        if self.ink.is_empty() {
            return;
        }
        let keep: std::collections::HashSet<SceneId> = on_screen.iter().copied().collect();
        self.ink.retain(|id, _| keep.contains(id));
    }
}

fn tessellate(points: &[vellum_doc::Point], thickness: f64, band: i32) -> vellum_ink::Mesh {
    let coordinates: Vec<(f64, f64)> = points.iter().map(|p| (p.x, p.y)).collect();
    Stroke::from_miro(&coordinates, Some(thickness))
        .render(Lod::new(2f64.powi(band)))
        .unwrap_or_else(|error| {
            // Logged, not swallowed. A stroke that fails to tessellate draws nothing, and
            // "nothing" is exactly what a screenshot of a missing feature looks like — so
            // the one line that tells the two apart costs a console write on a path that
            // does not normally run. `draw.rs` warns here for the same reason.
            log::warn!("ink tessellation failed: {error}");
            vellum_ink::Mesh::default()
        })
}
