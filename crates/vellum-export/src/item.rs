//! The placed item — the exporter's entire view of a board.
//!
//! Everything here is a plain value with no behaviour beyond geometry. That is the
//! point: `vellum-export` must not depend on `vellum-doc`, both because the two are
//! built in parallel and because an export should be reproducible from a snapshot
//! rather than from a live CRDT. See [`crate::source`] for how a `Board` maps onto
//! these types.

use crate::geom::{Path, Point, Rect};
use crate::style::Style;
use crate::text::TextBlock;

/// An item's identity within one export.
///
/// A `u64` rather than a string: the exporter only needs identity and equality, so
/// the document interns its own ids — Loro tree ids, or Miro's numeric widget ids —
/// into a dense range when it builds the snapshot. Nothing here is written to the
/// output, so the mapping does not have to be stable across runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u64);

/// A document's z-order key, compared as opaque bytes.
///
/// `docs/01-architecture.md` §4 puts z-order on **fractional indexing**, and Loro's
/// fractional index is a byte string, not a number — inserting between two items
/// appends bytes rather than renumbering. Parsing it into an `f64` would collapse
/// distinct keys and silently reorder a board, so the exporter compares the bytes it
/// was given and never interprets them.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZOrder(pub Vec<u8>);

impl ZOrder {
    /// A key for a caller that has a plain position rather than a fractional index.
    /// Big-endian so byte order and numeric order agree.
    pub fn from_index(index: u32) -> Self {
        Self(index.to_be_bytes().to_vec())
    }

    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }
}

/// Which end decorations a connector carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EndCap {
    #[default]
    None,
    /// A filled triangular head — Miro's default, and the only cap the SVG oracle
    /// recognises when counting connectors.
    Arrow,
    /// A filled disc.
    Circle,
}

impl EndCap {
    /// The cap's geometry at `terminal`, `size` units long.
    ///
    /// Defined once because all three writers must agree: the SVG writer emits this
    /// same triangle as the `#LineHeadArrow1` symbol, and a PDF or a PNG whose
    /// arrowheads were a different shape from the SVG's would be a bug nobody would
    /// notice until the two were placed side by side.
    pub fn path(self, terminal: crate::geom::Terminal, size: f64) -> crate::geom::Path {
        use crate::geom::{Affine, Path, SubPath, pt};
        match self {
            Self::None => Path::default(),
            Self::Arrow => {
                let unit = Path::new(vec![
                    SubPath::polyline(&[pt(0.0, 0.0), pt(-1.0, -0.4), pt(-1.0, 0.4)], true)
                        .expect("three points"),
                ]);
                let place = Affine::scaling(size, size)
                    .then(Affine::rotation_about(terminal.direction_deg, Point::default()))
                    .then(Affine::translation(terminal.point.x, terminal.point.y));
                unit.transformed(&place)
            }
            Self::Circle => Geometry::Ellipse {
                rect: Rect::new(
                    terminal.point.x - size * 0.4,
                    terminal.point.y - size * 0.4,
                    size * 0.8,
                    size * 0.8,
                ),
            }
            .to_path(),
        }
    }
}

/// What an item *is*, where that changes how it is exported.
///
/// The list is not "every widget Vellum has"; it is every distinction the four
/// writers actually make. A table and a mind map both export as their constituent
/// shapes, text and connectors, so neither needs a variant here.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A frame: clips its children, becomes one PDF page, and names a CSV section.
    Frame {
        /// Position in presentation order — `docs/features/README.md` §2, "frames
        /// sequenced as a deck". This is what orders PDF pages.
        order: u32,
        /// Miro frames always clip; a group used as a frame may not.
        clips: bool,
    },
    Sticky,
    /// A catalogue shape. `name` is [`vellum_shapes::Shape::name`], which becomes
    /// the SVG class so the shape's *type* survives the export exactly as it does
    /// in Miro's own SVG.
    Shape { name: &'static str },
    /// Standalone text with no body of its own.
    Text,
    /// A freehand or highlighter stroke.
    Ink,
    Image,
    Connector { start: EndCap, end: EndCap },
    /// A card widget: title, description, tags.
    Card,
    /// A URL preview card. Exported as a card, and counted as `preview-widget` by
    /// the Miro SVG reader.
    LinkPreview,
    /// A live embed, which cannot render in a GPU canvas
    /// (`docs/01-architecture.md` §7) and exports as its card.
    Embed,
}

