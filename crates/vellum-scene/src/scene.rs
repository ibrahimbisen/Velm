//! The spatially indexed set of items on a board.
//!
//! The single most important property of this module is that a frame costs what is
//! *on screen*, not what is on the board. Miro degrades on large boards because its
//! per-frame work grows with the document; an R-tree makes the viewport query
//! `O(log n + k)` in the number of visible items, so a 100k-widget board and a
//! 100-widget board cost the same to pan around. `culling_cost_tracks_the_viewport`
//! is the test that holds us to it.
//!
//! # Why two structures
//!
//! [`rstar`] cannot update an element's position in place, and its `remove` locates
//! an element by envelope, so it needs the old bounds to find what to delete. The
//! authoritative items therefore live in a `HashMap` keyed by id, and the R-tree
//! holds only `(id, bounds)` pairs. That keeps the tree nodes small — which is what
//! the query actually walks — and makes "remove by id" and "move an item" both
//! `O(log n)` rather than a linear scan.

use std::collections::HashMap;

use rstar::{AABB, RTree, RTreeObject};

use crate::camera::Camera;
use crate::geometry::{WorldPoint, WorldRect};
use crate::item::{ItemId, SceneItem};

/// What the R-tree stores: an identity and a box, nothing else.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SpatialEntry {
    id: ItemId,
    bounds: WorldRect,
}

impl RTreeObject for SpatialEntry {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.bounds.to_aabb()
    }
}

/// A board's worth of items, indexed for viewport queries and hit-testing.
#[derive(Debug, Default)]
pub struct Scene {
    items: HashMap<ItemId, SceneItem>,
    index: RTree<SpatialEntry>,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a scene in one shot.
    ///
    /// Prefer this to repeated [`Scene::insert`] when the whole board is known —
    /// rstar's bulk load builds a balanced tree in `O(n log n)` and produces
    /// noticeably tighter nodes than the same items inserted one at a time, which
    /// directly reduces how many nodes a viewport query has to visit.
    pub fn from_items(items: impl IntoIterator<Item = SceneItem>) -> Self {
        let items: HashMap<ItemId, SceneItem> = items.into_iter().map(|i| (i.id, i)).collect();
        let entries = items
            .values()
            .map(|i| SpatialEntry {
                id: i.id,
                bounds: i.bounds,
            })
            .collect();
        Self {
            items,
            index: RTree::bulk_load(entries),
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains(&self, id: ItemId) -> bool {
        self.items.contains_key(&id)
    }

    pub fn get(&self, id: ItemId) -> Option<&SceneItem> {
        self.items.get(&id)
    }

    /// Every item, in unspecified order. For iterating the whole board — never for
    /// drawing a frame, which must go through [`Scene::query_viewport`].
    pub fn iter(&self) -> impl Iterator<Item = &SceneItem> {
        self.items.values()
    }

    /// Adds an item, replacing and returning any item with the same id.
    pub fn insert(&mut self, item: SceneItem) -> Option<SceneItem> {
        let replaced = self.remove(item.id);
        self.index.insert(SpatialEntry {
            id: item.id,
            bounds: item.bounds,
        });
        self.items.insert(item.id, item);
        replaced
    }

    /// Removes an item by id, returning it. `None` if it was not there.
    pub fn remove(&mut self, id: ItemId) -> Option<SceneItem> {
        let item = self.items.remove(&id)?;
        // The old bounds are what let rstar descend to the right leaf instead of
        // scanning; this is why the map is consulted first.
        let removed = self.index.remove(&SpatialEntry {
            id,
            bounds: item.bounds,
        });
        debug_assert!(
            removed.is_some(),
            "scene index and item map disagreed about item {id}"
        );
        Some(item)
    }

    /// Moves or resizes an item. Returns `false` if the id is unknown.
    ///
    /// Implemented as remove-then-insert because an R-tree's structure encodes
    /// position; mutating bounds in place would leave the parent nodes' envelopes
    /// stale and queries would start missing the item.
    pub fn set_bounds(&mut self, id: ItemId, bounds: WorldRect) -> bool {
        let Some(item) = self.items.get_mut(&id) else {
            return false;
        };
        let old = std::mem::replace(&mut item.bounds, bounds);
        if old == bounds {
            return true;
        }
        self.index.remove(&SpatialEntry { id, bounds: old });
        self.index.insert(SpatialEntry { id, bounds });
        true
    }

    /// Changes an item's paint order. No index work: `z` is not spatial.
    pub fn set_z(&mut self, id: ItemId, z: i32) -> bool {
        match self.items.get_mut(&id) {
            Some(item) => {
                item.z = z;
                true
            }
            None => false,
        }
    }

    /// Everything overlapping `rect`, in unspecified order.
    pub fn query_rect(&self, rect: WorldRect) -> impl Iterator<Item = &SceneItem> {
        self.index
            .locate_in_envelope_intersecting(rect.to_aabb())
            .filter_map(|entry| self.items.get(&entry.id))
    }

    /// Everything the camera can currently see, in unspecified order.
    ///
    /// The returned iterator borrows the scene but not the camera (`use<'s>`), so a
    /// caller can pass a temporary camera without the borrow outliving the call.
    pub fn query_viewport<'s>(&'s self, camera: &Camera) -> impl Iterator<Item = &'s SceneItem> + use<'s> {
        self.query_rect(camera.visible_world_rect())
    }

    /// Fills `out` with the visible items sorted back-to-front, ready to draw.
    ///
    /// Takes a caller-owned buffer so the render loop can reuse one allocation for
    /// the life of the app instead of building a `Vec` sixty times a second.
    pub fn collect_visible<'s>(&'s self, camera: &Camera, out: &mut Vec<&'s SceneItem>) {
        out.clear();
        out.extend(self.query_viewport(camera));
        // Ties broken by id so the draw order is deterministic frame to frame —
        // otherwise two coincident quads would flicker as HashMap order shifts.
        out.sort_unstable_by_key(|item| (item.z, item.id));
    }

