//! What lives on a board: identity, kind, style and stacking order.

use crate::geometry::{Align, Color, Crop, Placement, Point};
use crate::text::StyledText;
use loro::TreeID;
use std::fmt;
use std::str::FromStr;

/// A stable handle to one item.
///
/// Backed by Loro's `TreeID`, which is `(peer, counter)` — globally unique without
/// coordination, so two people (or two imports) adding items offline can never
/// collide. It is `Copy` and cheap to hash, so selection sets and the spatial index
/// can key on it directly.
///
/// One sharp edge, verified against Loro 1.13: **undoing an item's creation and
/// then redoing it produces a new `ItemId`.** Loro's undo replays an inverse diff
/// rather than resurrecting the original node, so anything holding an `ItemId`
/// across an undo/redo — a selection, a hover target — must re-resolve it. See
/// [`Board::undo`](crate::Board::undo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(TreeID);

impl ItemId {
    pub(crate) const fn from_tree_id(id: TreeID) -> Self {
        Self(id)
    }

    pub(crate) const fn tree_id(self) -> TreeID {
        self.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Returned when a string is not a `<counter>@<peer>` item id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a valid item id")]
pub struct ParseItemIdError(String);

impl FromStr for ItemId {
    type Err = ParseItemIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TreeID::try_from(s).map(Self).map_err(|_| ParseItemIdError(s.to_owned()))
    }
}

/// How much of a link or embed card is drawn.
///
/// Miro shows the same three forms and switches between them from the card's own toolbar —
/// the `⊞` button in it. They are genuinely different *sizes* of the same item rather than
/// styles of it, which is why this is on the kind and not in `Style`: a collapsed row is one
/// line tall whatever box the item has, and the painter needs to know that before it lays
/// anything out.
///
/// [`CardMode::Card`] is the default because it is the form that carries a link's meaning —
/// a title and a site — in the least space. An imported Miro `preview` arrives as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CardMode {
    /// One line: favicon, host, title. What a bare URL in a list should look like, and
    /// what Miro calls a collapsed embed.
    Link,
    /// Favicon, site name, title, and a blurb if the page gave one. No image.
    #[default]
    Card,
    /// The whole thing: preview image above the text. A video's poster frame lives here.
    Large,
}

impl CardMode {
    pub const ALL: [Self; 3] = [Self::Link, Self::Card, Self::Large];

    /// The on-disk tag. Stable: it is written into every board file.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Card => "card",
            Self::Large => "large",
        }
    }

    /// Inverse of [`CardMode::tag`]. An unknown tag reads as `None` so the caller can
    /// fall back to the default rather than lose the item.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.tag() == tag)
    }

    /// Whether this mode draws the preview image at all, which is what decides whether an
    /// unfetched thumbnail is worth fetching.
    ///
    /// **Both `Card` and `Large`**, which is Miro's behaviour: a page that offers a picture
    /// shows it, and a page that does not simply has none. Restricting it to `Large` meant a
    /// pasted YouTube link fetched its poster frame and then drew a box of text, because a new
    /// card is a `Card` — the picture was on disk and nothing put it on screen. The two modes
    /// differ in how much of the card the picture takes; see [`CardMode::image_fraction`].
    pub const fn shows_image(self) -> bool {
        !matches!(self, Self::Link)
    }

    /// How much of the card's height the preview image takes.
    ///
    /// `Large` gives it most of the card — right for a video, where the frame *is* the content
    /// and the words are a caption. `Card` gives it about half, leaving room for the blurb,
    /// which is right for an article or a product.
    pub const fn image_fraction(self) -> f64 {
        match self {
            Self::Link => 0.0,
            Self::Card => 0.46,
            Self::Large => 0.62,
        }
    }
}

