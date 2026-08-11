//! Resolving a board and a scope into the ordered, clipped list every writer draws.
//!
//! All four writers share this stage, and none of them re-derives it. That is the
//! whole reason it exists: z-order, clipping and scope are the three things it is
//! easy to get subtly different between four output formats, and a PDF whose
//! stacking differs from the PNG of the same frame is a bug nobody notices until a
//! print comes out wrong.
//!
//! What happens here, once:
//!
//! - **Scope** selects the items ([`Scope`]), including the rule that selecting a
//!   frame selects its contents.
//! - **Z-order** sorts them back to front by the document's fractional index, with
//!   a *stable* sort so items sharing a key keep the order the source gave.
//! - **Clipping** resolves each item's frame into a clip rectangle, and drops
//!   anything the clip removes entirely — a frame's contents genuinely stop at its
//!   edge (`docs/features/README.md` §1, "clip content").
//! - **Resources** are gathered: each distinct image is fetched once however many
//!   items use it, and each distinct font face once however many sizes it is used
//!   at.

use crate::error::ExportError;
use crate::geom::Rect;
use crate::item::{ImageRef, Item, ItemId, Kind};
use crate::source::{BoardSource, ImageData, Scope};
use crate::text::{FaceKey, FontSpec};
use std::collections::{BTreeMap, BTreeSet};

/// An item together with everything the scene resolved about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub item: Item,
    /// The clip imposed by the containing frame, in board coordinates. `None` when
    /// the item is unframed, or its frame does not clip.
    pub clip: Option<Rect>,
}

impl Placed {
    /// The paint that actually survives the clip.
    pub fn visible_bounds(&self) -> Option<Rect> {
        let bounds = self.item.painted_bounds();
        match self.clip {
            Some(clip) => bounds.intersection(clip),
            None => Some(bounds),
        }
    }
}

/// A frame in scope, with everything a page needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub id: ItemId,
    pub rect: Rect,
    pub order: u32,
    pub title: Option<String>,
}

/// A board, resolved for one export.
#[derive(Debug, Clone)]
pub struct Scene {
    pub title: Option<String>,
    /// The union of everything visible, or the frame's rect for a frame export.
    /// Never empty — an export with no content is an error, not a zero-size file.
    pub bounds: Rect,
    /// Back to front.
    pub items: Vec<Placed>,
    /// Frames in scope, in presentation order.
    pub frames: Vec<Frame>,
    images: BTreeMap<String, ImageData>,
    fonts: BTreeMap<FaceKey, Vec<u8>>,
}

impl Scene {
    /// Resolves `source` under `scope`.
    ///
    /// Fails only when the result would be empty; every other degenerate input —
    /// a frame with no children, an item with no style, text with no font — is a
    /// legitimate board and produces legitimate output.
    pub fn collect<S: BoardSource>(source: &S, scope: &Scope) -> Result<Self, ExportError> {
        let all: Vec<Item> = source.items().filter(|i| !i.hidden).collect();

        let frames_by_id: BTreeMap<ItemId, &Item> =
            all.iter().filter(|i| i.kind.is_frame()).map(|i| (i.id, i)).collect();

        let wanted = select(&all, &frames_by_id, scope);

        let mut items: Vec<Placed> = Vec::with_capacity(wanted.len());
        for item in all.iter().filter(|i| wanted.contains(&i.id)) {
            // A frame clips its children, but never itself: its own border and its
            // name sit on the boundary and would be half eaten.
            let clip = item
                .frame
                .filter(|_| !item.kind.is_frame())
                .and_then(|id| frames_by_id.get(&id))
                .filter(|frame| matches!(frame.kind, Kind::Frame { clips: true, .. }))
                .map(|frame| frame.geometry.bounds());

            let placed = Placed { item: item.clone(), clip };
            if placed.visible_bounds().is_some() {
                items.push(placed);
            }
        }

        // Stable, so equal fractional indices keep document order rather than
        // shuffling between runs.
        items.sort_by(|a, b| a.item.z.cmp(&b.item.z));

        let mut frames: Vec<Frame> = items
            .iter()
            .filter_map(|p| match p.item.kind {
                Kind::Frame { order, .. } => Some(Frame {
                    id: p.item.id,
                    rect: p.item.geometry.bounds(),
                    order,
                    title: p.item.name.clone(),
                }),
                _ => None,
            })
            .collect();
        frames.sort_by_key(|f| (f.order, f.id));

        let bounds = match scope {
            // A frame export is the frame, exactly. Anything hanging over the edge
            // is clipped away in any case, so including it would only pad the page.
            Scope::Frame(id) => frames_by_id
                .get(id)
                .map(|f| f.geometry.bounds())
                .ok_or(ExportError::UnknownFrame(*id))?,
            _ => items
                .iter()
                .filter_map(Placed::visible_bounds)
                .reduce(Rect::union)
                .ok_or(ExportError::NothingToExport)?,
        };
        if bounds.is_empty() {
            return Err(ExportError::NothingToExport);
        }

        let mut scene = Self {
            title: source.title().map(str::to_owned),
            bounds,
            items,
            frames,
            images: BTreeMap::new(),
            fonts: BTreeMap::new(),
        };
        scene.gather_resources(source);
        Ok(scene)
    }

