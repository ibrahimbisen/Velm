//! The input contract: what a document must be able to say for it to be exported.
//!
//! # Why a trait and not a `Board`
//!
//! `vellum-export` deliberately does not depend on `vellum-doc`. Three reasons, in
//! order of weight:
//!
//! 1. **Export is a pure function of a snapshot.** A Loro document is a live CRDT
//!    with history and undo state; an export is a photograph. Taking the photograph
//!    through a narrow trait means an export can be produced from a document, from
//!    a decoded clipboard payload, from a test fixture, or from a board loaded out
//!    of SQLite without ever building a `Board` — and it means an export can be
//!    handed to a worker thread without carrying a CRDT along.
//! 2. **The dependency graph stays acyclic and shallow.** `docs/01-architecture.md`
//!    §2 has everything pointing downward; a serialisation crate that reached back
//!    into the document layer would be the first exception.
//! 3. **It is testable with no document at all.** Every writer in this crate is
//!    exercised against [`Snapshot`], a `Vec<Item>` and two maps.
//!
//! # How a `Board` maps onto it
//!
//! The mapping is mechanical, and is written out here so it does not have to be
//! rediscovered when `vellum-doc` wires it up:
//!
//! | Board concept | Becomes |
//! |---|---|
//! | tree node id | [`ItemId`], interned into a dense `u64` range |
//! | movable-tree parent, when it is a frame | [`Item::frame`] |
//! | movable-tree parent, when it is a group | *flattened* — a group has no paint of its own, so its children are emitted with the group's transform already applied and the group itself is not an item |
//! | fractional z index | [`ZOrder`], byte-for-byte |
//! | item rect + corner radius | [`Geometry::Rect`] |
//! | `vellum_shapes::Shape` | [`Path::from_outline`] into [`Geometry::Path`], with [`Kind::Shape`] carrying [`vellum_shapes::Shape::name`] |
//! | `vellum_ink` stroke | [`Geometry::Path`] plus a round-capped [`Stroke`](crate::style::Stroke) |
//! | `vellum_connect` route | [`Geometry::Path`] plus [`Kind::Connector`] with its end caps |
//! | styled spans, shaped by `vellum-text` | [`TextBlock`] lines with baselines |
//! | blob-store BLAKE3 key | [`ImageRef`] |
//! | frame index in the frames panel | the `order` field of [`Kind::Frame`] |
//!
//! The only part that is not a field rename is text: the board must run its layout
//! and hand over **positioned lines**. [`crate::text`] explains why.

use crate::item::{ImageRef, Item, ItemId};
use crate::text::{FaceKey, FontSpec};
use std::collections::BTreeMap;

// Referenced by the doc table above.
#[allow(unused_imports)]
use crate::{
    geom::Path,
    item::{Geometry, Kind, ZOrder},
    text::TextBlock,
};

/// Image bytes, in whichever form the caller already has them.
#[derive(Debug, Clone, PartialEq)]
pub enum ImageData {
    /// Bytes exactly as stored. The SVG writer base64s them verbatim, and the PDF
    /// writer passes JPEG straight into a `/DCTDecode` stream — no decode, no
    /// recompression, no generation loss.
    Encoded { media_type: String, bytes: Vec<u8> },
    /// Straight (non-premultiplied) RGBA8, row-major, `width * height * 4` bytes.
    /// This is what the app already has after decoding for a GPU texture, so
    /// handing it over avoids decoding the same image twice.
    Rgba8 { width: u32, height: u32, pixels: Vec<u8> },
}

impl ImageData {
    pub fn encoded(media_type: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self::Encoded { media_type: media_type.into(), bytes: bytes.into() }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Encoded { bytes, .. } => bytes.is_empty(),
            Self::Rgba8 { width, height, pixels } => {
                *width == 0 || *height == 0 || pixels.is_empty()
            }
        }
    }
}

/// Everything the exporters need from a document.
///
/// Implementors yield items **in any order** — [`crate::scene::Scene`] sorts by
/// [`ZOrder`] — and may yield borrowed or owned items as convenient.
pub trait BoardSource {
    /// Every item on the board, hidden ones included.
    ///
    /// Hidden items are yielded rather than filtered so that a caller comparing
    /// counts against an import sees the same total. [`crate::scene::Scene`] drops
    /// them.
    fn items(&self) -> impl Iterator<Item = Item> + '_;

    /// Resolves an image reference. Called at most once per distinct key per
    /// export, so an implementor may decode lazily without caching.
    fn image(&self, _image: &ImageRef) -> Option<ImageData> {
        None
    }