/// What an item actually is.
///
/// The variants are ordered by how much of the reference board they account for,
/// which is also the order they were added in: sticky, text, ink and image cover the
/// drawing pipeline end to end, and the six that follow are the ones the importer
/// was already decoding with full fidelity but had nowhere to put — 167 of the
/// board's 596 widgets between them.
///
/// Every variant is a *slot in the document*, not a promise that something draws it.
/// Holding the data is what makes a renderer for it a re-render of existing boards
/// rather than a re-import from Miro, and `docs/02-miro-formats.md` is explicit that
/// the clipboard is the only route to some of this content.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemKind {
    /// A sticky note. Miro's `sticker`.
    Sticky {
        text: StyledText,
        /// `None` means "use the board theme's default", not "transparent".
        background: Option<Color>,
    },
    /// Free-standing text. Miro's `text`.
    Text { text: StyledText },
    /// One freehand stroke. Miro's `paint`, and the reason the clipboard route
    /// beats every Miro API — no REST or Web SDK endpoint exposes drawings at all.
    Ink {
        /// Stroke points relative to the item's placement, in world units.
        points: Vec<Point>,
        /// `None` means the theme's default ink colour. Miro's separate stroke
        /// opacity (`lo`) is folded into this colour's alpha.
        color: Option<Color>,
        /// Stroke width in world units.
        thickness: f64,
    },
    /// A bitmap. `asset_id` is a content hash into the shared blob store, *not*
    /// Miro's `resource.id`; the importer resolves one to the other so a board file
    /// never depends on Miro's namespace.
    Image { asset_id: String, crop: Option<Crop> },
    /// A link card built from a page's OpenGraph metadata. Miro's `preview`, and at
    /// 91 instances the commonest non-drawing widget on the reference board.
    ///
    /// Every field is optional because Miro's `openGraph` block is: a page that
    /// serves no description gives us none, and an absent description is a different
    /// thing from an empty one — the first draws no line, the second draws a blank.
    LinkPreview {
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
        /// Content hash of the card's thumbnail in the shared blob store. `None`
        /// when the image was never fetched; the card still renders without it.
        thumbnail: Option<String>,
        /// The site's name — `"AliExpress"`, `"GitHub"`. Derived from the URL's host
        /// rather than fetched, so it is known the instant a link is pasted; a fetch
        /// may replace it with the provider's own `og:site_name`.
        provider: Option<String>,
        /// Content hash of the site's favicon in the blob store.
        favicon: Option<String>,
        /// How much of the card to draw. See [`CardMode`].
        mode: CardMode,
    },
    /// A rich embed — YouTube, Figma, Google Docs. Miro's `embed`.
    ///
    /// It renders as a card, not as a live frame: §1 of `docs/01-architecture.md`
    /// rules out a webview, and a GPU canvas cannot host an iframe. The markup is
    /// still stored because it cannot be reconstructed from the URL, so the day a
    /// `wry` overlay lands, existing boards already carry what it needs.
    ///
    /// The difference from [`ItemKind::LinkPreview`] is provenance, not appearance:
    /// Miro produces an `embed` for a URL its provider offers oEmbed for and a
    /// `preview` for everything else. Both draw as cards here, and both carry the same
    /// display fields — which is why a fetch, a thumbnail and a mode work the same on
    /// either.
    Embed {
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
        /// oEmbed provider name, e.g. `"YouTube"`.
        provider: Option<String>,
        /// The provider's `<iframe>` markup, verbatim.
        html: Option<String>,
        /// Content hash of the preview image — a video's poster frame, usually.
        ///
        /// An embed had no thumbnail field at all until links were made first-class,
        /// which is why all 41 on the reference board drew as blank cards.
        thumbnail: Option<String>,
        favicon: Option<String>,
        mode: CardMode,
    },
    /// A line between two things. Miro's `line`.
    ///
    /// The endpoints carry **bindings**, not baked-down world points — see
    /// [`ConnectorEnd`]. That indirection is the entire value of the variant: a
    /// connector with bindings re-routes when either end is dragged, and one without
    /// them detaches on the first drag and starts lying about the diagram.
    Connector {
        start: ConnectorEnd,
        end: ConnectorEnd,
        routing: Routing,
        dash: Dash,
        /// Stroke width in world units, before [`Placement`]'s
        /// scale. Miro's `t`; the reference board uses 2 throughout.
        thickness: f64,
        /// `None` means the theme's default connector colour.
        color: Option<Color>,
        captions: Vec<ConnectorCaption>,
    },
    /// A named region that contains other items and doubles as a presentation slide.
    /// Miro's `frame`.
    ///
    /// Containment is the movable tree, not a coordinate transform, so an item
    /// inside a frame keeps its world position. The frame's **background colour is
    /// [`Style::fill`]**, not a field here — one colour with two homes is one colour
    /// to keep in sync, and putting it on the style is what makes the format painter
    /// and style presets work on frames without a special case.
    Frame {
        /// The frame's name. Styled rather than a `String` so a frame title is one
        /// more thing the text pipeline already handles: searchable through
        /// [`ItemKind::text`], editable through [`Board::set_text`](crate::Board::set_text).
        title: StyledText,
        /// Position in the presentation running order. Miro's `prevFrameIndex`;
        /// `None` for a frame that is not part of the deck.
        order: Option<i64>,
        speaker_notes: Option<String>,
    },
    /// One of the catalogue's forms — rectangle, ellipse, diamond, cylinder, the
    /// twenty flowchart symbols, and the rest of `vellum-shapes`'s forty-one.
    ///
    /// **`form` is an opaque token, not a structured value.** This crate depends on
    /// `loro` and `thiserror` and nothing else, and `docs/01-architecture.md` has the
    /// dependency arrows pointing strictly downward — so holding a
    /// `vellum_shapes::Shape` here would invert them and drag `lyon` into the document
    /// layer. `vellum-app` depends on both and owns the encoding; everything here does
    /// is store the string and give it back unchanged.
    ///
    /// Fill, stroke and stroke width are [`Style`], like every other kind's. A shape
    /// with a colour of its own would be a second place for a colour to live.
    Shape {
        /// What `vellum-app` encoded. Round-trips verbatim; an unreadable one draws as
        /// a rectangle rather than costing the item.
        form: String,
        /// The shape's label. Miro's shapes carry text and so do these, which is also
        /// what makes them searchable and editable through the paths that already
        /// exist — see [`ItemKind::text`].
        text: StyledText,
    },
    /// A table. Miro's `table`.
    ///
    /// **`model` is an opaque token**, for the reason [`ItemKind::Shape`] gives: this
    /// crate depends on `loro` and `thiserror` and nothing else. `vellum-app` owns the
    /// encoding.
    ///
    /// Unlike a sticky's text — a Loro rich-text container, so two edits to different
    /// words merge — a table is one value, and editing a cell rewrites all of it.
    /// Deliberate: collaboration is cut and undo is per-gesture, so the merge grain
    /// buys nothing today, and the alternative is a Loro schema for merges, spans and
    /// per-cell styling before a single table can exist. Finer grain later is a
    /// re-encode of data that is already here, not a re-import.
    Table {
        /// The serialised `vellum_table::Table`.
        model: String,
    },
    /// A chart built from a data table. Miro's `chart`.
    ///
    /// Opaque `spec`, same reasoning as [`ItemKind::Table`] and [`ItemKind::Shape`].
    Chart {
        /// The serialised `vellum_chart::ChartSpec` — kind, data and style together,
        /// because a chart's data is not meaningful without knowing how it is drawn.
        spec: String,
    },
    /// A mind map: a node tree and the automatic layout that arranges it. Miro's
    /// `mindmap`.
    ///
    /// Opaque `model`, same reasoning as [`ItemKind::Table`] and [`ItemKind::Shape`].
    ///
    /// The **whole tree** is one value, not one item per node. A mind map's nodes have
    /// no independent existence — their positions are computed by the tidy layout from
    /// the tree's shape, so nine document items would carry nine placements that
    /// nothing is allowed to believe. It is one item whose contents happen to be a
    /// tree, which is also what makes "move the map" a single drag.
    MindMap {
        /// The serialised `crate::mindmap::MindMapModel` in `vellum-app` — the tree
        /// plus which of the three layout forms and which connector form it is drawn
        /// in, because a tree alone does not say what shape the map is.
        model: String,
    },
    /// A kanban board: columns of cards, with WIP limits. Miro's `kanban`.
    ///
    /// Opaque `board`, same reasoning as [`ItemKind::Table`] and [`ItemKind::Shape`],
    /// and one item for the whole container for the reason [`ItemKind::MindMap`] gives:
    /// a card's position is decided by the column it is in and its rank within it, so
    /// six document items would carry six placements nothing is allowed to believe.
    ///
    /// Note that `vellum-flow` intends a finer grain eventually — its own
    /// documentation says a card's id is what would map to a board item once the card's
    /// text is a document rich-text container. That is a re-encode of data already
    /// here, not a re-import, and it is not what this stores.
    Kanban {
        /// The serialised `vellum_flow::Kanban` — columns, cards, ranks and limits.
        board: String,
    },
    /// A container with no visual payload — Miro's `type: 10` group object, which
    /// arrives carrying nothing but a list of children.
    ///
    /// It draws nothing and it is not a frame: it does not clip, it has no title and
    /// it is not a slide. What it does is make a selection transform as one thing,
    /// which the movable tree already provides.
    Group,
    /// An embedded PDF. Miro's `document`.
    Document {
        /// Content hash of the PDF in the shared blob store.
        asset_id: String,
        /// Pages in the document, or `0` before it has been probed. Page rendering
        /// is a `pdfium`/`mupdf` decode into a texture, which is why the count is
        /// stored rather than derived — the board library must list a document
        /// without opening it.
        page_count: u32,
        /// Zero-based page currently shown. Stored verbatim: a value past the end is
        /// clamped where it is drawn, so a page count that arrives later cannot
        /// retroactively invalidate what is on disk.
        current_page: u32,
    },
}

