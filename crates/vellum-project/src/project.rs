//! Projecting the document into the spatial index.
//!
//! [`vellum_doc::Board`] is a CRDT: correct, ordered, and far too expensive to walk
//! sixty times a second — reading one item decodes a tree node and a map. The
//! renderer instead walks [`vellum_scene::Scene`], an R-tree of boxes, so that frame
//! cost tracks the viewport rather than the document. This module is the one place
//! the two are reconciled, and everything it computes is a *derived* value that the
//! document remains the truth for.
//!
//! # Three things this has to get right
//!
//! **The scene's payload is not the drawing.** `vellum_scene::RenderPayload` has a
//! single variant, so the projection carries the full [`vellum_doc::Item`] alongside
//! the box and the renderer reads it from here. The payload still gets the item's
//! dominant colour, so anything that falls back to the scene layer's own quad draws
//! something recognisable rather than nothing.
//!
//! **Bounds are axis-aligned and rotation is not.** A turned item contributes its
//! *enclosing* box, which is what keeps culling a cheap interval test — the exact
//! silhouette is the hit-tester's problem, and the renderer's.
//!
//! **Connectors have no bounds of their own.** Their geometry is their endpoints, so
//! they are resolved in a second pass once every other item's box exists. A
//! connector routed around obstacles can leave the box its endpoints span, so the
//! routed path is measured rather than assumed.
//!
//! # Identity
//!
//! Scene ids are `u64` and document ids are Loro `TreeID`s. The map between them is
//! kept here and **preserved across reprojections**, so a selection, a hover target
//! or a texture binding survives an edit somewhere else on the board.

use std::collections::HashMap;

use vellum_connect::Router;
use vellum_doc::{Board, Item, ItemId as DocId, ItemKind, Placement};
use vellum_render::Rgba;
use vellum_scene::{ItemId as SceneId, Scene, SceneItem, WorldPoint, WorldRect};

use crate::connector;
use crate::theme::{self, Theme};

/// One item, ready to cull, hit-test and draw.
#[derive(Debug, Clone)]
pub struct Projected {
    pub doc_id: DocId,
    pub parent: Option<SceneId>,
    /// Enclosing axis-aligned box, rotation and scale already applied.
    pub bounds: WorldRect,
    /// Paint order: the document's depth-first order, so a frame draws before its
    /// contents and siblings draw back to front.
    pub z: i32,
    pub item: Item,
    /// How many times *this item's* content has changed, as a cache stamp.
    ///
    /// **Per item, not per projection, and that is the whole point.** The painter's
    /// text, table, mind-map and kanban caches ask "is what I have still current?" by
    /// comparing stamps, and they used to be handed the *projection's* generation — so
    /// any edit anywhere bumped it and **every visible block re-shaped**. Typing one
    /// character into one sticky re-laid every piece of text on screen through
    /// cosmic-text, sixty times a second, which is what made typing feel like it was
    /// *"struggling so much to show me what i am typing"*.
    ///
    /// A full [`Projection::rebuild`] gives every item the same fresh stamp, so nothing
    /// is cached across a structural change. [`Projection::refresh_item`] bumps one.
    pub generation: u64,
}

impl Projected {
    /// The item's own rectangle before rotation, as the renderer needs it: top-left
    /// corner and size in world pixels.
    pub fn rect(&self) -> (WorldPoint, (f64, f64)) {
        let (width, height) = self.item.placement.scaled_size();
        (
            WorldPoint::new(
                self.item.placement.x - width / 2.0,
                self.item.placement.y - height / 2.0,
            ),
            (width, height),
        )
    }

    /// Rotation in radians, clockwise, matching every renderer instance field.
    pub fn rotation(&self) -> f32 {
        (self.item.placement.rotation.to_radians()) as f32
    }

    /// Whole-item opacity, `1.0` when the style says nothing.
    pub fn opacity(&self) -> f32 {
        self.item.style.opacity.unwrap_or(1.0).clamp(0.0, 1.0) as f32
    }
}

/// The document, projected. Rebuilt on every edit, queried every frame.
#[derive(Debug, Default)]
pub struct Projection {
    scene: Scene,
    items: HashMap<SceneId, Projected>,
    by_doc: HashMap<DocId, SceneId>,
    next_id: SceneId,
    content: Option<WorldRect>,
    /// Bumped by every rebuild, so caches keyed on item geometry know to drop.
    generation: u64,
    /// Set by [`Self::shed`] and cleared by the next rebuild. Distinguishes a board
    /// whose cache was released from one that is genuinely empty — the two look
    /// identical from `len()`, and only the first must be rebuilt before it is drawn.
    shed: bool,
}