impl Kind {
    pub const fn frame(order: u32) -> Self {
        Self::Frame { order, clips: true }
    }

    pub fn is_frame(&self) -> bool {
        matches!(self, Self::Frame { .. })
    }

    /// A stable lowercase tag, used for CSV's type column and for diagnostics. It
    /// matches the per-type keys `vellum-import` reports, so counts from an import
    /// and counts from an export can be compared directly.
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Frame { .. } => "frame",
            Self::Sticky => "sticky",
            Self::Shape { .. } => "shape",
            Self::Text => "text",
            Self::Ink => "ink",
            Self::Image => "image",
            Self::Connector { .. } => "connector",
            Self::Card => "card",
            Self::LinkPreview => "link_preview",
            Self::Embed => "embed",
        }
    }
}

/// A reference to image bytes held outside the item.
///
/// Keyed by string because the blob store is content-addressed on BLAKE3
/// (`docs/01-architecture.md` §5), and because an image used twenty times must be
/// resolved and embedded once.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageRef(pub String);

impl ImageRef {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

/// Where an item is and what silhouette it paints.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    /// An axis-aligned box, optionally rounded. Stickies, frames, images, cards and
    /// text boxes are all this.
    Rect { rect: Rect, corner_radius: f64 },
    /// The ellipse inscribed in `rect`. Separate from `Path` because SVG and PDF
    /// both express it more compactly and more accurately than four cubics.
    Ellipse { rect: Rect },
    /// Explicit geometry: catalogue shapes via [`Path::from_outline`], ink strokes
    /// from `vellum-ink`, connector routes from `vellum-connect`.
    Path(Path),
}

/// A sticky's corner radius, as a fraction of its shorter side.
///
/// Miro's sticker is a rounded square, and the radius is a property of the *kind*
/// rather than of the item: the SVG writer draws stickies through one shared `<use>`
/// symbol so the export can be read back by Miro's own SVG reader, and a shared
/// symbol cannot carry a per-item radius. Rather than let SVG round corners that
/// the PDF and the PNG leave square, all four writers take the silhouette from
/// [`Geometry::sticky`].
pub const STICKY_CORNER_FRACTION: f64 = 0.06;

impl Geometry {
    pub fn rect(rect: Rect) -> Self {
        Self::Rect { rect, corner_radius: 0.0 }
    }

    /// The silhouette every writer draws a sticky with. See
    /// [`STICKY_CORNER_FRACTION`].
    pub fn sticky(rect: Rect) -> Self {
        Self::rounded(rect, rect.width.min(rect.height) * STICKY_CORNER_FRACTION)
    }

    pub fn rounded(rect: Rect, corner_radius: f64) -> Self {
        Self::Rect { rect, corner_radius }
    }

    /// The item's box: exact for rectangles and ellipses, the path's own bounds
    /// otherwise. Text is laid out inside this, and clipping is tested against it.
    pub fn bounds(&self) -> Rect {
        match self {
            Self::Rect { rect, .. } | Self::Ellipse { rect } => *rect,
            Self::Path(path) => path.bounds().unwrap_or_default(),
        }
    }