impl ItemKind {
    /// A link card for a page, with its display fields unset.
    ///
    /// A builder rather than a struct literal at every call site, and the reason is the shape
    /// of this variant's history: it grew a thumbnail, then a provider, a favicon and a mode,
    /// and each addition was a compile error in every caller including four tests that cared
    /// about none of them. The fetched material and the display mode have defaults that mean
    /// "not fetched yet, draw the ordinary card", so a caller that knows only a URL should not
    /// have to name them.
    pub fn link_preview(
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
    ) -> Self {
        Self::LinkPreview {
            title,
            url,
            description,
            thumbnail: None,
            provider: None,
            favicon: None,
            mode: CardMode::default(),
        }
    }

    /// An oEmbed card, with its display fields unset. See [`ItemKind::link_preview`].
    pub fn embed(
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
        provider: Option<String>,
        html: Option<String>,
    ) -> Self {
        Self::Embed {
            title,
            url,
            description,
            provider,
            html,
            thumbnail: None,
            favicon: None,
            mode: CardMode::default(),
        }
    }


    /// The discriminant written into the document. Changing one of these strings is
    /// a file-format change.
    ///
    /// The tags match `vellum_import`'s `WidgetKind::label` so an import fidelity
    /// report can be joined against item counts without a translation table.
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Sticky { .. } => "sticky",
            Self::Text { .. } => "text",
            Self::Ink { .. } => "ink",
            Self::Image { .. } => "image",
            Self::LinkPreview { .. } => "link_preview",
            Self::Embed { .. } => "embed",
            Self::Connector { .. } => "connector",
            Self::Frame { .. } => "frame",
            Self::Shape { .. } => "shape",
            Self::Table { .. } => "table",
            Self::Chart { .. } => "chart",
            Self::MindMap { .. } => "mindmap",
            Self::Kanban { .. } => "kanban",
            Self::Group => "group",
            Self::Document { .. } => "document",
        }
    }

    /// The item's text, for the kinds that have any. Used by search, accessibility
    /// and the board-library preview.
    ///
    /// A link preview's title and an embed's are deliberately absent: they are
    /// metadata scraped from someone else's page, not text the user wrote, and
    /// folding them in would make a search for "Cooling" match a YouTube video
    /// nobody on this board named.
    pub const fn text(&self) -> Option<&StyledText> {
        match self {
            Self::Sticky { text, .. } | Self::Text { text } | Self::Shape { text, .. } => {
                Some(text)
            }
            Self::Frame { title, .. } => Some(title),
            Self::Ink { .. }
            | Self::Image { .. }
            | Self::LinkPreview { .. }
            | Self::Embed { .. }
            | Self::Connector { .. }
            | Self::Group
            | Self::Table { .. }
            | Self::Chart { .. }
            | Self::MindMap { .. }
            | Self::Kanban { .. }
            | Self::Document { .. } => None,
        }
    }
}