impl Projection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Releases everything that can be rebuilt from the document, for a board that is
    /// open but not on screen.
    ///
    /// Drops the two big things — `items`, which holds a **clone of every document
    /// `Item`** including ink point vectors and styled text, and the R-tree over them.
    ///
    /// # `by_doc` and `next_id` deliberately stay
    ///
    /// They are the board's *identity*, not its cache. [`Self::intern`] reuses
    /// `by_doc` so an item keeps the same [`SceneId`] across a rebuild, and
    /// `Editor::selection` is a `Vec<SceneId>` — so dropping the map would renumber
    /// every item from zero and a parked board would come back with its selection
    /// silently naming *different* items. Two `u64` per item is nothing beside the
    /// `Item` clones this releases.
    pub fn shed(&mut self) {
        self.items = HashMap::new();
        self.scene = Scene::new();
        self.content = None;
        self.shed = true;
    }

    /// Whether [`Self::shed`] released this projection and no rebuild has run since.
    pub const fn is_shed(&self) -> bool {
        self.shed
    }

    pub fn get(&self, id: SceneId) -> Option<&Projected> {
        self.items.get(&id)
    }

    pub fn scene_id(&self, doc_id: DocId) -> Option<SceneId> {
        self.by_doc.get(&doc_id).copied()
    }

    /// A document item's placement, for resolving a connector binding. Returns
    /// `None` for an id that is no longer on the board — a dangling binding, which
    /// `vellum_doc::ConnectorEnd` documents as an expected state rather than an
    /// error.
    pub fn placement_of(&self, doc_id: DocId) -> Option<Placement> {
        let scene_id = self.by_doc.get(&doc_id)?;
        Some(self.items.get(scene_id)?.item.placement)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SceneId, &Projected)> {
        self.items.iter()
    }

    /// The extent of everything on the board, for "fit to content". `None` on an
    /// empty board — there is nothing to fit, and a zero-sized rect would zoom the
    /// camera to its limit.
    pub fn content_bounds(&self) -> Option<WorldRect> {
        self.content
    }

    /// Rebuilds from `board`, keeping the scene ids of items that still exist.
    ///
    /// A full rebuild rather than a diff: it is `O(n)` on a document that changed,
    /// which happens on a paste or an undo, not per frame. Incremental updates are
    /// [`Self::moved`]'s job, and that is the path a drag takes.
    /// Re-reads **one** item after an edit that cannot have moved anything.
    ///
    /// [`Self::rebuild`] deep-clones every item out of the CRDT and rebuilds the R-tree.
    /// That is the right answer for a structural change and the wrong one for a
    /// keystroke: on a 1,300-item board it reconstructs the whole document per
    /// character. This touches the one item that changed.
    ///
    /// **Refuses, by returning `false`, if the item's bounds moved.** A text edit does
    /// not move a sticky — bounds come from the placement, and auto-fit changes the font
    /// size rather than the box — but a caller that is wrong about that would leave the
    /// R-tree disagreeing with the document, which is a click landing where an item is
    /// not. So the cheap path is *verified* rather than assumed, and the caller falls
    /// back to a rebuild when it does not hold.
    pub fn refresh_item(&mut self, board: &Board, doc_id: DocId) -> vellum_doc::Result<bool> {
        let Some(scene_id) = self.by_doc.get(&doc_id).copied() else { return Ok(false) };
        let Some(existing) = self.items.get(&scene_id) else { return Ok(false) };

        let item = board.item(doc_id)?;
        // Only this item's own placement is available cheaply, which is all a
        // non-connector needs. A connector's extent depends on other items, so it is
        // never eligible for the cheap path.
        if matches!(item.kind, ItemKind::Connector { .. }) {
            return Ok(false);
        }
        let placements = HashMap::from([(doc_id, item.placement)]);
        let bounds = item_bounds(&item, &placements);
        if bounds != existing.bounds {
            return Ok(false);
        }

        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        if let Some(projected) = self.items.get_mut(&scene_id) {
            projected.item = item;
            projected.generation = generation;
        }
        Ok(true)
    }

    /// Which items a marquee over `rect` catches, in paint order.
    ///
    /// **One rule, two callers.** `Editor::marquee` commits it on the button coming up and
    /// `App` draws a selection ring from it every frame the sweep is moving — and a preview
    /// that disagreed with the commit would be worse than no preview, because it would be
    /// believed. So the containment rule lives here rather than in either of them.
    ///
    /// A **frame** is caught only when the sweep contains the whole of it; everything else is
    /// caught by intersection. A frame is usually the largest thing on the board and everything
    /// sits on top of it, so an intersection test would hand the container back for any sweep
    /// that touched any of its contents — *"unless i select the whole frame it shouldnt select
    /// that frame."* Requiring full containment of *everything* is the other wrong answer: a
    /// sweep that clipped a sticky's corner would select nothing.
    pub fn marquee_hits(&self, rect: WorldRect) -> Vec<SceneId> {
        let mut hits: Vec<SceneId> = self
            .scene()
            .query_rect(rect)
            .filter(|item| {
                self.get(item.id).is_none_or(|projected| {
                    !matches!(projected.item.kind, vellum_doc::ItemKind::Frame { .. })
                        || (rect.min.x <= projected.bounds.min.x
                            && rect.min.y <= projected.bounds.min.y
                            && rect.max.x >= projected.bounds.max.x
                            && rect.max.y >= projected.bounds.max.y)
                })
            })
            .map(|item| item.id)
            .collect();
        // Deterministic, and in paint order, so a selection reads the same way twice.
        hits.sort_unstable_by_key(|id| self.get(*id).map(|p| p.z).unwrap_or(0));
        hits
    }

    pub fn rebuild(&mut self, board: &Board) -> vellum_doc::Result<()> {
        let ids = board.item_ids();
        let mut items = HashMap::with_capacity(ids.len());
        let mut by_doc = HashMap::with_capacity(ids.len());

        // Pass one: everything with a placement of its own.
        let mut placements: HashMap<DocId, Placement> = HashMap::with_capacity(ids.len());
        let mut pending = Vec::with_capacity(ids.len());
        let total = ids.len() as i32;
        for (index, doc_id) in ids.iter().enumerate() {
            let item = board.item(*doc_id)?;
            placements.insert(*doc_id, item.placement);
            let scene_id = self.intern(*doc_id);
            by_doc.insert(*doc_id, scene_id);
            pending.push((scene_id, depth(&item, index as i32, total), item));
        }

        // The generation this rebuild *would* stamp everything with. It still advances,
        // because `Painter::sync` compares it to decide whether to prune caches for items
        // the board no longer holds — a rebuild that left the counter alone would leave a
        // deleted item's layout resident for ever.
        let generation = self.generation.wrapping_add(1);

        // Pass two: bounds. Connectors are resolved last because they are the only
        // kind whose extent depends on other items.
        for (scene_id, z, item) in pending {
            // `item.parent`, not a second `board.parent_of` — `Board::item` has already
            // asked, and asking again was one extra CRDT lookup per item per rebuild.
            let parent = item.parent.and_then(|p| by_doc.get(&p)).copied();
            let bounds = item_bounds(&item, &placements);
            // **An item that did not change keeps its old generation.**
            //
            // Every layout cache in `crate::draw` — shaped text, tessellated ink, the
            // three structured widgets, an agent node's pieces — is keyed on
            // `Projected::generation`. Stamping the whole board with one new number meant
            // that moving a single sticky threw away every laid-out block on the board and
            // made the painter reshape all of them, rationed at `TEXT_LAYOUT_BUDGET`, so
            // the words vanished and trickled back over many frames. Measured with
            // `--demo reproject-cost` on a 1,002-item board before this line existed:
            // *"one item moved and 1002 of 1002 were restamped, so 0 cached layout(s)
            // survived"*.
            //
            // The comparison is everything a cached layout can depend on, not just the
            // item: `bounds` and `z` are derived here and a cache keyed on the item alone
            // would keep a stale layout when a *neighbour's* move changed a connector's
            // extent. `parent` is in for the same reason — `clipped_by_frame` reads it.
            //
            // Deliberately **not** an optimisation of the read: every item is still
            // decoded out of the CRDT, because that is how we find out whether it changed.
            // What this saves is the reshaping downstream, which is the expensive half by
            // a wide margin.
            let unchanged = self.items.get(&scene_id).filter(|was| {
                was.doc_id == item.id
                    && was.parent == parent
                    && was.bounds == bounds
                    && was.z == z
                    && was.item == item
            });
            let stamp = unchanged.map_or(generation, |was| was.generation);
            items.insert(
                scene_id,
                Projected { doc_id: item.id, parent, bounds, z, item, generation: stamp },
            );
        }

        self.content = items.values().map(|p| p.bounds).reduce(union);
        self.scene = Scene::from_items(items.iter().map(|(id, projected)| {
            SceneItem::solid_quad(*id, projected.bounds, projected.z, dominant_color(projected))
        }));
        self.items = items;
        self.by_doc = by_doc;
        self.generation = generation;
        self.shed = false;
        Ok(())
    }

    /// Moves one item's box, keeping the R-tree in step.
    ///
    /// The drag path, and the reason a drag costs nothing on a large board.
    /// `Scene::set_bounds` is a remove-and-reinsert in the index — an `O(log n)` pair
    /// — where a rebuild would be `O(n log n)` over the whole board for one item that
    /// moved.
    ///
    /// **A pure translation translates the box rather than recomputing it.** Three
    /// kinds have bounds that are not their placement: an ink stroke is half a
    /// thickness wider on every side, a connector's box comes from where it actually
    /// runs, and both are lost by [`placement_bounds`]. Sliding the existing box by
    /// the same delta keeps all three exact for the only edit a drag makes, and the
    /// rebuild on commit settles anything else.
    pub fn moved(&mut self, id: SceneId, placement: Placement) -> bool {
        let Some(projected) = self.items.get_mut(&id) else {
            return false;
        };
        let was = projected.item.placement;
        let translated = was.width == placement.width
            && was.height == placement.height
            && was.scale == placement.scale
            && was.rotation == placement.rotation;

        projected.item.placement = placement;
        projected.bounds = if translated {
            let (dx, dy) = (placement.x - was.x, placement.y - was.y);
            WorldRect::from_corners(
                WorldPoint::new(projected.bounds.min.x + dx, projected.bounds.min.y + dy),
                WorldPoint::new(projected.bounds.max.x + dx, projected.bounds.max.y + dy),
            )
        } else {
            placement_bounds(&placement)
        };
        let bounds = projected.bounds;
        self.content = self.content.map(|c| union(c, bounds));

        // A **resize or a rotation** has to bump the generation; a translation must not.
        //
        // Caches keyed on the generation — laid-out glyphs above all — cannot otherwise
        // notice. A sticky's text is auto-fitted to its box, and `crate::text`'s only
        // staleness key is this number, so dragging a corner from 200×200 to 600×200
        // grew the quad, grew the ring, updated the R-tree, and kept the text laid out
        // for the old box until the button came up, when the commit's rebuild finally
        // moved the generation and it snapped. Same for a text item's wrap width and a
        // frame's title size.
        //
        // A pure translation genuinely changes no layout, and bumping it there would
        // re-shape every visible block on every frame of every drag — which is the cost
        // this whole method exists to avoid.
        if !translated {
            self.generation = self.generation.wrapping_add(1);
        }
        self.scene.set_bounds(id, bounds)
    }

    /// Replaces one item's text without touching the index.
    ///
    /// Text does not change an item's box — a sticky auto-fits its words into the box
    /// it already has — so this is a field write and nothing more. It exists so that
    /// typing shows on the canvas at the same cost as typing into any other field,
    /// rather than reprojecting the whole document per keystroke.
    pub fn retext(&mut self, id: SceneId, text: vellum_doc::StyledText) -> bool {
        let Some(projected) = self.items.get_mut(&id) else {
            return false;
        };
        match &mut projected.item.kind {
            ItemKind::Sticky { text: slot, .. }
            | ItemKind::Text { text: slot }
            | ItemKind::Shape { text: slot, .. }
            | ItemKind::Frame { title: slot, .. }
            // An agent's role and a note's title are the item's own text, so they type on
            // the canvas at the same cost as every other field. Leaving them out is not a
            // missing feature but a *silently half-working* one: `ItemKind::text` already
            // answers for both, so the caret opens and accepts keystrokes, and only the
            // live redraw is absent — the words appear a gesture late, on the next
            // reprojection. That is the worst kind of gap, because it looks like lag.
            | ItemKind::Agent { label: slot, .. }
            | ItemKind::AgentNote { title: slot, .. } => {
                *slot = text;
                // Caches keyed on the projection's generation — laid-out glyphs above
                // all — have to notice, or the old words stay on screen.
                self.generation = self.generation.wrapping_add(1);
                true
            }
            _ => false,
        }
    }

    /// A scene id for `doc_id`, reusing the one it already had.
    fn intern(&mut self, doc_id: DocId) -> SceneId {
        if let Some(existing) = self.by_doc.get(&doc_id) {
            return *existing;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.by_doc.insert(doc_id, id);
        id
    }
}