    /// The same geometry as a path — the form every writer can always draw.
    ///
    /// A rounded rectangle's corners become cubic arcs with the standard
    /// `k = 0.5523` handle length, exact at the quadrant endpoints and midpoint.
    pub fn to_path(&self) -> Path {
        use crate::geom::{Segment, SubPath, pt};
        /// 4/3·tan(π/8) — the circular-arc cubic constant.
        const K: f64 = 0.552_284_749_830_793_4;
        match self {
            Self::Rect { rect, corner_radius } => {
                let r = corner_radius.min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
                let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
                if r <= 0.0 {
                    return Path::new(
                        SubPath::polyline(
                            &[pt(x0, y0), pt(x1, y0), pt(x1, y1), pt(x0, y1)],
                            true,
                        )
                        .into_iter()
                        .collect(),
                    );
                }
                let h = r * K;
                Path::new(vec![SubPath::closed(
                    pt(x0 + r, y0),
                    vec![
                        Segment::Line { to: pt(x1 - r, y0) },
                        Segment::Cubic {
                            ctrl1: pt(x1 - r + h, y0),
                            ctrl2: pt(x1, y0 + r - h),
                            to: pt(x1, y0 + r),
                        },
                        Segment::Line { to: pt(x1, y1 - r) },
                        Segment::Cubic {
                            ctrl1: pt(x1, y1 - r + h),
                            ctrl2: pt(x1 - r + h, y1),
                            to: pt(x1 - r, y1),
                        },
                        Segment::Line { to: pt(x0 + r, y1) },
                        Segment::Cubic {
                            ctrl1: pt(x0 + r - h, y1),
                            ctrl2: pt(x0, y1 - r + h),
                            to: pt(x0, y1 - r),
                        },
                        Segment::Line { to: pt(x0, y0 + r) },
                        Segment::Cubic {
                            ctrl1: pt(x0, y0 + r - h),
                            ctrl2: pt(x0 + r - h, y0),
                            to: pt(x0 + r, y0),
                        },
                    ],
                )])
            }
            Self::Ellipse { rect } => {
                let c = rect.centre();
                let (rx, ry) = (rect.width / 2.0, rect.height / 2.0);
                let (hx, hy) = (rx * K, ry * K);
                Path::new(vec![SubPath::closed(
                    pt(c.x + rx, c.y),
                    vec![
                        Segment::Cubic {
                            ctrl1: pt(c.x + rx, c.y + hy),
                            ctrl2: pt(c.x + hx, c.y + ry),
                            to: pt(c.x, c.y + ry),
                        },
                        Segment::Cubic {
                            ctrl1: pt(c.x - hx, c.y + ry),
                            ctrl2: pt(c.x - rx, c.y + hy),
                            to: pt(c.x - rx, c.y),
                        },
                        Segment::Cubic {
                            ctrl1: pt(c.x - rx, c.y - hy),
                            ctrl2: pt(c.x - hx, c.y - ry),
                            to: pt(c.x, c.y - ry),
                        },
                        Segment::Cubic {
                            ctrl1: pt(c.x + hx, c.y - ry),
                            ctrl2: pt(c.x + rx, c.y - hy),
                            to: pt(c.x + rx, c.y),
                        },
                    ],
                )])
            }
            Self::Path(path) => path.clone(),
        }
    }
}

/// One item, placed on the board.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: ItemId,
    /// The frame that contains this item. A frame clips its children and gathers
    /// them into a PDF page and a CSV section, so this one field carries all three.
    pub frame: Option<ItemId>,
    pub z: ZOrder,
    pub kind: Kind,
    pub geometry: Geometry,
    /// Clockwise rotation in degrees about the geometry's centre.
    pub rotation: f64,
    pub style: Style,
    pub text: Option<TextBlock>,
    /// Image content, painted to fill the geometry's bounds.
    pub image: Option<ImageRef>,
    /// The item's **label**, as opposed to its content: a frame's name, a card's
    /// heading. A frame's name is drawn outside its box, names the frame's CSV
    /// section and titles its PDF page, so it cannot live in `text`.
    ///
    /// A card's visible title and description belong in `text` — the writers draw
    /// `text`, and a caller that put a card's heading here alone would find it in
    /// the CSV but not in the picture. `name` is the fallback the CSV uses when an
    /// item has no text at all.
    pub name: Option<String>,
    /// A URL the whole item links to.
    pub link: Option<String>,
    /// Excluded from every output. A filtered type or a hidden layer sets this
    /// rather than being dropped from the iterator, so counts still add up.
    pub hidden: bool,
}