/// One end of a connector: what it is attached to, and where on it.
///
/// Miro stores an end as `{"point": {"x": 1, "y": 0.5}, "widgetIndex": 219}` — a
/// **normalised attachment on the target's bounds**, not a world coordinate.
/// `docs/02-miro-formats.md` calls preserving that indirection the point of
/// importing connectors at all, and `vellum-connect` is built the same way: geometry
/// is derived from live bounds on every route rather than stored.
///
/// # What happens when the target is deleted
///
/// **The binding dangles.** Removing an item does not rewrite the connectors bound
/// to it, and reading one back returns the id it was written with even after that id
/// is gone. Three reasons, in order of weight:
///
/// 1. **Loro's undo re-creates a deleted node under a *new* [`ItemId`]** — verified,
///    and pinned by a test in [`board`](crate::board). So auto-unbinding would
///    destroy the binding on delete and *still* not restore it on undo, while also
///    undoing its own rewrite and leaving `target` pointing at the dead id anyway.
///    It pays an operation for a strictly worse outcome.
/// 2. **A CRDT cannot cascade referential updates.** One peer deleting the target
///    while another drags the connector would produce two conflicting rewrites of
///    the same field, with no correct merge. A binding that is inert data has no
///    such conflict — resolution happens on read, locally, every time.
/// 3. It keeps the information. A dangling binding still says which item this line
///    described, which is what makes repair possible at all.
///
/// Resolve with [`Board::contains`](crate::Board::contains) before use, and repair
/// with [`Board::rebind_connectors`](crate::Board::rebind_connectors), which
/// retargets or cuts loose every endpoint bound to an id in one undoable step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnectorEnd {
    /// The item this end attaches to, or `None` for an end pinned to the canvas.
    pub target: Option<ItemId>,
    /// A normalised position on a rectangle: `(0, 0)` is its top-left corner,
    /// `(1, 1)` the bottom-right, `(1, 0.5)` the middle of its right edge.
    ///
    /// **Which rectangle depends on `target`**: the target item's own unrotated
    /// bounds when it is bound, and the connector item's own
    /// [`Placement`] when it is not. One rule either way, so a
    /// free end still has real bounds for culling and hit-testing, and dragging it
    /// moves the connector's own rectangle rather than needing a second coordinate
    /// space.
    ///
    /// Values are not clamped. Miro has only ever been observed emitting the four
    /// edge midpoints and the centre; an out-of-range value is preserved so a change
    /// there shows up as a visibly misplaced endpoint rather than being silently
    /// snapped onto an edge.
    pub anchor: (f64, f64),
    pub arrowhead: ArrowKind,
}

