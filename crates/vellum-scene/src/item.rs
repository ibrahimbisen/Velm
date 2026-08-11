//! What lives on the board, from the spatial layer's point of view.
//!
//! Deliberately thin. The document layer owns the truth — Miro's rich text, styles
//! and parent hierarchy — while a [`SceneItem`] carries only what culling,
//! hit-testing and the instanced renderer need: an identity, a box, a paint order
//! and a payload. Keeping it small matters because the renderer walks these every
//! frame and a 100k-item board should stay in cache.

use crate::geometry::WorldRect;

/// Stable identity for a scene item, assigned by the document layer.
///
/// `u64` rather than an index so that removing an item never renumbers the others —
/// selection, undo and the eventual CRDT all hold onto ids across edits.
pub type ItemId = u64;

/// What the renderer should draw for an item.
///
/// Only one variant today. Text and images are the next two, and they arrive with
/// the glyph atlas and the texture-residency work respectively; the enum exists now
/// so adding them is an additive change to the renderer's match rather than a
/// redesign of the scene.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RenderPayload {
    /// A flat fill covering the item's whole bounding box.
    ///
    /// Colour is straight (non-premultiplied) RGBA in the surface's colour space,
    /// which is what the instanced quad pipeline uploads verbatim.
    SolidQuad { color: [f32; 4] },
}

/// One drawable thing on the board.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneItem {
    pub id: ItemId,
    /// Axis-aligned world-space bounds. Rotation is not represented here: a rotated
    /// widget stores its rotation in the document and contributes its *enclosing*
    /// axis-aligned box to the index, so culling stays a cheap AABB test and only
    /// hit-testing pays for the exact shape.
    pub bounds: WorldRect,
    /// Painter's order. Higher draws later, so higher is nearer the viewer.
    /// Signed so an item can be pushed behind everything without renumbering.
    pub z: i32,
    pub payload: RenderPayload,
}

impl SceneItem {
    /// Convenience for the common case: a coloured rectangle.
    pub fn solid_quad(id: ItemId, bounds: WorldRect, z: i32, color: [f32; 4]) -> Self {
        Self {
            id,
            bounds,
            z,
            payload: RenderPayload::SolidQuad { color },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::WorldPoint;

    #[test]
    fn solid_quad_carries_its_colour_and_box() {
        let item = SceneItem::solid_quad(
            7,
            WorldRect::from_origin_size(WorldPoint::new(10.0, 20.0), 100.0, 50.0),
            -3,
            [1.0, 0.97, 0.62, 1.0],
        );
        assert_eq!(item.id, 7);
        assert_eq!(item.z, -3);
        assert_eq!(item.bounds.width(), 100.0);
        assert_eq!(
            item.payload,
            RenderPayload::SolidQuad { color: [1.0, 0.97, 0.62, 1.0] }
        );
    }
}