/// The enclosing box of a placed item, rotation and scale applied.
pub fn placement_bounds(placement: &Placement) -> WorldRect {
    let (width, height) = placement.scaled_size();
    let (half_w, half_h) = if placement.rotation == 0.0 {
        (width / 2.0, height / 2.0)
    } else {
        // The enclosing box of a rotated rectangle: each axis picks up a projection
        // of both of the rectangle's own axes.
        let (sin, cos) = placement.rotation.to_radians().sin_cos();
        (
            (width * cos.abs() + height * sin.abs()) / 2.0,
            (width * sin.abs() + height * cos.abs()) / 2.0,
        )
    };
    WorldRect::from_corners(
        WorldPoint::new(placement.x - half_w, placement.y - half_h),
        WorldPoint::new(placement.x + half_w, placement.y + half_h),
    )
}

/// Bounds for one item, including the kinds whose extent is not their placement.
fn item_bounds(item: &Item, placements: &HashMap<DocId, Placement>) -> WorldRect {
    match &item.kind {
        // A stroke's placement is the box through its points; the drawn line is half
        // a thickness wider on every side, and culling that away clips the ends of
        // every stroke crossing the viewport edge.
        ItemKind::Ink { thickness, .. } => placement_bounds(&item.placement)
            .inflate((thickness * item.placement.scale).max(0.0) / 2.0),
        ItemKind::Connector { thickness, .. } => {
            let routed = connector::route(
                &item.kind,
                &item.placement,
                |target| placements.get(&target).copied(),
                &Router::default(),
                &[],
            );
            match routed {
                Some(routed) => {
                    let rect = routed.path.stroke_bounds(routed.style.thickness.max(*thickness));
                    WorldRect::from_corners(
                        WorldPoint::new(rect.min.x, rect.min.y),
                        WorldPoint::new(rect.max.x, rect.max.y),
                    )
                }
                None => placement_bounds(&item.placement),
            }
        }
        _ => placement_bounds(&item.placement),
    }
}