impl ConnectorEnd {
    /// The centre of a rectangle — Miro's centre attachment, and the default anchor.
    pub const CENTER: (f64, f64) = (0.5, 0.5);
    pub const LEFT: (f64, f64) = (0.0, 0.5);
    pub const RIGHT: (f64, f64) = (1.0, 0.5);
    pub const TOP: (f64, f64) = (0.5, 0.0);
    pub const BOTTOM: (f64, f64) = (0.5, 1.0);

    /// An end attached to `target`, which re-resolves as the target moves.
    pub const fn bound(target: ItemId, anchor: (f64, f64)) -> Self {
        Self { target: Some(target), anchor, arrowhead: ArrowKind::None }
    }

    /// An end pinned to the connector's own placement rectangle.
    pub const fn free(anchor: (f64, f64)) -> Self {
        Self { target: None, anchor, arrowhead: ArrowKind::None }
    }

    pub const fn with_arrowhead(mut self, arrowhead: ArrowKind) -> Self {
        self.arrowhead = arrowhead;
        self
    }
}

impl Default for ConnectorEnd {
    fn default() -> Self {
        Self::free(Self::CENTER)
    }
}

/// A connector terminator.
///
/// The variants are the shapes Miro's UI offers, named one for one with
/// `vellum_connect::Arrowhead` so converting between the document and the geometry
/// crate is a rename rather than a lookup table. Only "none" and "filled triangle"
/// have been seen on a real board; `docs/02-miro-formats.md` records which of Miro's
/// integer codes are verified and which are inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ArrowKind {
    #[default]
    None,
    /// An open "V" of two strokes, drawn at the line's own thickness.
    LineArrow,
    FilledTriangle,
    OpenTriangle,
    Circle,
    FilledCircle,
    Diamond,
    FilledDiamond,
}

impl ArrowKind {
    /// The token stored in the document.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LineArrow => "line",
            Self::FilledTriangle => "filled_triangle",
            Self::OpenTriangle => "open_triangle",
            Self::Circle => "circle",
            Self::FilledCircle => "filled_circle",
            Self::Diamond => "diamond",
            Self::FilledDiamond => "filled_diamond",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "none" => Self::None,
            "line" => Self::LineArrow,
            "filled_triangle" => Self::FilledTriangle,
            "open_triangle" => Self::OpenTriangle,
            "circle" => Self::Circle,
            "filled_circle" => Self::FilledCircle,
            "diamond" => Self::Diamond,
            "filled_diamond" => Self::FilledDiamond,
            _ => return None,
        })
    }
}

/// How a connector gets from one endpoint to the other — Miro's `lt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Routing {
    /// A single straight segment. Every connector on the reference board.
    #[default]
    Straight,
    /// A cubic bezier leaving each endpoint along its edge normal.
    Curved,
    /// Axis-aligned segments that route around obstacles.
    Orthogonal,
}

impl Routing {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Straight => "straight",
            Self::Curved => "curved",
            Self::Orthogonal => "orthogonal",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "straight" => Self::Straight,
            "curved" => Self::Curved,
            "orthogonal" => Self::Orthogonal,
            _ => return None,
        })
    }
}