    /// Fetches each distinct image and font face once. An image on twenty items is
    /// one fetch and one embedded copy; `docs/01-architecture.md` §5 stores it once
    /// too, keyed on the same BLAKE3 hash.
    fn gather_resources<S: BoardSource>(&mut self, source: &S) {
        let refs: BTreeSet<ImageRef> =
            self.items.iter().filter_map(|p| p.item.image.clone()).collect();
        for image in refs {
            if let Some(data) = source.image(&image).filter(|d| !d.is_empty()) {
                self.images.insert(image.0, data);
            }
        }

        let faces: BTreeMap<FaceKey, FontSpec> = self
            .items
            .iter()
            .filter_map(|p| p.item.text.as_ref())
            .flat_map(|t| &t.lines)
            .flat_map(|l| &l.spans)
            .map(|s| (s.font.face_key(), s.font.clone()))
            .collect();
        for (key, spec) in faces {
            if let Some(bytes) = source.font(&spec) {
                self.fonts.insert(key, bytes.to_vec());
            }
        }
    }

    pub fn image(&self, image: &ImageRef) -> Option<&ImageData> {
        self.images.get(&image.0)
    }

    pub fn font(&self, font: &FontSpec) -> Option<&[u8]> {
        self.fonts.get(&font.face_key()).map(Vec::as_slice)
    }

    /// Every font face the scene resolved bytes for, in a stable order — the PDF
    /// writer embeds exactly these.
    pub(crate) fn faces(&self) -> impl Iterator<Item = (&FaceKey, &[u8])> {
        self.fonts.iter().map(|(k, v)| (k, v.as_slice()))
    }

    /// Items in a frame, in z-order, excluding the frame itself.
    pub fn items_in_frame(&self, frame: ItemId) -> impl Iterator<Item = &Placed> {
        self.items.iter().filter(move |p| p.item.frame == Some(frame) && p.item.id != frame)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Per-kind counts, keyed by [`Kind::tag`] — directly comparable with the
    /// per-type counts `vellum-import` produces, which is what makes an
    /// import-then-export round trip checkable.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for placed in &self.items {
            *counts.entry(placed.item.kind.tag().to_string()).or_insert(0) += 1;
        }
        counts
    }
}