/// The colour the scene layer's own quad payload carries.
///
/// Only a fallback: the renderer reads [`Projected::item`] and draws the real thing.
/// It exists so that a consumer of the scene alone — a minimap, a thumbnail, a
/// debug view — shows a board rather than a field of identical grey boxes.
fn dominant_color(projected: &Projected) -> [f32; 4] {
    // The scene's own payload is theme-independent, because a scene outlives a theme
    // switch and nothing re-projects for one. Consumers that *do* know the theme —
    // the minimap — ask for it by name.
    swatch(projected, Theme::LIGHT).into()
}

/// The one colour that stands for an item when it is too small to draw properly.
///
/// The minimap's whole job is making your own board recognisable at a glance, and on
/// the reference board that recognition is the yellow field of stickies against the
/// dark mass of ink. A field of identical grey boxes would be a map of nothing.
/// An item's depth, from its position in the document's pre-order walk.
///
/// Frames are pushed into a band of their own, **behind every other kind**, instead of
/// taking whatever position the document gives them. A frame is the surface a region of
/// the board is arranged *on*; anything painting over its contents has stopped being a
/// frame and become a lid. Document order alone does not deliver that — [`Board::add`]
/// puts a new item on top of its siblings, so drawing a frame around work that already
/// existed hid all of it, and an import interleaves frames wherever Miro listed them.
///
/// Done here rather than in the painter's sort so that **one** number drives both paint
/// order and [`vellum_scene::Scene::hit_test`]. Reordering only the painter would draw
/// a frame behind a sticky while the hit-test still answered "frame", and the click
/// would land on the thing underneath the one you can see.
///
/// The bands cannot overlap: `index` is in `0..total`, so a frame lands in
/// `-total..0` and everything else in `0..total`. Relative order inside each band is
/// untouched, which is what keeps a frame nested in another frame from vanishing
/// beneath its parent.
fn depth(item: &vellum_doc::Item, index: i32, total: i32) -> i32 {
    match item.kind {
        ItemKind::Frame { .. } => index - total,
        _ => index,
    }
}