/// Whether a connector is drawn solid, dashed or dotted — Miro's `ls`.
///
/// The pattern itself — dash-on and dash-off lengths — is not stored: it scales with
/// thickness, and `vellum_connect::LineStyle::dash_pattern` derives it. Storing both
/// would let a board be saved with a dash length that contradicts its own style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

impl Dash {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Solid => "solid",
            Self::Dashed => "dashed",
            Self::Dotted => "dotted",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "solid" => Self::Solid,
            "dashed" => Self::Dashed,
            "dotted" => Self::Dotted,
            _ => return None,
        })
    }
}

/// A label riding on a connector.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorCaption {
    pub text: StyledText,
    /// Where the caption sits along the routed path: `0.0` at the start endpoint,
    /// `1.0` at the end.
    ///
    /// A fraction of arc length rather than a world point, for the same reason the
    /// endpoints are bindings — a caption pinned to a coordinate slides off its own
    /// line the moment either end moves. The reference board carries no captioned
    /// connector (`"line": {"captions": []}` on all 18), so the fraction is taken
    /// from Miro's REST model, where a caption's position is a percentage.
    pub position: f64,
}

impl ConnectorCaption {
    /// A caption at the midpoint, which is where Miro places a new one.
    pub fn new(text: impl Into<StyledText>) -> Self {
        Self { text: text.into(), position: 0.5 }
    }

    pub fn at(text: impl Into<StyledText>, position: f64) -> Self {
        Self { text: text.into(), position }
    }
}

/// Item-level styling: the properties that apply to the whole item rather than to
/// a run of characters.
///
/// The split follows Miro's own: `ffn`/`fs`/`ta`/`lh` are widget-level keys there,
/// while bold/italic/underline/link vary inside the rich-text HTML. Per-character
/// formatting lives in [`SpanStyle`](crate::SpanStyle).
///
/// Every field is optional and `None` means "inherit the board's default", so a
/// freshly created sticky carries no style at all and picks up theme changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Style {
    /// Miro's `ffn`, e.g. `"Noto Sans"`.
    pub font_family: Option<String>,
    /// Miro's `fs`, in world units. `None` is auto-fit (Miro's `fs: 0, fsa: 1`).
    pub font_size: Option<f64>,
    /// Miro's `tc`. Individual spans may override it.
    pub text_color: Option<Color>,
    /// Miro's `ta`.
    pub align: Option<Align>,
    /// Miro's `lh`, a multiple of the font size.
    pub line_height: Option<f64>,
    /// Whole-item opacity, 0.0–1.0, applied on top of any per-colour alpha.
    pub opacity: Option<f64>,
    /// The colour behind the item's content — Miro's `bc`. A frame's background
    /// today; a shape's interior when shapes land.
    ///
    /// `None` inherits the board's default. **Transparent is `Some` with a zero
    /// alpha**, which is a different statement: one says "whatever the theme says",
    /// the other says "nothing, deliberately".
    ///
    /// A sticky's colour is *not* this. It stays on [`ItemKind::Sticky`] because in
    /// Miro it is what a sticky is — the colour packs are the widget, the picker is
    /// on the note itself — whereas a fill is a property a frame merely has.
    pub fill: Option<Color>,
    /// The colour of an item's outline — Miro's `bo`. A shape's border today.
    ///
    /// `None` inherits the theme's, on the same reading as [`Style::fill`]: an absent
    /// value defers, a transparent one refuses. An ink stroke's colour is not this —
    /// it stays on [`ItemKind::Ink`] for the reason a sticky's background stays on the
    /// sticky, because a stroke *is* its colour rather than merely having one.
    pub stroke: Option<Color>,
    /// Outline width in world units — Miro's `bw`. `None` is a hairline.
    pub stroke_width: Option<f64>,
    /// Whether the item refuses to be selected, moved, resized or deleted by pointer.
    ///
    /// A `bool` rather than `Option<bool>`, unlike every other field here, and the
    /// difference is deliberate: the others distinguish "inherit the theme's" from "none,
    /// deliberately", which is a real distinction for a colour. There is no theme default
    /// for *locked* — an item either is or is not — so a third state would be a state
    /// nothing could act on. `false` is therefore also the default, which keeps
    /// [`Style::is_default`] answering `true` for every item written before this field
    /// existed and leaves the on-disk format unchanged for them.
    ///
    /// **This is a UI affordance, not a permission.** The document layer stores it and
    /// nothing here enforces it: `Board::set_placement` on a locked item succeeds. The
    /// enforcement is in `vellum-app`, at the pointer, which is where "locked" means
    /// something — the properties panel can still unlock it, and undo can still move it,
    /// both of which are correct.
    pub locked: bool,
}