/// Which ids a scope selects.
fn select(
    all: &[Item],
    frames_by_id: &BTreeMap<ItemId, &Item>,
    scope: &Scope,
) -> BTreeSet<ItemId> {
    match scope {
        Scope::Board => all.iter().map(|i| i.id).collect(),
        Scope::Frame(frame) => all
            .iter()
            .filter(|i| i.id == *frame || i.frame == Some(*frame))
            .map(|i| i.id)
            .collect(),
        Scope::Selection(ids) => {
            let chosen: BTreeSet<ItemId> = ids.iter().copied().collect();
            // Selecting a frame selects its contents. Without this, exporting a
            // selected frame would produce an empty box, which is never what anyone
            // meant by "export selection".
            let frames: BTreeSet<ItemId> =
                chosen.iter().copied().filter(|id| frames_by_id.contains_key(id)).collect();
            all.iter()
                .filter(|i| {
                    chosen.contains(&i.id) || i.frame.is_some_and(|f| frames.contains(&f))
                })
                .map(|i| i.id)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use crate::item::{Geometry, ZOrder};
    use crate::source::Snapshot;

    const FRAME: ItemId = ItemId(100);

    fn sticky(id: u64, rect: Rect) -> Item {
        Item::new(id, Kind::Sticky, Geometry::rect(rect))
    }

    /// A frame at (0,0)-(200,200) with three children: one inside, one straddling
    /// the right edge, one entirely outside. Plus a loose sticky far away.
    fn board() -> Snapshot {
        Snapshot::new(vec![
            Item::new(100, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 200.0, 200.0)))
                .with_name("Wiring"),
            sticky(1, Rect::new(10.0, 10.0, 50.0, 50.0)).in_frame(FRAME),
            sticky(2, Rect::new(180.0, 10.0, 50.0, 50.0)).in_frame(FRAME),
            sticky(3, Rect::new(400.0, 400.0, 50.0, 50.0)).in_frame(FRAME),
            sticky(4, Rect::new(-500.0, -500.0, 50.0, 50.0)),
        ])
        .with_title("test board")
    }

    fn scene(scope: Scope) -> Scene {
        Scene::collect(&board(), &scope).expect("the fixture has content")
    }

    #[test]
    fn board_scope_takes_everything_and_bounds_it() {
        let scene = scene(Scope::Board);
        // Item 3 is clipped away entirely by its frame; 4 is unframed and survives.
        assert_eq!(scene.items.len(), 4);
        assert_eq!(scene.bounds, Rect::new(-500.0, -500.0, 700.0, 700.0));
        assert_eq!(scene.title.as_deref(), Some("test board"));
    }

    #[test]
    fn a_frames_children_are_clipped_to_it_and_the_frame_itself_is_not() {
        let scene = scene(Scope::Board);
        let clip_of = |id: u64| {
            scene.items.iter().find(|p| p.item.id == ItemId(id)).expect("present").clip
        };
        assert_eq!(clip_of(100), None, "a frame does not clip itself");
        assert_eq!(clip_of(1), Some(Rect::new(0.0, 0.0, 200.0, 200.0)));
        assert_eq!(clip_of(4), None, "an unframed item is unclipped");
    }

    #[test]
    fn an_item_wholly_outside_its_frame_is_dropped() {
        let scene = scene(Scope::Board);
        assert!(scene.items.iter().all(|p| p.item.id != ItemId(3)));
        // The straddling one survives, cut down to the overlap.
        let straddler = scene.items.iter().find(|p| p.item.id == ItemId(2)).expect("present");
        assert_eq!(straddler.visible_bounds(), Some(Rect::new(180.0, 10.0, 20.0, 50.0)));
    }

    #[test]
    fn a_non_clipping_frame_lets_its_children_overflow() {
        let mut items = board().items_slice().to_vec();
        items[0].kind = Kind::Frame { order: 0, clips: false };
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        assert_eq!(scene.items.len(), 5, "nothing is clipped away now");
        assert!(scene.items.iter().all(|p| p.clip.is_none()));
    }

    #[test]
    fn a_frame_export_is_bounded_by_the_frame_not_by_its_contents() {
        let scene = scene(Scope::Frame(FRAME));
        assert_eq!(scene.bounds, Rect::new(0.0, 0.0, 200.0, 200.0));
        assert_eq!(scene.items.len(), 3, "the frame and its two visible children");
        assert!(scene.items.iter().all(|p| p.item.id != ItemId(4)), "the loose sticky is out");
    }

    #[test]
    fn selecting_a_frame_brings_its_contents() {
        let scene = scene(Scope::selection([FRAME]));
        assert_eq!(scene.items.len(), 3);
    }

    #[test]
    fn selecting_loose_items_takes_exactly_those() {
        let scene = scene(Scope::selection([ItemId(1), ItemId(4)]));
        assert_eq!(scene.items.len(), 2);
        assert_eq!(scene.bounds, Rect::new(-500.0, -500.0, 560.0, 560.0));
    }

    #[test]
    fn hidden_items_never_reach_the_scene() {
        let mut items = board().items_slice().to_vec();
        items[1].hidden = true;
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        assert!(scene.items.iter().all(|p| p.item.id != ItemId(1)));
    }

    #[test]
    fn z_order_is_ascending_and_stable_within_a_key() {
        let shared = ZOrder::from_bytes(vec![7]);
        let items = vec![
            sticky(1, Rect::new(0.0, 0.0, 1.0, 1.0)).with_z(ZOrder::from_bytes(vec![9])),
            sticky(2, Rect::new(0.0, 0.0, 1.0, 1.0)).with_z(shared.clone()),
            sticky(3, Rect::new(0.0, 0.0, 1.0, 1.0)).with_z(shared),
            sticky(4, Rect::new(0.0, 0.0, 1.0, 1.0)).with_z(ZOrder::from_bytes(vec![1])),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let order: Vec<u64> = scene.items.iter().map(|p| p.item.id.0).collect();
        assert_eq!(order, vec![4, 2, 3, 1], "ascending, ties in document order");
    }

    #[test]
    fn frames_come_out_in_presentation_order() {
        let items = vec![
            Item::new(1, Kind::frame(2), Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0))),
            Item::new(2, Kind::frame(0), Geometry::rect(Rect::new(20.0, 0.0, 10.0, 10.0))),
            Item::new(3, Kind::frame(1), Geometry::rect(Rect::new(40.0, 0.0, 10.0, 10.0))),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        assert_eq!(scene.frames.iter().map(|f| f.id.0).collect::<Vec<_>>(), vec![2, 3, 1]);
    }

    #[test]
    fn an_empty_board_is_an_error_not_an_empty_file() {
        let empty = Snapshot::default();
        assert!(matches!(
            Scene::collect(&empty, &Scope::Board),
            Err(ExportError::NothingToExport)
        ));
    }

    #[test]
    fn exporting_a_frame_that_is_not_there_names_the_frame() {
        assert!(matches!(
            Scene::collect(&board(), &Scope::Frame(ItemId(999))),
            Err(ExportError::UnknownFrame(ItemId(999)))
        ));
    }

    #[test]
    fn counts_are_keyed_the_way_the_importer_reports_them() {
        let counts = scene(Scope::Board).counts();
        assert_eq!(counts.get("frame"), Some(&1));
        assert_eq!(counts.get("sticky"), Some(&3));
    }

    #[test]
    fn a_repeated_image_is_fetched_once() {
        use crate::source::ImageData;
        let key = ImageRef::new("blake3:abc");
        let items = (0..5)
            .map(|i| {
                Item::new(i, Kind::Image, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                    .with_image(key.clone())
            })
            .collect();
        let source = Snapshot::new(items)
            .with_image("blake3:abc", ImageData::encoded("image/png", vec![1, 2, 3]));
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        assert_eq!(scene.images.len(), 1);
        assert!(scene.image(&key).is_some());
    }
}