pub fn swatch(projected: &Projected, theme: Theme) -> Rgba {
    let color = match &projected.item.kind {
        ItemKind::Sticky { background, .. } => background.map_or(theme.sticky, theme::convert),
        ItemKind::Ink { color, .. } | ItemKind::Connector { color, .. } => {
            color.map_or(theme.stroke, theme::convert)
        }
        // The same token the canvas draws a frame in. Reading `surface` here instead
        // put a frame on the minimap in a colour it is not on the board.
        ItemKind::Frame { .. } => {
            projected.item.style.fill.map_or(theme.frame_fill, theme::convert)
        }
        ItemKind::Shape { .. } => projected.item.style.fill.map_or(theme.surface, theme::convert),
        ItemKind::Text { .. } | ItemKind::Group => Rgba::TRANSPARENT,

        // The four Agent Canvas kinds, named rather than left to the catch-all — and this
        // is a **defect fix, not a preference**. All four draw their card in `theme.surface`
        // on the board, which is right there and useless here: `push_minimap` paints its own
        // panel in `surface` at 0.92, so a surface-coloured item is a white rectangle on a
        // white plate. They were drawn, every frame, and could not be seen — reported as
        // *"agents dont show up on the minimap"* against a screenshot of a blank map with two
        // agents plainly on the board.
        //
        // The colours are the ones the layer already wears, so the map and the canvas say the
        // same thing: an agent is the accent (its status dot and its links are), a note is a
        // note, and a file tree and a browser are content rather than actors.
        ItemKind::Agent { .. } => theme.accent,
        ItemKind::AgentNote { .. } => theme.sticky,
        ItemKind::FileTree { .. } | ItemKind::Browser { .. } => theme.text_muted,

        // ⚠ Anything landing here is drawn in the same colour as the map's own panel, i.e.
        // invisible. That is tolerable only for a kind whose *job* is to be a backdrop — a
        // table, a kanban and a chart are white cards and read as absence on the map. Adding
        // a kind and leaving it to this arm is how the bug above happened; give it a colour.
        _ => theme.surface,
    };
    color.with_alpha(color.a * projected.opacity())
}