impl Style {
    /// True when nothing needs to be written to the document.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// One item, read out of the document.
///
/// This is a snapshot, not a live handle: mutating it changes nothing. Edits go
/// through [`Board`](crate::Board) so that every change is one CRDT transaction and
/// therefore one undo step.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: ItemId,
    /// The frame or group containing this item; `None` for a top-level item.
    /// Parenting does not transform coordinates — see [`Placement`].
    pub parent: Option<ItemId>,
    pub placement: Placement,
    pub style: Style,
    pub kind: ItemKind,
}

/// The description of an item to add.
///
/// A struct rather than a long argument list because the optional parts — parent,
/// style — are exactly the ones that would otherwise be a row of `None`s at every
/// call site.
#[derive(Debug, Clone, PartialEq)]
pub struct NewItem {
    pub kind: ItemKind,
    pub placement: Placement,
    pub style: Style,
    pub parent: Option<ItemId>,
}

impl NewItem {
    pub fn new(kind: ItemKind, placement: Placement) -> Self {
        Self { kind, placement, style: Style::default(), parent: None }
    }

    pub fn with_parent(mut self, parent: impl Into<Option<ItemId>>) -> Self {
        self.parent = parent.into();
        self
    }

    pub fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// An item's stacking position among its siblings.
///
/// This is Loro's fractional index for the tree node, rendered as uppercase hex.
/// Fractional rather than integer so that inserting between two items writes one
/// key and touches nothing else — with integer z-orders, dropping a sticky between
/// two of a thousand items renumbers up to a thousand rows, and in a CRDT every one
/// of those is an operation to store, sync and merge.
///
/// Ordering is plain lexicographic byte order on the hex string, which is exactly
/// the underlying fractional order: the encoding is big-endian, so comparing digit
/// by digit compares the numbers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZIndex(String);

impl ZIndex {
    pub(crate) const fn new(hex: String) -> Self {
        Self(hex)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ZIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_round_trip_through_their_string_form() {
        let id = ItemId::from_tree_id(TreeID::new(7, 42));
        let text = id.to_string();
        assert_eq!(text, "42@7");
        assert_eq!(text.parse::<ItemId>().unwrap(), id);
    }

    #[test]
    fn malformed_item_ids_are_rejected_with_the_input_in_the_message() {
        let err = "not-an-id".parse::<ItemId>().unwrap_err();
        assert!(err.to_string().contains("not-an-id"), "{err}");
    }

    /// Every kind, in the order they were added. These strings are the file format
    /// and they are also what an import fidelity report joins on, so a rename here
    /// is a change in two places at once.
    fn one_of_every_kind() -> Vec<ItemKind> {
        vec![
            ItemKind::Sticky { text: StyledText::plain("a"), background: None },
            ItemKind::Text { text: StyledText::plain("b") },
            ItemKind::Ink { points: vec![], color: None, thickness: 1.0 },
            ItemKind::Image { asset_id: "h".into(), crop: None },
            ItemKind::link_preview(None, None, None),
            ItemKind::embed(None, None, None, None, None),
            ItemKind::Connector {
                start: ConnectorEnd::default(),
                end: ConnectorEnd::default(),
                routing: Routing::Straight,
                dash: Dash::Solid,
                thickness: 2.0,
                color: None,
                captions: Vec::new(),
            },
            ItemKind::Frame { title: StyledText::plain("f"), order: None, speaker_notes: None },
            ItemKind::Group,
            ItemKind::Document { asset_id: "h".into(), page_count: 0, current_page: 0 },
        ]
    }

    #[test]
    fn kind_tags_are_distinct_and_stable() {
        let tags: Vec<_> = one_of_every_kind().iter().map(ItemKind::tag).collect();
        assert_eq!(
            tags,
            [
                "sticky",
                "text",
                "ink",
                "image",
                "link_preview",
                "embed",
                "connector",
                "frame",
                "group",
                "document"
            ]
        );

        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len(), "two kinds share a tag");
    }