    /// The item under a world point, or `None`.
    ///
    /// Returns the *topmost* by paint order, so it agrees with what the user can see;
    /// ties on `z` are broken by the higher id, matching [`Scene::collect_visible`]'s
    /// draw order so the last-drawn item is the one picked.
    pub fn hit_test(&self, point: WorldPoint) -> Option<ItemId> {
        self.hit_test_where(point, |_| true)
    }

    /// The topmost item at `point` that `accept` allows.
    ///
    /// The predicate is applied *before* "topmost", not after, so a rejected item is
    /// genuinely transparent: the click reaches whatever is under it. That is the whole
    /// reason this exists rather than a caller filtering `hit_test`'s answer — filtering
    /// afterwards turns a rejected item into a hole in the board, and a locked sticky lying
    /// over a frame would make the frame unclickable through it.
    ///
    /// `accept` sees only the id, because the scene holds geometry and z-order and knows
    /// nothing about locking, kinds or selection. Whatever the rule is, it lives with the
    /// caller that has the document.
    pub fn hit_test_where(
        &self,
        point: WorldPoint,
        accept: impl Fn(ItemId) -> bool,
    ) -> Option<ItemId> {
        // A degenerate box at the point turns "which envelopes contain this?" into
        // the same intersection query the viewport uses, so both paths agree on
        // whether the border of an item counts as inside it.
        let probe = AABB::from_point(point.to_array());
        self.index
            .locate_in_envelope_intersecting(probe)
            .filter_map(|entry| self.items.get(&entry.id))
            .filter(|item| item.bounds.contains(point))
            .filter(|item| accept(item.id))
            .max_by_key(|item| (item.z, item.id))
            .map(|item| item.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::Camera;
    use crate::geometry::ScreenSize;
    use crate::item::RenderPayload;

    fn quad(id: ItemId, x: f64, y: f64, w: f64, h: f64, z: i32) -> SceneItem {
        SceneItem::solid_quad(
            id,
            WorldRect::from_origin_size(WorldPoint::new(x, y), w, h),
            z,
            [0.5, 0.5, 0.5, 1.0],
        )
    }

    /// A tiny deterministic PRNG so the big tests are reproducible without pulling
    /// in `rand`. SplitMix64 — good enough to scatter boxes, and it never changes.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Uniform in `[low, high)`.
        fn next_range(&mut self, low: f64, high: f64) -> f64 {
            let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
            low + unit * (high - low)
        }
    }

    fn scattered_scene(count: usize, extent: f64) -> Scene {
        let mut rng = SplitMix64(0x5EED);
        let items = (0..count).map(|i| {
            let x = rng.next_range(-extent, extent);
            let y = rng.next_range(-extent, extent);
            quad(i as ItemId, x, y, 120.0, 80.0, (i % 16) as i32)
        });
        Scene::from_items(items)
    }

    #[test]
    fn insert_get_and_remove() {
        let mut scene = Scene::new();
        assert!(scene.is_empty());

        scene.insert(quad(1, 0.0, 0.0, 10.0, 10.0, 0));
        scene.insert(quad(2, 50.0, 50.0, 10.0, 10.0, 0));
        assert_eq!(scene.len(), 2);
        assert!(scene.contains(1));
        assert_eq!(scene.get(2).unwrap().bounds.min, WorldPoint::new(50.0, 50.0));

        let removed = scene.remove(1).expect("item 1 should exist");
        assert_eq!(removed.id, 1);
        assert!(!scene.contains(1));
        assert_eq!(scene.len(), 1);
        assert!(scene.remove(1).is_none());
    }

    #[test]
    fn inserting_the_same_id_replaces_rather_than_duplicates() {
        let mut scene = Scene::new();
        scene.insert(quad(1, 0.0, 0.0, 10.0, 10.0, 0));
        let replaced = scene.insert(quad(1, 900.0, 900.0, 10.0, 10.0, 5));

        assert_eq!(replaced.unwrap().bounds.min, WorldPoint::ORIGIN);
        assert_eq!(scene.len(), 1);
        // The stale entry must be gone from the index too, not just from the map.
        assert_eq!(
            scene
                .query_rect(WorldRect::from_origin_size(WorldPoint::ORIGIN, 20.0, 20.0))
                .count(),
            0
        );
        assert_eq!(scene.hit_test(WorldPoint::new(905.0, 905.0)), Some(1));
    }

    #[test]
    fn removed_items_leave_no_trace_in_the_index() {
        let mut scene = Scene::new();
        for i in 0..50 {
            scene.insert(quad(i, i as f64 * 10.0, 0.0, 8.0, 8.0, 0));
        }
        for i in (0..50).step_by(2) {
            scene.remove(i);
        }
        assert_eq!(scene.len(), 25);

        let all = WorldRect::from_origin_size(WorldPoint::new(-1000.0, -1000.0), 5000.0, 5000.0);
        let found: Vec<ItemId> = {
            let mut v: Vec<ItemId> = scene.query_rect(all).map(|i| i.id).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(found, (0..50).filter(|i| i % 2 == 1).collect::<Vec<_>>());
    }

    #[test]
    fn set_bounds_moves_the_item_in_the_index() {
        let mut scene = Scene::new();
        scene.insert(quad(1, 0.0, 0.0, 10.0, 10.0, 0));

        assert!(scene.set_bounds(1, WorldRect::from_origin_size(WorldPoint::new(500.0, 500.0), 10.0, 10.0)));

        assert!(scene.hit_test(WorldPoint::new(5.0, 5.0)).is_none(), "found at the old position");
        assert_eq!(scene.hit_test(WorldPoint::new(505.0, 505.0)), Some(1));
        assert_eq!(scene.get(1).unwrap().bounds.min, WorldPoint::new(500.0, 500.0));
        assert!(!scene.set_bounds(999, WorldRect::from_origin_size(WorldPoint::ORIGIN, 1.0, 1.0)));
    }

    /// Dragging emits a stream of `set_bounds` calls; the index has to survive all
    /// of them without leaking entries or losing the item.
    #[test]
    fn repeated_set_bounds_keeps_the_index_consistent() {
        let mut scene = scattered_scene(2_000, 20_000.0);
        let before = scene.len();

        for step in 0..500 {
            let x = step as f64 * 7.0;
            scene.set_bounds(42, WorldRect::from_origin_size(WorldPoint::new(x, x), 30.0, 30.0));
        }

        assert_eq!(scene.len(), before);
        let last = 499.0 * 7.0;
        assert_eq!(scene.hit_test(WorldPoint::new(last + 15.0, last + 15.0)), Some(42));
    }

    #[test]
    fn set_z_reorders_without_touching_the_index() {
        let mut scene = Scene::new();
        scene.insert(quad(1, 0.0, 0.0, 100.0, 100.0, 0));
        scene.insert(quad(2, 0.0, 0.0, 100.0, 100.0, 1));
        assert_eq!(scene.hit_test(WorldPoint::new(50.0, 50.0)), Some(2));

        assert!(scene.set_z(1, 99));
        assert_eq!(scene.hit_test(WorldPoint::new(50.0, 50.0)), Some(1));
        assert!(!scene.set_z(404, 0));
    }

    #[test]
    fn hit_test_returns_the_topmost_item() {
        let mut scene = Scene::new();
        scene.insert(quad(10, 0.0, 0.0, 100.0, 100.0, 0));
        scene.insert(quad(11, 20.0, 20.0, 100.0, 100.0, 5));
        scene.insert(quad(12, 40.0, 40.0, 100.0, 100.0, 2));

        // Only item 10 covers this.
        assert_eq!(scene.hit_test(WorldPoint::new(5.0, 5.0)), Some(10));
        // 10 and 11 overlap; 11 has the higher z.
        assert_eq!(scene.hit_test(WorldPoint::new(25.0, 25.0)), Some(11));
        // All three overlap here; 11 still wins on z even though 12 was inserted last.
        assert_eq!(scene.hit_test(WorldPoint::new(50.0, 50.0)), Some(11));
        assert_eq!(scene.hit_test(WorldPoint::new(-1.0, -1.0)), None);
    }

    #[test]
    fn hit_test_breaks_z_ties_the_same_way_the_renderer_draws() {
        let mut scene = Scene::new();
        scene.insert(quad(1, 0.0, 0.0, 50.0, 50.0, 3));
        scene.insert(quad(2, 0.0, 0.0, 50.0, 50.0, 3));

        let camera = Camera::new(ScreenSize::new(800.0, 600.0));
        let mut visible = Vec::new();
        scene.collect_visible(&camera, &mut visible);

        let last_drawn = visible.last().unwrap().id;
        assert_eq!(scene.hit_test(WorldPoint::new(25.0, 25.0)), Some(last_drawn));
    }

    #[test]
    fn hit_test_includes_the_border() {
        let mut scene = Scene::new();
        scene.insert(quad(1, 0.0, 0.0, 10.0, 10.0, 0));
        assert_eq!(scene.hit_test(WorldPoint::new(0.0, 0.0)), Some(1));
        assert_eq!(scene.hit_test(WorldPoint::new(10.0, 10.0)), Some(1));
        assert_eq!(scene.hit_test(WorldPoint::new(10.0001, 5.0)), None);
    }

    #[test]
    fn hit_test_on_an_empty_scene_is_none() {
        assert_eq!(Scene::new().hit_test(WorldPoint::ORIGIN), None);
    }

    #[test]
    fn viewport_query_includes_partially_visible_items() {
        let mut scene = Scene::new();
        // Straddling the left edge of a 800x600 viewport centred on the origin,
        // whose visible rect is x in [-400, 400].
        scene.insert(quad(1, -450.0, 0.0, 100.0, 100.0, 0));
        scene.insert(quad(2, 1000.0, 0.0, 100.0, 100.0, 0));

        let camera = Camera::new(ScreenSize::new(800.0, 600.0));
        let ids: Vec<ItemId> = scene.query_viewport(&camera).map(|i| i.id).collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn collect_visible_is_sorted_back_to_front() {
        let mut scene = Scene::new();
        for (id, z) in [(1u64, 5i32), (2, -3), (3, 0), (4, 5)] {
            scene.insert(quad(id, 0.0, 0.0, 20.0, 20.0, z));
        }

        let camera = Camera::new(ScreenSize::new(800.0, 600.0));
        let mut out = Vec::new();
        scene.collect_visible(&camera, &mut out);

        assert_eq!(out.iter().map(|i| i.id).collect::<Vec<_>>(), vec![2, 3, 1, 4]);
    }

    #[test]
    fn collect_visible_reuses_the_buffer() {
        let scene = scattered_scene(1_000, 5_000.0);
        let camera = Camera::new(ScreenSize::new(800.0, 600.0));
        let mut out = vec![];
        scene.collect_visible(&camera, &mut out);
        let first = out.len();
        scene.collect_visible(&camera, &mut out);
        assert_eq!(out.len(), first, "stale entries were left in the buffer");
    }

    /// The index must agree with a brute-force scan, or culling is silently dropping
    /// content — the one bug a perf test would never catch.
    #[test]
    fn indexed_queries_match_brute_force() {
        let scene = scattered_scene(5_000, 10_000.0);

        for (cx, cy, zoom) in [
            (0.0, 0.0, 1.0),
            (7_500.0, -3_200.0, 0.25),
            (-9_900.0, 9_900.0, 4.0),
            (0.0, 0.0, 0.01),
        ] {
            let mut camera = Camera::new(ScreenSize::new(1280.0, 720.0));
            camera.set_center(WorldPoint::new(cx, cy));
            camera.set_zoom_about(zoom, crate::geometry::ScreenPoint::new(640.0, 360.0));
            camera.set_center(WorldPoint::new(cx, cy));

            let rect = camera.visible_world_rect();
            let mut expected: Vec<ItemId> = scene
                .iter()
                .filter(|i| i.bounds.intersects(&rect))
                .map(|i| i.id)
                .collect();
            let mut got: Vec<ItemId> = scene.query_viewport(&camera).map(|i| i.id).collect();
            expected.sort_unstable();
            got.sort_unstable();

            assert_eq!(got, expected, "mismatch at centre ({cx}, {cy}) zoom {zoom}");
        }
    }

    #[test]
    fn hit_test_matches_brute_force() {
        let scene = scattered_scene(5_000, 10_000.0);
        let mut rng = SplitMix64(0xC0FFEE);

        for _ in 0..500 {
            let p = WorldPoint::new(
                rng.next_range(-10_000.0, 10_000.0),
                rng.next_range(-10_000.0, 10_000.0),
            );
            let expected = scene
                .iter()
                .filter(|i| i.bounds.contains(p))
                .max_by_key(|i| (i.z, i.id))
                .map(|i| i.id);
            assert_eq!(scene.hit_test(p), expected, "mismatch at {p:?}");
        }
    }

    /// The headline claim of this crate: frame cost follows the viewport, not the
    /// board. 100k items spread over a 200k-px-square board, a 1280x720 window at
    /// 100% zoom — the query must hand back a handful of items, not a hundred
    /// thousand.
    #[test]
    fn culling_cost_tracks_the_viewport() {
        const COUNT: usize = 100_000;
        const EXTENT: f64 = 100_000.0;

        let scene = scattered_scene(COUNT, EXTENT);
        assert_eq!(scene.len(), COUNT);

        let mut camera = Camera::new(ScreenSize::new(1280.0, 720.0));
        camera.set_center(WorldPoint::new(12_345.0, -6_789.0));

        let visible = scene.query_viewport(&camera).count();

        // The viewport covers 1280x720 of a 200_000x200_000 world holding 100k
        // items, so the expectation is ~23. Anything near COUNT means the query
        // degenerated into a full scan.
        assert!(
            visible < 500,
            "viewport query returned {visible} of {COUNT} items — culling is not working"
        );
        assert!(
            visible > 0,
            "viewport query returned nothing; the test board is misconfigured"
        );

        // And the same scene zoomed all the way out does return most of the board,
        // which proves the small number above is culling rather than a broken query.
        camera.set_center(WorldPoint::ORIGIN);
        camera.set_zoom_about(0.01, crate::geometry::ScreenPoint::new(640.0, 360.0));
        camera.set_center(WorldPoint::ORIGIN);
        let wide = scene.query_viewport(&camera).count();
        assert!(wide > visible * 100, "zoomed out only found {wide} items");
    }

    #[test]
    fn bulk_load_and_incremental_insert_agree() {
        let bulk = scattered_scene(2_000, 20_000.0);
        let mut incremental = Scene::new();
        for item in bulk.iter() {
            incremental.insert(item.clone());
        }

        let camera = {
            let mut c = Camera::new(ScreenSize::new(1280.0, 720.0));
            c.set_center(WorldPoint::new(1_000.0, 2_000.0));
            c
        };

        let sorted = |s: &Scene| {
            let mut v: Vec<ItemId> = s.query_viewport(&camera).map(|i| i.id).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(sorted(&bulk), sorted(&incremental));
    }

    #[test]
    fn payload_survives_the_round_trip() {
        let mut scene = Scene::new();
        scene.insert(SceneItem::solid_quad(
            1,
            WorldRect::from_origin_size(WorldPoint::ORIGIN, 10.0, 10.0),
            0,
            [1.0, 0.97, 0.62, 1.0],
        ));
        let item = scene.get(1).unwrap();
        assert_eq!(
            item.payload,
            RenderPayload::SolidQuad { color: [1.0, 0.97, 0.62, 1.0] }
        );
    }
}