    /// Font file bytes for a run, if the caller can supply them.
    ///
    /// Only PDF and the CPU rasteriser need this: SVG names a family and lets the
    /// viewer resolve it. Returning `None` is a supported answer — the PDF falls
    /// back to a standard font and the rasteriser reports the text it could not
    /// draw. `size` is ignored; see [`FontSpec::face_key`].
    fn font(&self, _font: &FontSpec) -> Option<&[u8]> {
        None
    }

    /// The board's name, used as the SVG `<title>` and the PDF document title.
    fn title(&self) -> Option<&str> {
        None
    }
}

/// A board held entirely in memory — the reference implementation of
/// [`BoardSource`].
///
/// This is what tests build, and what a caller that has already gathered its items
/// can use without writing a trait impl. It is also the shape the pipeline takes
/// when an export is moved off the UI thread: a `Snapshot` is `Send`, a CRDT
/// document is not necessarily.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    items: Vec<Item>,
    images: BTreeMap<String, ImageData>,
    fonts: BTreeMap<FaceKey, Vec<u8>>,
    title: Option<String>,
}

impl Snapshot {
    pub fn new(items: Vec<Item>) -> Self {
        Self { items, ..Self::default() }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_image(mut self, key: impl Into<String>, data: ImageData) -> Self {
        self.images.insert(key.into(), data);
        self
    }

    /// Registers a font file for one face. The key ignores size, so one call covers
    /// every size that face is used at.
    pub fn with_font(mut self, font: &FontSpec, bytes: impl Into<Vec<u8>>) -> Self {
        self.fonts.insert(font.face_key(), bytes.into());
        self
    }

    pub fn push(&mut self, item: Item) {
        self.items.push(item);
    }

    pub fn items_slice(&self) -> &[Item] {
        &self.items
    }
}

impl BoardSource for Snapshot {
    fn items(&self) -> impl Iterator<Item = Item> + '_ {
        self.items.iter().cloned()
    }

    fn image(&self, image: &ImageRef) -> Option<ImageData> {
        self.images.get(&image.0).cloned()
    }

    fn font(&self, font: &FontSpec) -> Option<&[u8]> {
        self.fonts.get(&font.face_key()).map(Vec::as_slice)
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

/// Which part of the board to export.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scope {
    /// Everything, hidden items excepted.
    #[default]
    Board,
    /// One frame and the items it contains. The frame itself is included, and the
    /// export's bounds are the frame's rect exactly — not the union of what sits
    /// inside it, which would let one overflowing sticky change the page size.
    Frame(ItemId),
    /// An explicit selection. Selecting a frame brings its contents with it, which
    /// is what selecting a frame does on the canvas.
    Selection(Vec<ItemId>),
}

impl Scope {
    pub fn selection(ids: impl IntoIterator<Item = ItemId>) -> Self {
        Self::Selection(ids.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use crate::item::{Geometry, Kind};

    #[test]
    fn a_snapshot_yields_what_it_was_given() {
        let snapshot = Snapshot::new(vec![
            Item::new(1, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 1.0, 1.0))),
            Item::new(2, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 1.0, 1.0))),
        ])
        .with_title("Reference Board");
        assert_eq!(snapshot.items().count(), 2);
        assert_eq!(snapshot.title(), Some("Reference Board"));
    }

    #[test]
    fn fonts_resolve_by_face_regardless_of_size() {
        let font = FontSpec::new("Inter", 12.0).bold();
        let snapshot = Snapshot::default().with_font(&font, vec![1, 2, 3]);
        assert_eq!(snapshot.font(&FontSpec::new("Inter", 96.0).bold()), Some([1, 2, 3].as_slice()));
        assert_eq!(snapshot.font(&FontSpec::new("Inter", 12.0)), None, "regular is another face");
    }

    #[test]
    fn an_unregistered_image_resolves_to_nothing_rather_than_panicking() {
        let snapshot = Snapshot::default().with_image("a", ImageData::encoded("image/png", [0u8]));
        assert!(snapshot.image(&ImageRef::new("a")).is_some());
        assert!(snapshot.image(&ImageRef::new("b")).is_none());
    }

    #[test]
    fn empty_image_payloads_are_recognised() {
        assert!(ImageData::encoded("image/png", Vec::new()).is_empty());
        assert!(ImageData::Rgba8 { width: 0, height: 4, pixels: vec![0; 16] }.is_empty());
        assert!(!ImageData::Rgba8 { width: 1, height: 1, pixels: vec![0; 4] }.is_empty());
    }
}