    #[test]
    fn only_text_bearing_kinds_expose_text() {
        let sticky = ItemKind::Sticky { text: StyledText::plain("hi"), background: None };
        assert_eq!(sticky.text().map(StyledText::to_plain).as_deref(), Some("hi"));

        // A frame's name is text, so it is searchable and editable like any other.
        let frame = ItemKind::Frame {
            title: StyledText::plain("Engine bay"),
            order: Some(3),
            speaker_notes: None,
        };
        assert_eq!(frame.text().map(StyledText::to_plain).as_deref(), Some("Engine bay"));

        // Scraped metadata is not the user's text and must not answer a search.
        let preview = ItemKind::link_preview(
            Some("Cooling fan review".into()),
            Some("https://example.com".into()),
            None,
        );
        assert!(preview.text().is_none());
        assert!(ItemKind::Image { asset_id: "h".into(), crop: None }.text().is_none());
        assert!(ItemKind::Group.text().is_none());
    }

    #[test]
    fn connector_enum_tags_round_trip() {
        for kind in [
            ArrowKind::None,
            ArrowKind::LineArrow,
            ArrowKind::FilledTriangle,
            ArrowKind::OpenTriangle,
            ArrowKind::Circle,
            ArrowKind::FilledCircle,
            ArrowKind::Diamond,
            ArrowKind::FilledDiamond,
        ] {
            assert_eq!(ArrowKind::from_tag(kind.tag()), Some(kind), "{kind:?}");
        }
        assert_eq!(ArrowKind::from_tag("triangle"), None);

        for routing in [Routing::Straight, Routing::Curved, Routing::Orthogonal] {
            assert_eq!(Routing::from_tag(routing.tag()), Some(routing));
        }
        assert_eq!(Routing::from_tag("elbow"), None);

        for dash in [Dash::Solid, Dash::Dashed, Dash::Dotted] {
            assert_eq!(Dash::from_tag(dash.tag()), Some(dash));
        }
        assert_eq!(Dash::from_tag("dashdot"), None);
    }

    /// The defaults are the ones a connector drawn with no configuration needs:
    /// unbound, attached at the centre, no arrowhead.
    #[test]
    fn a_default_connector_end_is_free_and_centred() {
        let end = ConnectorEnd::default();
        assert_eq!(end.target, None);
        assert_eq!(end.anchor, ConnectorEnd::CENTER);
        assert_eq!(end.arrowhead, ArrowKind::None);

        let target = ItemId::from_tree_id(TreeID::new(3, 9));
        let bound = ConnectorEnd::bound(target, ConnectorEnd::RIGHT)
            .with_arrowhead(ArrowKind::FilledTriangle);
        assert_eq!(bound.target, Some(target));
        assert_eq!(bound.anchor, (1.0, 0.5));
        assert_eq!(bound.arrowhead, ArrowKind::FilledTriangle);
        assert_eq!(ConnectorEnd::free(ConnectorEnd::TOP).anchor, (0.5, 0.0));
    }

    #[test]
    fn a_new_caption_sits_at_the_midpoint() {
        let caption = ConnectorCaption::new("feeds");
        assert_eq!(caption.position, 0.5);
        assert_eq!(caption.text, StyledText::plain("feeds"));
        assert_eq!(ConnectorCaption::at("near the end", 0.9).position, 0.9);
    }

    /// A fill of `None` inherits the theme; a fill of transparent black does not.
    /// Collapsing the two would make "no background, deliberately" unexpressible.
    #[test]
    fn an_absent_fill_is_not_a_transparent_one() {
        let inherited = Style::default();
        let transparent = Style { fill: Some(Color::rgba(0, 0, 0, 0)), ..Style::default() };
        assert!(inherited.is_default());
        assert!(!transparent.is_default());
        assert_ne!(inherited, transparent);
    }

    /// The whole point of fractional indices: a value can always be placed between
    /// two neighbours, and string ordering must agree with fractional ordering.
    /// These are the exact indices Loro 1.13 produces for two nodes plus one
    /// inserted between them.
    #[test]
    fn z_index_string_order_matches_fractional_order() {
        let first = ZIndex::new("80".into());
        let middle = ZIndex::new("817F80".into());
        let last = ZIndex::new("8180".into());
        assert!(first < middle, "{first} !< {middle}");
        assert!(middle < last, "{middle} !< {last}");

        let mut shuffled = vec![last.clone(), first.clone(), middle.clone()];
        shuffled.sort();
        assert_eq!(shuffled, vec![first, middle, last]);
    }

    #[test]
    fn new_item_builder_defaults_to_unstyled_and_unparented() {
        let item =
            NewItem::new(ItemKind::Text { text: StyledText::plain("x") }, Placement::default());
        assert!(item.style.is_default());
        assert_eq!(item.parent, None);

        let parent = ItemId::from_tree_id(TreeID::new(1, 0));
        let child = item.with_parent(parent);
        assert_eq!(child.parent, Some(parent));
    }
}