impl Item {
    /// A bare item. Everything optional is empty, so a test or a caller sets only
    /// what it means.
    pub fn new(id: u64, kind: Kind, geometry: Geometry) -> Self {
        Self {
            id: ItemId(id),
            frame: None,
            z: ZOrder::from_index(id as u32),
            kind,
            geometry,
            rotation: 0.0,
            style: Style::NONE,
            text: None,
            image: None,
            name: None,
            link: None,
            hidden: false,
        }
    }

    pub fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn with_text(mut self, text: TextBlock) -> Self {
        self.text = Some(text);
        self
    }

    pub fn with_image(mut self, image: ImageRef) -> Self {
        self.image = Some(image);
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }

    pub fn with_rotation(mut self, degrees: f64) -> Self {
        self.rotation = degrees;
        self
    }

    pub fn in_frame(mut self, frame: ItemId) -> Self {
        self.frame = Some(frame);
        self
    }

    pub fn with_z(mut self, z: ZOrder) -> Self {
        self.z = z;
        self
    }

    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }

    /// The silhouette to paint, which is the item's own geometry except for a
    /// sticky — see [`STICKY_CORNER_FRACTION`] for why that one is decided by kind.
    pub fn silhouette(&self) -> Geometry {
        match self.kind {
            Kind::Sticky => Geometry::sticky(self.geometry.bounds()),
            _ => self.geometry.clone(),
        }
    }

    /// The transform from unrotated geometry to where the item actually sits.
    pub fn transform(&self) -> crate::geom::Affine {
        if self.rotation == 0.0 {
            crate::geom::Affine::IDENTITY
        } else {
            crate::geom::Affine::rotation_about(self.rotation, self.geometry.bounds().centre())
        }
    }

    /// The box the item's paint occupies, rotation and stroke width included.
    ///
    /// A rotated rectangle's box is the box of its four rotated corners, not the
    /// rotated box — the difference is up to 41% of the diagonal, which is the
    /// difference between an export that fits and one that clips its own content.
    pub fn painted_bounds(&self) -> Rect {
        let base = self.geometry.bounds();
        let rect = if self.rotation == 0.0 {
            base
        } else {
            let t = self.transform();
            Rect::of_points(base.corners().map(|c| t.apply(c))).unwrap_or(base)
        };
        rect.inflated(self.style.stroke_overhang())
    }

    /// Where text is laid out: the geometry's box, inset by the block's padding.
    pub fn text_box(&self) -> Rect {
        let padding = self.text.as_ref().map_or(0.0, |t| t.padding);
        self.geometry.bounds().inflated(-padding)
    }

    /// The item's plain text, empty when it has none.
    pub fn plain_text(&self) -> String {
        self.text.as_ref().map(TextBlock::plain_text).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::pt;
    use crate::style::{Color, Stroke};

    fn boxed(id: u64) -> Item {
        Item::new(id, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 100.0, 50.0)))
    }

    #[test]
    fn z_order_bytes_sort_in_numeric_order() {
        let mut keys = [ZOrder::from_index(10), ZOrder::from_index(2), ZOrder::from_index(300)];
        keys.sort();
        assert_eq!(keys, [ZOrder::from_index(2), ZOrder::from_index(10), ZOrder::from_index(300)]);
    }

    /// A fractional index inserts by appending, and byte order must respect that —
    /// this is the property that makes it safe to treat the key as opaque.
    #[test]
    fn a_key_between_two_others_sorts_between_them() {
        let a = ZOrder::from_bytes(vec![0x80]);
        let b = ZOrder::from_bytes(vec![0x80, 0x40]);
        let c = ZOrder::from_bytes(vec![0x81]);
        assert!(a < b && b < c);
    }

    #[test]
    fn a_rounded_rect_path_closes_and_keeps_its_bounds() {
        let rect = Rect::new(10.0, 20.0, 100.0, 60.0);
        let path = Geometry::rounded(rect, 12.0).to_path();
        assert_eq!(path.subpaths.len(), 1);
        assert!(path.subpaths[0].closed);
        let b = path.bounds().unwrap();
        assert!((b.x - rect.x).abs() < 1e-9 && (b.width - rect.width).abs() < 1e-9, "{b:?}");
    }

    #[test]
    fn a_corner_radius_larger_than_the_box_is_clamped_to_a_stadium() {
        let rect = Rect::new(0.0, 0.0, 20.0, 100.0);
        let path = Geometry::rounded(rect, 500.0).to_path();
        let b = path.bounds().unwrap();
        assert!((b.width - 20.0).abs() < 1e-9 && (b.height - 100.0).abs() < 1e-9, "{b:?}");
    }

    #[test]
    fn an_ellipse_path_touches_all_four_sides_of_its_box() {
        let rect = Rect::new(-5.0, 7.0, 40.0, 20.0);
        let b = Geometry::Ellipse { rect }.to_path().bounds().unwrap();
        assert!((b.x - rect.x).abs() < 1e-6, "{b:?}");
        assert!((b.y - rect.y).abs() < 1e-6, "{b:?}");
        assert!((b.width - rect.width).abs() < 1e-6, "{b:?}");
        assert!((b.height - rect.height).abs() < 1e-6, "{b:?}");
    }

    #[test]
    fn rotating_a_rectangle_grows_its_box_by_the_corners_not_the_box() {
        let item = boxed(1).with_rotation(45.0);
        let b = item.painted_bounds();
        let expected = (100.0f64 + 50.0) / 2.0f64.sqrt();
        assert!((b.width - expected).abs() < 1e-6, "{b:?}");
        assert!((b.height - expected).abs() < 1e-6, "{b:?}");
        // The centre does not move.
        assert!(b.centre().distance(pt(50.0, 25.0)) < 1e-9);
    }

    #[test]
    fn painted_bounds_include_half_the_stroke() {
        let item = boxed(1).with_style(Style::NONE.with_stroke(Stroke::new(Color::BLACK, 10.0)));
        assert_eq!(item.painted_bounds(), Rect::new(-5.0, -5.0, 110.0, 60.0));
    }

    #[test]
    fn padding_insets_the_text_box_on_every_side() {
        let item = boxed(1)
            .with_text(crate::text::TextBlock::default().with_padding(8.0));
        assert_eq!(item.text_box(), Rect::new(8.0, 8.0, 84.0, 34.0));
    }

    /// The silhouette all four writers share, so an SVG's rounded sticky and a
    /// PNG's square one can never diverge again.
    #[test]
    fn a_sticky_is_rounded_whatever_geometry_it_carries() {
        let item = boxed(1);
        assert!(matches!(item.geometry, Geometry::Rect { corner_radius: 0.0, .. }));
        match item.silhouette() {
            Geometry::Rect { corner_radius, .. } => {
                assert!((corner_radius - 50.0 * STICKY_CORNER_FRACTION).abs() < 1e-9);
            }
            other => panic!("expected a rounded rect, got {other:?}"),
        }
        // Nothing else is second-guessed.
        let shape = Item::new(
            2,
            Kind::Shape { name: "rectangle" },
            Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)),
        );
        assert_eq!(shape.silhouette(), shape.geometry);
    }

    #[test]
    fn kind_tags_match_the_importers_type_keys() {
        assert_eq!(Kind::frame(0).tag(), "frame");
        assert_eq!(Kind::LinkPreview.tag(), "link_preview");
        assert_eq!(Kind::Shape { name: "cloud" }.tag(), "shape");
    }
}