fn union(a: WorldRect, b: WorldRect) -> WorldRect {
    WorldRect::from_corners(
        WorldPoint::new(a.min.x.min(b.min.x), a.min.y.min(b.min.y)),
        WorldPoint::new(a.max.x.max(b.max.x), a.max.y.max(b.max.y)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{ConnectorEnd, Dash, NewItem, Point, Routing, StyledText};
    use vellum_scene::{Camera, ScreenSize};

    fn sticky(x: f64, y: f64) -> NewItem {
        NewItem::new(
            ItemKind::Sticky { text: StyledText::plain("fan"), background: None },
            Placement::new(x, y, 199.0, 228.0),
        )
    }

    /// Both halves matter, and the second is the one that keeps this honest.
    ///
    /// Carrying an unchanged item's generation forward is what stops one edit throwing
    /// away every laid-out block on the board. The hazard of getting it wrong is silent
    /// and worse than the slowness it fixes: an item whose generation survives a change it
    /// should not survive draws **the previous text, for ever**, and no assertion about
    /// item content would catch it because the document is correct — only the picture is
    /// wrong.
    ///
    /// So this asserts the bystander keeps its stamp *and* that each kind of change moves
    /// the changed item's. A/B'd both ways: with the carry-forward removed the first
    /// assertion fails, and with the comparison weakened to `doc_id` alone the later ones
    /// do. A test that only checked the first would pass on a build that never
    /// invalidated anything.
    #[test]
    fn only_the_item_that_changed_loses_its_cached_layout() {
        let mut board = Board::new();
        let moved = board.add(sticky(0.0, 0.0)).unwrap();
        let bystander = board.add(sticky(500.0, 0.0)).unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let stamp = |projection: &Projection, id| {
            let scene = projection.scene_id(id).expect("interned");
            projection.get(scene).expect("projected").generation
        };
        let (was_moved, was_bystander) =
            (stamp(&projection, moved), stamp(&projection, bystander));

        // A move.
        let mut placement = board.item(moved).unwrap().placement;
        placement.x += 1.0;
        board.set_placement(moved, placement).unwrap();
        projection.rebuild(&board).unwrap();
        assert_ne!(stamp(&projection, moved), was_moved, "a moved item kept its stamp");
        assert_eq!(
            stamp(&projection, bystander),
            was_bystander,
            "an untouched item lost its cached layout because a neighbour moved"
        );

        // A content change, which is the case a bounds-only comparison would miss: the
        // box is identical and every word in it is different.
        let was = stamp(&projection, bystander);
        board.set_text(bystander, StyledText::plain("different words entirely")).unwrap();
        projection.rebuild(&board).unwrap();
        assert_ne!(
            stamp(&projection, bystander),
            was,
            "retyped text kept a stamp, so the board would draw the old words for ever"
        );

        // A style change, which moves neither the box nor the words.
        let was = stamp(&projection, moved);
        let mut style = board.item(moved).unwrap().style;
        style.font_size = Some(48.0);
        board.set_style(moved, style).unwrap();
        projection.rebuild(&board).unwrap();
        assert_ne!(stamp(&projection, moved), was, "a restyled item kept its stamp");
    }

    /// The counter must keep advancing even when nothing changed, because it is what
    /// `Painter::sync` compares to decide whether to prune caches for items the board no
    /// longer holds. Carrying stamps forward must not quietly freeze it — a deleted item's
    /// layout and tessellated ink would stay resident for the life of the session.
    #[test]
    fn the_projections_own_generation_advances_even_when_no_item_does() {
        let mut board = Board::new();
        board.add(sticky(0.0, 0.0)).unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let before = projection.generation();
        projection.rebuild(&board).unwrap();
        assert_ne!(projection.generation(), before, "the prune counter stopped moving");
    }

    #[test]
    fn a_placement_is_centred_not_cornered() {
        // `vellum_doc::Placement` stores the item's centre, matching Miro's
        // `_position.offsetPx`. Reading it as a top-left corner puts every item on
        // the board half its own size out of place.
        let bounds = placement_bounds(&Placement::new(100.0, 50.0, 200.0, 120.0));
        assert_eq!(bounds.min, WorldPoint::new(0.0, -10.0));
        assert_eq!(bounds.max, WorldPoint::new(200.0, 110.0));
    }

    #[test]
    fn scale_grows_the_box_about_the_centre() {
        let mut placement = Placement::new(0.0, 0.0, 100.0, 100.0);
        placement.scale = 2.0;
        let bounds = placement_bounds(&placement);
        assert_eq!(bounds.width(), 200.0);
        assert_eq!(bounds.center(), WorldPoint::ORIGIN);
    }

    #[test]
    fn rotation_produces_the_enclosing_box() {
        let mut placement = Placement::new(0.0, 0.0, 200.0, 100.0);
        placement.rotation = 90.0;
        let bounds = placement_bounds(&placement);
        assert!((bounds.width() - 100.0).abs() < 1e-9, "{}", bounds.width());
        assert!((bounds.height() - 200.0).abs() < 1e-9, "{}", bounds.height());

        placement.rotation = 45.0;
        let diagonal = placement_bounds(&placement);
        let expected = (200.0 + 100.0) * std::f64::consts::FRAC_1_SQRT_2;
        assert!((diagonal.width() - expected).abs() < 1e-9, "{}", diagonal.width());
    }

    #[test]
    fn a_board_projects_into_a_queryable_scene() {
        let mut board = Board::new();
        board.add(sticky(0.0, 0.0)).unwrap();
        board.add(sticky(5_000.0, 5_000.0)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        assert_eq!(projection.len(), 2);
        assert_eq!(projection.scene().len(), 2);

        let camera = Camera::new(ScreenSize::new(1280.0, 720.0));
        let visible: Vec<_> = projection.scene().query_viewport(&camera).collect();
        assert_eq!(visible.len(), 1, "culling let a distant sticky through");
    }

    /// The document's depth-first order is the paint order, so a frame draws before
    /// what it contains. Getting this backwards hides every item inside a frame.
    #[test]
    fn paint_order_follows_the_document_tree() {
        let mut board = Board::new();
        let frame = board
            .add(NewItem::new(
                ItemKind::Frame {
                    title: StyledText::plain("Engine bay"),
                    order: None,
                    speaker_notes: None,
                },
                Placement::new(0.0, 0.0, 1000.0, 800.0),
            ))
            .unwrap();
        let child = board.add(sticky(0.0, 0.0).with_parent(frame)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let frame_id = projection.scene_id(frame).unwrap();
        let child_id = projection.scene_id(child).unwrap();
        assert!(
            projection.get(frame_id).unwrap().z < projection.get(child_id).unwrap().z,
            "a frame drew over its own contents"
        );
        assert_eq!(projection.get(child_id).unwrap().parent, Some(frame_id));
    }

    /// The case document order gets wrong on its own: a frame drawn *after* the work
    /// it surrounds. `Board::add` puts a new item on top of its siblings, so before
    /// frames had a band of their own this covered the sticky completely — which is
    /// what an imported board looked like, since Miro interleaves frames wherever it
    /// listed them.
    #[test]
    fn a_frame_added_last_still_draws_behind_what_it_surrounds() {
        let mut board = Board::new();
        let note = board.add(sticky(0.0, 0.0)).unwrap();
        let frame = board
            .add(NewItem::new(
                ItemKind::Frame {
                    title: StyledText::plain("Engine bay"),
                    order: None,
                    speaker_notes: None,
                },
                Placement::new(0.0, 0.0, 1000.0, 800.0),
            ))
            .unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let frame_id = projection.scene_id(frame).unwrap();
        let note_id = projection.scene_id(note).unwrap();
        assert!(
            projection.get(frame_id).unwrap().z < projection.get(note_id).unwrap().z,
            "a frame added after the sticky painted over it",
        );
    }

    /// Frames go behind everything, but not behind each other arbitrarily — an inner
    /// frame has to stay on top of the one containing it.
    #[test]
    fn frames_still_stack_among_themselves_in_document_order() {
        let mut board = Board::new();
        let outer = board
            .add(NewItem::new(
                ItemKind::Frame {
                    title: StyledText::plain("Outer"),
                    order: None,
                    speaker_notes: None,
                },
                Placement::new(0.0, 0.0, 1000.0, 800.0),
            ))
            .unwrap();
        let inner = board
            .add(
                NewItem::new(
                    ItemKind::Frame {
                        title: StyledText::plain("Inner"),
                        order: None,
                        speaker_notes: None,
                    },
                    Placement::new(0.0, 0.0, 400.0, 300.0),
                )
                .with_parent(outer),
            )
            .unwrap();
        let note = board.add(sticky(0.0, 0.0).with_parent(inner)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let outer_z = projection.get(projection.scene_id(outer).unwrap()).unwrap().z;
        let inner_z = projection.get(projection.scene_id(inner).unwrap()).unwrap().z;
        let note_z = projection.get(projection.scene_id(note).unwrap()).unwrap().z;
        assert!(outer_z < inner_z, "the inner frame vanished under its parent");
        assert!(inner_z < note_z, "a frame drew over its own contents");
    }

    /// Scene ids outlive a rebuild, so a selection or a texture binding survives an
    /// edit somewhere else on the board.
    #[test]
    fn rebuilding_keeps_the_scene_ids_of_surviving_items() {
        let mut board = Board::new();
        let first = board.add(sticky(0.0, 0.0)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let id = projection.scene_id(first).unwrap();
        let generation = projection.generation();

        board.add(sticky(400.0, 0.0)).unwrap();
        projection.rebuild(&board).unwrap();

        assert_eq!(projection.scene_id(first), Some(id));
        assert_eq!(projection.len(), 2);
        assert_ne!(projection.generation(), generation);
    }

    /// The whole point of shedding: the items go, the *identity* stays. A parked
    /// board's `Editor::selection` is a `Vec<SceneId>`, so renumbering on the way back
    /// would silently point it at different items — which is worse than losing it,
    /// because nothing would look wrong.
    #[test]
    fn shedding_keeps_scene_ids_stable() {
        let mut board = Board::new();
        let first = board.add(sticky(0.0, 0.0)).unwrap();
        let second = board.add(sticky(400.0, 0.0)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let before = [
            projection.scene_id(first).unwrap(),
            projection.scene_id(second).unwrap(),
        ];
        assert!(!projection.is_shed());

        projection.shed();
        assert!(projection.is_shed());
        assert_eq!(projection.len(), 0, "shedding kept the items it exists to drop");
        assert_eq!(projection.content_bounds(), None);
        assert_eq!(projection.scene().len(), 0, "shedding kept the R-tree");

        projection.rebuild(&board).unwrap();

        assert!(!projection.is_shed(), "a rebuild left the projection marked shed");
        assert_eq!(projection.len(), 2);
        assert_eq!(
            [projection.scene_id(first).unwrap(), projection.scene_id(second).unwrap()],
            before,
            "a shed board came back with its items renumbered"
        );
    }

    /// Shedding is not an edit. Anything keyed on the generation — laid-out glyphs
    /// above all — must not be invalidated just because a tab went to the background.
    #[test]
    fn shedding_does_not_bump_the_generation() {
        let mut board = Board::new();
        board.add(sticky(0.0, 0.0)).unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let generation = projection.generation();
        projection.shed();
        assert_eq!(projection.generation(), generation);
    }

    #[test]
    fn a_removed_item_leaves_the_scene() {
        let mut board = Board::new();
        let doomed = board.add(sticky(0.0, 0.0)).unwrap();
        board.add(sticky(400.0, 0.0)).unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        board.remove(doomed).unwrap();
        projection.rebuild(&board).unwrap();

        assert_eq!(projection.len(), 1);
        assert_eq!(projection.scene().len(), 1);
        assert!(projection.scene().hit_test(WorldPoint::ORIGIN).is_none());
    }

    /// The incremental path a drag takes. The index has to follow, or the item stops
    /// being hit-testable at its new position and stays clickable at the old one.
    #[test]
    fn moving_an_item_moves_it_in_the_index() {
        let mut board = Board::new();
        let id = board.add(sticky(0.0, 0.0)).unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let scene_id = projection.scene_id(id).unwrap();

        assert!(projection.moved(scene_id, Placement::new(9_000.0, 9_000.0, 199.0, 228.0)));

        assert!(projection.scene().hit_test(WorldPoint::ORIGIN).is_none());
        assert_eq!(
            projection.scene().hit_test(WorldPoint::new(9_000.0, 9_000.0)),
            Some(scene_id)
        );
        assert!(!projection.moved(9_999, Placement::default()));
    }

    /// A pure translation has to translate the box, not recompute it from the
    /// placement: an ink stroke's box is half a thickness wider on every side, and
    /// `placement_bounds` does not know that. Dragging one used to clip its ends.
    #[test]
    fn dragging_a_stroke_keeps_the_width_its_bounds_were_built_with() {
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                ItemKind::Ink {
                    points: vec![Point::new(-50.0, 0.0), Point::new(50.0, 0.0)],
                    color: None,
                    thickness: 20.0,
                },
                Placement::new(0.0, 0.0, 100.0, 0.0),
            ))
            .unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let scene = projection.scene_id(id).unwrap();
        let before = projection.get(scene).unwrap().bounds;

        assert!(projection.moved(scene, Placement::new(600.0, 300.0, 100.0, 0.0)));
        let after = projection.get(scene).unwrap().bounds;

        assert_eq!(after.width(), before.width());
        assert_eq!(after.height(), before.height(), "the stroke width was dropped");
        assert_eq!(after.center(), WorldPoint::new(600.0, 300.0));

        // A resize is not a translation, so it *is* recomputed.
        assert!(projection.moved(scene, Placement::new(600.0, 300.0, 400.0, 0.0)));
        assert_eq!(projection.get(scene).unwrap().bounds.width(), 400.0);
    }

    /// Typing is a field write, not a reprojection — but the generation still has to
    /// move, or the laid-out glyphs are never re-shaped and the old words stay up.
    #[test]
    fn retexting_replaces_the_words_and_invalidates_the_layout() {
        let mut board = Board::new();
        let id = board.add(sticky(0.0, 0.0)).unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let scene = projection.scene_id(id).unwrap();
        let generation = projection.generation();
        let bounds = projection.get(scene).unwrap().bounds;

        assert!(projection.retext(scene, StyledText::plain("water pump")));
        let projected = projection.get(scene).unwrap();
        assert_eq!(projected.item.kind.text().unwrap().to_plain(), "water pump");
        assert_eq!(projected.bounds, bounds, "text must not move the box");
        assert_ne!(projection.generation(), generation);

        // A kind with nowhere to put words says so rather than silently succeeding.
        let image = board
            .add(NewItem::new(
                ItemKind::Image { asset_id: "h".into(), crop: None },
                Placement::new(0.0, 0.0, 10.0, 10.0),
            ))
            .unwrap();
        projection.rebuild(&board).unwrap();
        let image = projection.scene_id(image).unwrap();
        assert!(!projection.retext(image, StyledText::plain("no")));
    }

    /// A stroke is drawn half a thickness wider than its point cloud on every side.
    #[test]
    fn ink_bounds_include_the_stroke_width() {
        let mut board = Board::new();
        board
            .add(NewItem::new(
                ItemKind::Ink {
                    points: vec![Point::new(-50.0, 0.0), Point::new(50.0, 0.0)],
                    color: None,
                    thickness: 20.0,
                },
                Placement::new(0.0, 0.0, 100.0, 0.0),
            ))
            .unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let bounds = projection.iter().next().unwrap().1.bounds;
        assert_eq!(bounds.height(), 20.0, "a flat stroke has no drawn height");
        assert_eq!(bounds.width(), 120.0);
    }

    /// A connector's box comes from where it actually runs, not from a placement it
    /// does not have. Culling on the placement alone makes lines disappear.
    #[test]
    fn a_connector_is_bounded_by_its_live_endpoints() {
        let mut board = Board::new();
        let left = board
            .add(NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(0.0, 0.0, 100.0, 100.0),
            ))
            .unwrap();
        let right = board
            .add(NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(1_000.0, 0.0, 100.0, 100.0),
            ))
            .unwrap();
        let line = board
            .add(NewItem::new(
                ItemKind::Connector {
                    start: ConnectorEnd::bound(left, ConnectorEnd::RIGHT),
                    end: ConnectorEnd::bound(right, ConnectorEnd::LEFT),
                    routing: Routing::Straight,
                    dash: Dash::Solid,
                    thickness: 2.0,
                    color: None,
                    captions: Vec::new(),
                },
                Placement::new(0.0, 0.0, 0.0, 0.0),
            ))
            .unwrap();

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let bounds = projection.get(projection.scene_id(line).unwrap()).unwrap().bounds;

        assert!((bounds.min.x - 50.0).abs() < 2.0, "{bounds:?}");
        assert!((bounds.max.x - 950.0).abs() < 2.0, "{bounds:?}");
        assert!(bounds.height() >= 2.0, "the stroke width is missing from {bounds:?}");
    }

    #[test]
    fn content_bounds_cover_every_item_and_are_none_when_empty() {
        let mut projection = Projection::new();
        projection.rebuild(&Board::new()).unwrap();
        assert!(projection.content_bounds().is_none());

        let mut board = Board::new();
        board.add(sticky(0.0, 0.0)).unwrap();
        board.add(sticky(10_000.0, 4_000.0)).unwrap();
        projection.rebuild(&board).unwrap();

        let content = projection.content_bounds().unwrap();
        for (_, item) in projection.iter() {
            assert!(content.intersects(&item.bounds));
            assert!(content.min.x <= item.bounds.min.x && content.max.x >= item.bounds.max.x);
        }
    }

    #[test]
    fn the_scene_payload_carries_a_recognisable_colour() {
        let mut board = Board::new();
        board.add(sticky(0.0, 0.0)).unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let id = *projection.iter().next().unwrap().0;
        let vellum_scene::RenderPayload::SolidQuad { color } =
            projection.scene().get(id).unwrap().payload;
        assert_eq!(Rgba::from(color).pack(), Theme::LIGHT.sticky.pack());
    }

    /// Every Agent Canvas node has to be *visible* on the minimap, and the assertion has to
    /// be about that rather than about which colour was chosen.
    ///
    /// `push_minimap` paints its own panel in `theme.surface`, so the failure this guards is
    /// not "the wrong colour" but "the same colour as the plate underneath" — an item drawn
    /// every frame that nobody can see. A test spelled `assert_eq!(swatch, accent)` would pass
    /// on a build that painted the map's panel in the accent too; a **delta against
    /// `surface`** is the thing that is actually true, and it is what fails on the old
    /// `_ => theme.surface` arm.
    ///
    /// The floor is `theme.rs`'s own grid rule arrived at from the other side: 8/255 is where
    /// a one-pixel difference stops being a difference. A minimap item is a few pixels across,
    /// so this asks for a great deal more than that on at least one channel.
    #[test]
    fn every_agent_node_is_visible_against_the_minimaps_own_panel() {
        use vellum_doc::StyledText;

        let kinds = [
            ("agent", ItemKind::Agent { model: String::new(), label: StyledText::plain("A") }),
            ("note", ItemKind::AgentNote { model: String::new(), title: StyledText::plain("N") }),
            ("file tree", ItemKind::FileTree { model: String::new() }),
            ("browser", ItemKind::Browser { model: String::new() }),
        ];

        let mut board = Board::new();
        let mut added = Vec::new();
        for (name, kind) in kinds {
            // Looked up by id rather than by walking `projection.iter()` in step: that
            // iteration order is the scene's, not the order things were added, so a zip
            // would be asserting about whichever node happened to come out first.
            added.push((name, board.add(NewItem::new(kind, Placement::new(0.0, 0.0, 520.0, 400.0))).unwrap()));
        }
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let panel = Theme::LIGHT.surface;
        let mut seen = Vec::new();
        for (name, doc) in added {
            let scene = projection.scene_id(doc).unwrap();
            let projected = projection.get(scene).unwrap();
            let colour = swatch(projected, Theme::LIGHT);
            let delta = [
                (colour.r - panel.r).abs(),
                (colour.g - panel.g).abs(),
                (colour.b - panel.b).abs(),
            ];
            let worst = delta[0].max(delta[1]).max(delta[2]);
            assert!(
                worst > 0.15,
                "a {name} node draws at {delta:?} from the minimap's own panel — \
                 that is a rectangle painted every frame that nobody can see"
            );
            assert!(colour.a > 0.0, "a {name} node is skipped by the minimap's alpha test");
            seen.push(colour.pack());
        }

        // An agent must not read as a note. Two kinds sharing a colour is a map that cannot
        // be used for the one thing a minimap is for — recognising the shape of your board.
        assert_ne!(seen[0], seen[1], "an agent and a note are the same colour on the map");
    }
}
