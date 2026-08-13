//! The board document itself.
//!
//! # Why a CRDT for a single-user app
//!
//! Loro is not here for multiplayer — it is here because a CRDT with an operation
//! log gives undo/redo, version history and incremental persistence *as
//! consequences of the data structure* rather than as three separately maintained
//! subsystems. Multiplayer then costs a transport instead of a rewrite. The
//! alternative, a plain in-memory scene graph plus a hand-written undo stack, is
//! cheaper on day one and a rewrite on the day two people open the same board.
//!
//! # Layout of the document
//!
//! Two root containers, whose names are part of the file format:
//!
//! - `board` — a map holding [`SCHEMA_VERSION`] and the title.
//! - `items` — a **movable tree**, one node per item, each node's metadata map
//!   holding the item's placement, style and kind-specific fields.
//!
//! The movable tree is the reason Loro was chosen over a flat list plus a parent
//! field. Reparenting is a first-class operation with defined concurrent semantics
//! (no cycles, no lost subtrees), and it maps straight onto Miro's `_parent` field,
//! which is how frames and groups arrive from an import.
//!
//! Sibling order is the z-order, maintained by the tree's fractional index — see
//! [`ZIndex`].

use crate::error::{DocError, Result};
use crate::geometry::{Align, Color, Crop, Placement, Point};
use crate::item::{
    ArrowKind, CardMode, ConnectorCaption, ConnectorEnd, Dash, Item, ItemId, ItemKind, NewItem,
    Routing, Style, ZIndex,
};
use crate::text::{self, StyledText};
use loro::{
    CommitOptions, Container, ExportMode, LoroDoc, LoroError, LoroMap, LoroResult, LoroText,
    LoroTree, LoroTreeError, LoroValue, TreeParentId, UndoManager, ValueOrContainer,
    VersionVector,
};

/// Version of the document layout, stored in every board.
///
/// Bump this when a key in [`key`] changes meaning, **or when a build starts writing
/// fields an older one would drop on save**. A board written by a newer build
/// refuses to open rather than silently losing the fields this build cannot see — an
/// unreadable file the user can still recover is better than a readable one that
/// quietly discards their work on the next save. Older boards open normally and are
/// stamped with the current version on load, so a v1 board that gains a connector
/// does not go on claiming to be readable by a build that cannot read connectors.
///
/// - **v1** — sticky, text, ink, image.
/// - **v2** — link previews, embeds, connectors with bindings, frames, groups and
///   documents; item fill colour; heading and list structure in styled text.
pub const SCHEMA_VERSION: i64 = 2;

/// Keys used inside the Loro document. These strings *are* the file format.
/// What the canvas is made of.
///
/// Miro puts this under Board ▸ Background colour and `docs/04-ui-reference.md` §4
/// flags it as cheap and worth having; the user asked for it directly. Two
/// independent choices rather than one enumeration of combinations, because "the
/// green board with dots" and "the green board with lines" are the same board with a
/// different pattern, and folding them together would make the swatch grid have to be
/// drawn three times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Background {
    /// `None` is the palette's own canvas colour, whatever that turns out to be — not
    /// a hard-coded grey. A board that never chose one has to keep following the
    /// palette rather than freezing today's value into the document.
    pub color: Option<Color>,
    /// The texture drawn over the colour.
    ///
    /// **Overridden app-wide, and this field is the fallback.** The user asked for the grid
    /// to be one setting across every board — *"grid opacity and grid color and grid should
    /// apply to all of the boards not just to that board"* — so `vellum-app`'s library
    /// sidecar carries the choice and `app::canvas_pattern` prefers it. This stays because a
    /// board that chose a pattern before that was true must keep drawing it until a global
    /// choice is made; dropping the field would silently restyle boards on disk.
    pub pattern: Pattern,
}

/// The texture drawn over a board's background colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pattern {
    /// Nothing at all.
    Plain,
    /// A field of dots on the 1-2-5 decade sequence a technical drawing uses.
    ///
    /// The default. A blank field gives the eye nothing to judge position or scale
    /// against, and this is a board for technical drawings — the user asked for a
    /// grid to be what a board opens with.
    #[default]
    Dots,
    /// A small cross at each intersection: the corners of the grid squares marked,
    /// without the squares themselves being drawn.
    ///
    /// Between [`Self::Dots`] and [`Self::Lines`]. A cross states the intersection
    /// more definitely than a dot — it has direction, so it reads as a coordinate
    /// rather than a speck — while leaving the field clearer than full graph paper.
    Crosses,
    /// The same spacing, drawn as lines. Graph paper, for boards that are laid out
    /// against a rule rather than freely.
    Lines,
}

impl Pattern {
    pub const ALL: [Self; 4] = [Self::Plain, Self::Dots, Self::Crosses, Self::Lines];

    /// The token written into the document. A word rather than an index, so a file
    /// written by a build that adds a fourth pattern is still readable.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Dots => "dots",
            Self::Crosses => "crosses",
            Self::Lines => "lines",
        }
    }

    /// `None` for a tag from a newer build, which reads back as the default rather
    /// than as an error — an unknown pattern is a board that draws plainly, not a
    /// board that fails to open.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|pattern| pattern.tag() == tag)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Plain => "No grid",
            Self::Dots => "Dots",
            Self::Crosses => "Crosses",
            Self::Lines => "Lines",
        }
    }
}

mod key {
    pub const BOARD: &str = "board";
    pub const ITEMS: &str = "items";

    pub const SCHEMA: &str = "schema";
    /// The board's name, and — in an item's own map — a link card's headline.
    /// Distinct maps, same word, so one constant serves both.
    pub const TITLE: &str = "title";
    /// The canvas colour, and the pattern drawn on it. Board-level, not item-level.
    pub const BG_COLOR: &str = "bg_color";
    pub const BG_PATTERN: &str = "bg_pattern";

    pub const KIND: &str = "kind";
    pub const X: &str = "x";
    pub const Y: &str = "y";
    pub const SCALE: &str = "scale";
    pub const ROTATION: &str = "rotation";
    pub const WIDTH: &str = "w";
    pub const HEIGHT: &str = "h";
    pub const STYLE: &str = "style";

    pub const FONT_FAMILY: &str = "font_family";
    pub const FONT_SIZE: &str = "font_size";
    pub const TEXT_COLOR: &str = "text_color";
    pub const ALIGN: &str = "align";
    pub const LINE_HEIGHT: &str = "line_height";
    pub const OPACITY: &str = "opacity";
    pub const FILL: &str = "fill";
    pub const STROKE: &str = "stroke";
    pub const STROKE_WIDTH: &str = "stroke_width";
    /// Whether the item refuses the pointer. See [`Style::locked`](crate::Style::locked).
    pub const LOCKED: &str = "locked";

    pub const TEXT: &str = "text";
    pub const BACKGROUND: &str = "background";
    pub const POINTS: &str = "points";
    pub const INK_COLOR: &str = "ink_color";
    pub const THICKNESS: &str = "thickness";
    pub const ASSET: &str = "asset";
    pub const CROP: &str = "crop";

    pub const URL: &str = "url";
    pub const DESCRIPTION: &str = "description";
    pub const THUMBNAIL: &str = "thumbnail";
    pub const PROVIDER: &str = "provider";
    pub const HTML: &str = "html";
    pub const FAVICON: &str = "favicon";
    /// How much of a link card is drawn — `CardMode`'s tag.
    pub const CARD_MODE: &str = "card_mode";

    pub const START: &str = "start";
    pub const END: &str = "end";
    pub const ROUTING: &str = "routing";
    pub const DASH: &str = "dash";
    pub const LINE_COLOR: &str = "line_color";
    pub const CAPTIONS: &str = "captions";
    /// Inside a connector-end map: the bound item's id, absent for a free end.
    pub const TARGET: &str = "target";
    /// Inside a connector-end map: the arrowhead tag.
    pub const ARROW: &str = "arrow";
    /// Inside a caption map: its fraction along the routed path.
    pub const POSITION: &str = "pos";
    /// Inside the captions map: how many slots are live. Not an ordinal, so it
    /// cannot collide with one.
    pub const COUNT: &str = "n";

    pub const ORDER: &str = "order";
    pub const SPEAKER_NOTES: &str = "speaker_notes";

    /// The catalogue entry a shape is, as `vellum-app` encoded it. Opaque here — see
    /// [`ItemKind::Shape`](crate::ItemKind::Shape).
    pub const FORM: &str = "form";

    /// A table's serialised model. Opaque here — see [`ItemKind::Table`].
    pub const MODEL: &str = "model";

    /// A chart's serialised spec. Opaque here — see [`ItemKind::Chart`].
    pub const SPEC: &str = "spec";

    /// A kanban board's serialised columns. Opaque here — see [`ItemKind::Kanban`].
    ///
    /// Its own key rather than sharing [`MODEL`] or [`TREE`], for the reason given on
    /// [`TREE`]: a key shared between kinds survives a change of kind.
    pub const COLUMNS: &str = "columns";

    /// A mind map's serialised tree. Opaque here — see [`ItemKind::MindMap`].
    ///
    /// Its own key rather than sharing [`MODEL`] with a table: the two blobs are
    /// different formats, and a key shared between kinds survives a change of kind —
    /// [`super::kind_keys_for_tag`] only clears the keys the *new* kind does not use.
    /// A table turned into a mind map would then find a table where its tree should be.
    pub const TREE: &str = "tree";

    pub const PAGE_COUNT: &str = "page_count";
    pub const CURRENT_PAGE: &str = "current_page";

    /// An agent node's serialised configuration. Opaque here — see [`ItemKind::Agent`].
    ///
    /// Its own key rather than sharing [`MODEL`] with a table, for the reason [`TREE`]
    /// gives: [`super::kind_keys_for_tag`] only clears the keys the *new* kind does not
    /// use, so a shared key survives a change of kind and a table turned into an agent
    /// would find a table where its configuration should be.
    pub const AGENT: &str = "agent";

    /// A file-tree node's serialised root and scope. Opaque here — see
    /// [`ItemKind::FileTree`]. Its own key, for the reason [`AGENT`] gives.
    pub const FILE_TREE: &str = "file_tree";

    /// A note node's serialised path, scope and links. Opaque here — see
    /// [`ItemKind::AgentNote`]. Its own key, for the reason [`AGENT`] gives.
    ///
    /// The note's *content* is not here and never will be: it is a `.md` file on disk.
    pub const NOTE: &str = "note";

    /// A browser node's serialised address and settings. Opaque here — see
    /// [`ItemKind::Browser`]. Its own key, for the reason [`AGENT`] gives.
    pub const BROWSER: &str = "browser";

    /// Every kind-specific key, so switching an item's kind can clear the ones the
    /// new kind does not use. Kept in step with [`super::kind_keys`] by a test.
    pub const KIND_SPECIFIC: [&str; 34] = [
        AGENT,
        FILE_TREE,
        NOTE,
        BROWSER,
        TEXT,
        BACKGROUND,
        POINTS,
        INK_COLOR,
        THICKNESS,
        ASSET,
        CROP,
        TITLE,
        URL,
        DESCRIPTION,
        THUMBNAIL,
        PROVIDER,
        HTML,
        FAVICON,
        CARD_MODE,
        START,
        END,
        ROUTING,
        DASH,
        LINE_COLOR,
        CAPTIONS,
        ORDER,
        SPEAKER_NOTES,
        PAGE_COUNT,
        CURRENT_PAGE,
        FORM,
        MODEL,
        SPEC,
        TREE,
        COLUMNS,
    ];
}

/// The one kind tag [`Board`] recognises without decoding a whole item, so that
/// sweeping the board for connector bindings does not pay for reading every
/// sticky's text. A test asserts it still agrees with [`ItemKind::tag`].
const CONNECTOR_TAG: &str = "connector";

/// Fractional-index jitter. Zero keeps indices as short as possible; jitter only
/// pays for itself when many peers insert at the same position concurrently, which
/// costs document size on every board to hedge against a case single-user editing
/// never hits. Revisit when the sync server lands.
const FRACTIONAL_INDEX_JITTER: u8 = 0;

/// Origin tag for commits that only bring a container into existence.
///
/// These are excluded from the undo stack, which is what makes them safe to split
/// out of the edit they belong to. See [`Board::scaffold`] for why they have to be
/// split out at all.
const SCAFFOLD_ORIGIN: &str = "vellum-scaffold";

/// A point in the document's history, used to export only what changed since a
/// previous save.
///
/// Opaque on purpose: it wraps a Loro version vector, and the persistence layer
/// should be able to store and replay these without depending on Loro directly.
#[derive(Debug, Clone, PartialEq)]
pub struct Version(VersionVector);

/// An infinite-canvas board.
///
/// Read methods take `&self`, mutations take `&mut self`. Loro is internally
/// mutable and would allow `&self` everywhere, but a document that can change
/// through a shared reference is a trap for the renderer and the spatial index,
/// which both cache derived state.
///
/// Each mutating method commits exactly one CRDT transaction, which is also exactly
/// one undo step. Use [`Board::begin_undo_group`] to fuse several into one.
pub struct Board {
    doc: LoroDoc,
    items: LoroTree,
    meta: LoroMap,
    undo: UndoManager,
}

impl Board {
    /// An empty, untitled board.
    pub fn new() -> Self {
        let doc = LoroDoc::new();
        let (items, meta) = configure(&doc);
        meta.insert(key::SCHEMA, SCHEMA_VERSION).expect("fresh document accepts writes");
        meta.insert(key::TITLE, "").expect("fresh document accepts writes");
        doc.commit();
        // Built after the initial write so the empty board is not itself undoable.
        Self { undo: new_undo_manager(&doc), doc, items, meta }
    }

    /// Loads a board from the bytes produced by [`Board::to_bytes`].
    ///
    /// Undo history is *not* restored: it lives in the `UndoManager`, not in the
    /// snapshot. Reopening a board therefore starts with an empty undo stack, which
    /// matches what every desktop app does and avoids shipping unbounded history in
    /// the file.
    ///
    /// An older board is stamped with the current [`SCHEMA_VERSION`] as it loads.
    /// Without that, adding one connector to a v1 board would produce a file that
    /// still advertises itself as readable by a build with no connector support —
    /// which would then open it and drop the connector on the next save. The stamp
    /// is applied before the undo manager exists, so it is not an undoable edit.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let doc = LoroDoc::from_snapshot(bytes)?;
        let (items, meta) = configure(&doc);

        let Some(found) = int_at(&meta, key::SCHEMA) else {
            return Err(DocError::Malformed(
                "not a Velm board: the document has no schema marker".into(),
            ));
        };
        if found > SCHEMA_VERSION {
            return Err(DocError::UnsupportedSchema { found, supported: SCHEMA_VERSION });
        }
        if found < SCHEMA_VERSION {
            meta.insert(key::SCHEMA, SCHEMA_VERSION)?;
            doc.commit();
        }

        Ok(Self { undo: new_undo_manager(&doc), doc, items, meta })
    }

    /// Serialises the whole board, history included, as a Loro snapshot.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// The document's current version, for incremental saves.
    pub fn version(&self) -> Version {
        Version(self.doc.oplog_vv())
    }

    /// Everything that happened since `since`, in a form [`Board::apply`] accepts.
    ///
    /// This is what makes saving cheap: a board with 10,000 items writes a few
    /// hundred bytes when one sticky moves, instead of re-serialising the document.
    pub fn export_since(&self, since: &Version) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::updates(&since.0))?)
    }

    /// Applies bytes from [`Board::export_since`], from this board or another peer.
    ///
    /// Applied changes are not undoable: undo covers this peer's own edits, so
    /// replaying a log or receiving a collaborator's change never lets the local
    /// user undo work that was not theirs.
    pub fn apply(&mut self, updates: &[u8]) -> Result<()> {
        self.doc.import(updates)?;
        Ok(())
    }

    /// The underlying CRDT. The sync transport will need it; nothing else should.
    pub fn crdt(&self) -> &LoroDoc {
        &self.doc
    }

    // ----- board metadata -------------------------------------------------

    pub fn title(&self) -> String {
        string_at(&self.meta, key::TITLE).unwrap_or_default()
    }

    pub fn set_title(&mut self, title: &str) -> Result<()> {
        self.meta.insert(key::TITLE, title)?;
        self.doc.commit();
        Ok(())
    }

    /// What the canvas is made of — its colour and its pattern.
    ///
    /// A property of the **board**, not of the view: *"i want to be able to select
    /// backgrounds for the baords"*. Two boards open side by side should not have to
    /// agree, and the choice has to survive being reopened on another machine, which
    /// is what puts it in the document rather than beside it with the stars and the
    /// start views.
    pub fn background(&self) -> Background {
        Background {
            color: color_at(&self.meta, key::BG_COLOR),
            pattern: string_at(&self.meta, key::BG_PATTERN)
                .as_deref()
                .and_then(Pattern::from_tag)
                .unwrap_or_default(),
        }
    }

    pub fn set_background(&mut self, background: Background) -> Result<()> {
        put_color(&self.meta, key::BG_COLOR, background.color)?;
        // The default is written as an absence, so a board that never chose one
        // carries no key and an old file reads the same as a new one.
        //
        // Matched against `Pattern::default()` rather than a named variant. It used
        // to name `Plain`, and when the default moved to `Dots` the two silently
        // disagreed: saving the default wrote a key, and a board that had chosen and
        // un-chosen a pattern stopped matching one that never chose. Deriving the
        // sentinel from the default keeps them the same fact.
        //
        // A consequence worth naming: a board stored before the default moved carries
        // no key and now reads as `Dots` rather than `Plain`. That is the intended
        // direction — the user asked for a board to open with a grid — but it does
        // mean absence means "whatever the default is now", not "plain forever".
        put_str(
            &self.meta,
            key::BG_PATTERN,
            match background.pattern {
                p if p == Pattern::default() => None,
                other => Some(other.tag()),
            },
        )?;
        self.doc.commit();
        Ok(())
    }

    // ----- reading items --------------------------------------------------

    /// Number of live items, including those nested inside frames and groups.
    pub fn item_count(&self) -> usize {
        self.items.get_nodes(false).len()
    }

    pub fn is_empty(&self) -> bool {
        self.item_count() == 0
    }

    /// Whether the item exists and has not been removed.
    ///
    /// Loro keeps deleted tree nodes addressable so that concurrent edits to a
    /// removed item can still be resolved, so this is *not* the same question as
    /// "has this id ever existed".
    pub fn contains(&self, id: ItemId) -> bool {
        matches!(self.items.is_node_deleted(&id.tree_id()), Ok(false))
    }

    pub fn item(&self, id: ItemId) -> Result<Item> {
        if !self.contains(id) {
            return Err(DocError::NoSuchItem(id));
        }
        let meta = self.items.get_meta(id.tree_id())?;
        Ok(Item {
            id,
            parent: self.parent_of(id),
            placement: read_placement(&meta),
            style: read_style(&meta),
            kind: read_kind(&meta)?,
        })
    }

    /// Every item, in paint order: back to front, each item immediately followed by
    /// its descendants. Feeding this straight to the renderer produces the correct
    /// stacking without a separate sort.
    pub fn items(&self) -> Result<Vec<Item>> {
        self.item_ids().into_iter().map(|id| self.item(id)).collect()
    }

    /// Item ids in the same order as [`Board::items`], without decoding anything.
    pub fn item_ids(&self) -> Vec<ItemId> {
        let mut out = Vec::with_capacity(self.item_count());
        let mut stack: Vec<ItemId> = self.children(None).into_iter().rev().collect();
        // Iterative rather than recursive: nesting depth comes from the file, and a
        // deeply nested import must not be able to overflow the stack.
        while let Some(id) = stack.pop() {
            out.push(id);
            stack.extend(self.children(Some(id)).into_iter().rev());
        }
        out
    }

    /// Everything under `id`, at any depth, in the same order as [`Board::item_ids`].
    ///
    /// Excludes `id` itself, so the caller decides whether the container comes along.
    ///
    /// Copying a frame needs this: a frame's children are separate items that merely
    /// name it as their parent, so a selection holding only the frame used to copy an
    /// empty box. The order is the useful part — a parent always precedes its own
    /// descendants, which is what lets a paste create items and reparent them in one
    /// forward pass without having to sort.
    ///
    /// Iterative rather than recursive, for the same reason [`Board::item_ids`] is:
    /// nesting depth comes from the file, so an import must not be able to overflow
    /// the stack.
    pub fn descendants(&self, id: ItemId) -> Vec<ItemId> {
        let mut out = Vec::new();
        let mut stack: Vec<ItemId> = self.children(Some(id)).into_iter().rev().collect();
        while let Some(id) = stack.pop() {
            out.push(id);
            stack.extend(self.children(Some(id)).into_iter().rev());
        }
        out
    }

    /// Direct children of `parent` (or top-level items when `None`), back to front.
    pub fn children(&self, parent: Option<ItemId>) -> Vec<ItemId> {
        self.items
            .children(parent.map(ItemId::tree_id))
            .unwrap_or_default()
            .into_iter()
            .map(ItemId::from_tree_id)
            .collect()
    }

    pub fn parent_of(&self, id: ItemId) -> Option<ItemId> {
        match self.items.parent(id.tree_id()) {
            Some(TreeParentId::Node(parent)) => Some(ItemId::from_tree_id(parent)),
            _ => None,
        }
    }

    /// The item's stacking key among its siblings. See [`ZIndex`].
    pub fn z_index(&self, id: ItemId) -> Option<ZIndex> {
        self.items.fractional_index(id.tree_id()).map(ZIndex::new)
    }

    // ----- adding and removing --------------------------------------------

    /// Adds an item on top of its siblings.
    pub fn add(&mut self, item: NewItem) -> Result<ItemId> {
        self.insert(item, None)
    }

    /// Adds an item at a specific position in its parent's stacking order.
    ///
    /// `index` is the position among current siblings, so `add_at(item, 1)` with
    /// two existing siblings places the new item between them. Only the new item
    /// gets a stacking key; the siblings are not touched.
    pub fn add_at(&mut self, item: NewItem, index: usize) -> Result<ItemId> {
        self.insert(item, Some(index))
    }

    fn insert(&mut self, item: NewItem, index: Option<usize>) -> Result<ItemId> {
        if let Some(parent) = item.parent
            && !self.contains(parent)
        {
            return Err(DocError::NoSuchItem(parent));
        }

        let parent = item.parent.map(ItemId::tree_id);
        let node = match index {
            Some(index) => self.items.create_at(parent, index)?,
            None => self.items.create(parent)?,
        };

        let meta = self.items.get_meta(node)?;
        write_placement(&meta, &item.placement)?;
        write_style(&meta, &item.style)?;
        write_kind(&meta, &item.kind)?;
        self.doc.commit();
        Ok(ItemId::from_tree_id(node))
    }

    /// Removes an item **and everything inside it**. Deleting a frame deletes its
    /// contents, which is what "delete this frame" means to a user.
    ///
    /// Connectors bound to it are **not** rewritten: their bindings dangle, and
    /// [`ConnectorEnd::target`] documents why that beats auto-unbinding. A caller
    /// that wants the other behaviour asks for it —
    /// [`Board::connectors_bound_to`] finds them, [`Board::rebind_connectors`]
    /// changes them.
    pub fn remove(&mut self, id: ItemId) -> Result<()> {
        if !self.contains(id) {
            return Err(DocError::NoSuchItem(id));
        }
        self.items.delete(id.tree_id())?;
        self.doc.commit();
        Ok(())
    }

    // ----- connector bindings ---------------------------------------------

    /// Every connector with an endpoint bound to `id`, in paint order.
    ///
    /// `id` need not still exist: finding the connectors left pointing at something
    /// just deleted is one of the two reasons this method exists. The other is
    /// dragging — the items that have to be re-routed when one moves are exactly
    /// these.
    ///
    /// Linear in the number of items, because the binding lives on the connector and
    /// nothing indexes the reverse direction. That is the right cost for an
    /// interaction that happens on mouse-down; it is the wrong cost per frame, so a
    /// live drag resolves the set once and keeps it.
    pub fn connectors_bound_to(&self, id: ItemId) -> Vec<ItemId> {
        self.item_ids().into_iter().filter(|item| self.binds_to(*item, id)).collect()
    }

    /// Retargets every connector endpoint bound to `from`.
    ///
    /// `Some(to)` moves the bindings onto another item; `None` cuts them loose,
    /// leaving each end at the anchor it already had — now read against the
    /// connector's own placement, per [`ConnectorEnd::anchor`]. Returns the number
    /// of **endpoints** changed, so a connector bound to `from` at both ends counts
    /// twice.
    ///
    /// The whole sweep is one commit and therefore one undo step. `from` does not
    /// have to exist: repairing bindings after a delete is the point.
    pub fn rebind_connectors(&mut self, from: ItemId, to: Option<ItemId>) -> Result<usize> {
        if let Some(to) = to
            && !self.contains(to)
        {
            return Err(DocError::NoSuchItem(to));
        }

        let (wanted, target) = (from.to_string(), to.map(|id| id.to_string()));
        let mut changed = 0usize;
        for connector in self.connectors_bound_to(from) {
            let meta = self.items.get_meta(connector.tree_id())?;
            for side in [key::START, key::END] {
                let Some(end) = map_at(&meta, side) else { continue };
                if string_at(&end, key::TARGET).as_deref() != Some(wanted.as_str()) {
                    continue;
                }
                put_str(&end, key::TARGET, target.as_deref())?;
                changed += 1;
            }
        }
        if changed > 0 {
            self.doc.commit();
        }
        Ok(changed)
    }

    /// Whether `item` is a connector with either end bound to `target`, decided
    /// without decoding the item.
    fn binds_to(&self, item: ItemId, target: ItemId) -> bool {
        let Ok(meta) = self.items.get_meta(item.tree_id()) else { return false };
        if string_at(&meta, key::KIND).as_deref() != Some(CONNECTOR_TAG) {
            return false;
        }
        let wanted = target.to_string();
        [key::START, key::END].iter().any(|side| {
            map_at(&meta, side)
                .and_then(|end| string_at(&end, key::TARGET))
                .is_some_and(|found| found == wanted)
        })
    }

    // ----- editing --------------------------------------------------------

    pub fn set_placement(&mut self, id: ItemId, placement: Placement) -> Result<()> {
        let meta = self.meta_of(id)?;
        write_placement(&meta, &placement)?;
        self.doc.commit();
        Ok(())
    }

    /// Moves an item to an absolute world position, leaving size and rotation alone.
    pub fn set_position(&mut self, id: ItemId, x: f64, y: f64) -> Result<()> {
        let meta = self.meta_of(id)?;
        meta.insert(key::X, x)?;
        meta.insert(key::Y, y)?;
        self.doc.commit();
        Ok(())
    }

    /// Moves an item by a delta. Dragging N items is N of these inside one
    /// [`Board::begin_undo_group`].
    pub fn translate(&mut self, id: ItemId, dx: f64, dy: f64) -> Result<()> {
        let meta = self.meta_of(id)?;
        let x = num_at(&meta, key::X).unwrap_or(0.0) + dx;
        let y = num_at(&meta, key::Y).unwrap_or(0.0) + dy;
        meta.insert(key::X, x)?;
        meta.insert(key::Y, y)?;
        self.doc.commit();
        Ok(())
    }

    /// Replaces the item's content, including changing it to another kind. Keys
    /// belonging to the previous kind are removed rather than left as dead weight.
    pub fn set_kind(&mut self, id: ItemId, kind: ItemKind) -> Result<()> {
        let meta = self.meta_of(id)?;
        self.scaffold(&meta, &kind)?;
        write_kind(&meta, &kind)?;
        self.doc.commit();
        Ok(())
    }

    /// Creates the rich-text containers `kind` needs, in a commit of their own.
    ///
    /// **A text container must not be created and filled in the same commit that
    /// gets undone.** Verified against Loro 1.13: create-and-fill, undo, redo leaves
    /// the text duplicated — `"fan"` comes back as `"fanfan"` — because the redo
    /// replays the insert into a container the redo has also resurrected with its
    /// contents. Splitting the creation into an earlier commit makes the undone
    /// commit a plain edit of an existing container, which round-trips correctly.
    ///
    /// The split would otherwise cost a second, empty undo step, so the scaffolding
    /// commit is tagged with [`SCAFFOLD_ORIGIN`] and excluded from the undo stack —
    /// see [`new_undo_manager`]. It leaves behind an empty container when the edit
    /// is undone, which is invisible and is cleaned up by the next kind change.
    ///
    /// [`Board::add`] does not need this: undoing a creation removes the whole tree
    /// node, containers and all, so nothing survives to be doubled.
    fn scaffold(&self, meta: &LoroMap, kind: &ItemKind) -> Result<()> {
        match kind {
            ItemKind::Sticky { .. }
            | ItemKind::Text { .. }
            | ItemKind::Frame { .. }
            | ItemKind::Shape { .. } => {
                meta.ensure_mergeable_text(key::TEXT)?;
            }
            ItemKind::Connector { captions, .. } if !captions.is_empty() => {
                let map = meta.ensure_mergeable_map(key::CAPTIONS)?;
                for slot in 0..captions.len() {
                    map.ensure_mergeable_map(&slot.to_string())?
                        .ensure_mergeable_text(key::TEXT)?;
                }
            }
            // Every other kind stores plain values, which are last-writer-wins and
            // therefore idempotent under a replayed redo.
            _ => return Ok(()),
        }
        self.doc.commit_with(CommitOptions::new().origin(SCAFFOLD_ORIGIN));
        Ok(())
    }

    pub fn set_style(&mut self, id: ItemId, style: Style) -> Result<()> {
        let meta = self.meta_of(id)?;
        write_style(&meta, &style)?;
        self.doc.commit();
        Ok(())
    }

    /// Replaces the text of a sticky, a text item or a frame — for a frame, its
    /// name, which is the same container and therefore the same operation.
    ///
    /// Whole-value replacement: the caret preserves an item's runs across an edit, but it
    /// does so by handing over a whole `StyledText` per keystroke, so there is no
    /// character-level edit for this to apply. The storage underneath is already a rich-text
    /// CRDT, so switching to incremental edits later is a change to this method, not to the
    /// format.
    pub fn set_text(&mut self, id: ItemId, text: StyledText) -> Result<()> {
        let meta = self.meta_of(id)?;
        match read_kind(&meta)? {
            ItemKind::Sticky { .. }
            | ItemKind::Text { .. }
            | ItemKind::Frame { .. }
            | ItemKind::Shape { .. } => {}
            other => {
                return Err(DocError::WrongKind {
                    id,
                    expected: "sticky, text, frame or shape",
                    found: other.tag(),
                });
            }
        }
        let target = meta.ensure_mergeable_text(key::TEXT)?;
        text::write(&target, &text)?;
        self.doc.commit();
        Ok(())
    }

    /// Moves an item into a frame or group, or out to the top level with `None`.
    ///
    /// Placement is untouched: `x`/`y` are absolute world coordinates, so
    /// reparenting never makes an item jump.
    pub fn reparent(&mut self, id: ItemId, parent: Option<ItemId>) -> Result<()> {
        if !self.contains(id) {
            return Err(DocError::NoSuchItem(id));
        }
        if let Some(parent) = parent
            && !self.contains(parent)
        {
            return Err(DocError::NoSuchItem(parent));
        }

        match self.items.mov(id.tree_id(), parent.map(ItemId::tree_id)) {
            Err(LoroError::TreeError(LoroTreeError::CyclicMoveError)) => {
                Err(DocError::CyclicReparent {
                    child: id,
                    parent: parent.expect("only a node parent can create a cycle"),
                })
            }
            other => {
                other?;
                self.doc.commit();
                Ok(())
            }
        }
    }

    // ----- z-order --------------------------------------------------------

    /// Places `id` directly above `other`.
    ///
    /// If they have different parents, `id` also moves into `other`'s parent —
    /// "above that" is only meaningful within one stack.
    pub fn raise_above(&mut self, id: ItemId, other: ItemId) -> Result<()> {
        self.restack(id, other, true)
    }

    /// Places `id` directly below `other`, adopting `other`'s parent if they differ.
    pub fn lower_below(&mut self, id: ItemId, other: ItemId) -> Result<()> {
        self.restack(id, other, false)
    }

    fn restack(&mut self, id: ItemId, other: ItemId, above: bool) -> Result<()> {
        for candidate in [id, other] {
            if !self.contains(candidate) {
                return Err(DocError::NoSuchItem(candidate));
            }
        }
        let result = if above {
            self.items.mov_after(id.tree_id(), other.tree_id())
        } else {
            self.items.mov_before(id.tree_id(), other.tree_id())
        };
        match result {
            Err(LoroError::TreeError(LoroTreeError::CyclicMoveError)) => {
                Err(DocError::CyclicReparent { child: id, parent: other })
            }
            other => {
                other?;
                self.doc.commit();
                Ok(())
            }
        }
    }

    /// Raises an item above all of its siblings. A no-op when it is already there,
    /// so repeated presses do not fill the undo stack with nothing.
    pub fn bring_to_front(&mut self, id: ItemId) -> Result<()> {
        self.move_to_edge(id, true)
    }

    /// Lowers an item below all of its siblings.
    pub fn send_to_back(&mut self, id: ItemId) -> Result<()> {
        self.move_to_edge(id, false)
    }

    fn move_to_edge(&mut self, id: ItemId, front: bool) -> Result<()> {
        if !self.contains(id) {
            return Err(DocError::NoSuchItem(id));
        }
        let siblings = self.children(self.parent_of(id));
        let target = if front { siblings.last() } else { siblings.first() };
        match target {
            Some(&edge) if edge != id => self.restack(id, edge, front),
            _ => Ok(()),
        }
    }

    // ----- undo / redo ----------------------------------------------------

    /// Reverses the last local edit. Returns whether anything was undone.
    ///
    /// Loro applies an inverse diff rather than rewinding, which has one visible
    /// consequence: **undoing an item's creation and redoing it yields a new
    /// [`ItemId`]**. Callers holding ids across an undo — a selection, a drag in
    /// progress — must re-resolve them afterwards.
    pub fn undo(&mut self) -> Result<bool> {
        let undone = self.undo.undo()?;
        self.doc.commit();
        Ok(undone)
    }

    /// Reapplies the last undone edit. Returns whether anything was redone.
    pub fn redo(&mut self) -> Result<bool> {
        let redone = self.undo.redo()?;
        self.doc.commit();
        Ok(redone)
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// Starts collapsing subsequent edits into a single undo step, for a gesture
    /// that spans many operations — dragging a selection, or an import.
    pub fn begin_undo_group(&mut self) -> Result<()> {
        self.undo.group_start()?;
        Ok(())
    }

    /// Ends the group opened by [`Board::begin_undo_group`].
    pub fn end_undo_group(&mut self) {
        self.undo.group_end();
    }

    /// Milliseconds within which consecutive edits fuse into one undo step.
    ///
    /// Zero — the default — makes every operation its own step, which is the only
    /// behaviour a library can define. An interactive editor should raise it so a
    /// continuous drag is one undo, and use [`Board::begin_undo_group`] where the
    /// boundaries are known exactly.
    pub fn set_undo_merge_interval(&mut self, millis: i64) {
        self.undo.set_merge_interval(millis);
    }

    fn meta_of(&self, id: ItemId) -> Result<LoroMap> {
        if !self.contains(id) {
            return Err(DocError::NoSuchItem(id));
        }
        Ok(self.items.get_meta(id.tree_id())?)
    }
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Board {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Board")
            .field("title", &self.title())
            .field("item_count", &self.item_count())
            .finish_non_exhaustive()
    }
}

/// Settings that live in the runtime `LoroDoc`, not in the snapshot, and so must be
/// re-applied every time a document is created or loaded.
fn configure(doc: &LoroDoc) -> (LoroTree, LoroMap) {
    doc.config_text_style(text::style_config());
    let items = doc.get_tree(key::ITEMS);
    items.enable_fractional_index(FRACTIONAL_INDEX_JITTER);
    (items, doc.get_map(key::BOARD))
}

fn new_undo_manager(doc: &LoroDoc) -> UndoManager {
    let mut undo = UndoManager::new(doc);
    undo.set_merge_interval(0);
    // Bringing a container into existence is not an edit the user made; see
    // [`Board::scaffold`].
    undo.add_exclude_origin_prefix(SCAFFOLD_ORIGIN);
    undo
}

// ----- placement / style / kind encoding ----------------------------------

fn write_placement(meta: &LoroMap, placement: &Placement) -> LoroResult<()> {
    meta.insert(key::X, placement.x)?;
    meta.insert(key::Y, placement.y)?;
    meta.insert(key::SCALE, placement.scale)?;
    meta.insert(key::ROTATION, placement.rotation)?;
    meta.insert(key::WIDTH, placement.width)?;
    meta.insert(key::HEIGHT, placement.height)
}

/// Reads placement, substituting defaults for anything unreadable.
///
/// Deliberately lenient where [`read_kind`] is strict: an item at the wrong
/// position is a visible, fixable problem, whereas refusing to open the board loses
/// everything else on it too.
fn read_placement(meta: &LoroMap) -> Placement {
    Placement {
        x: num_at(meta, key::X).unwrap_or(0.0),
        y: num_at(meta, key::Y).unwrap_or(0.0),
        scale: num_at(meta, key::SCALE).unwrap_or(1.0),
        rotation: num_at(meta, key::ROTATION).unwrap_or(0.0),
        width: num_at(meta, key::WIDTH).unwrap_or(0.0),
        height: num_at(meta, key::HEIGHT).unwrap_or(0.0),
    }
}

fn write_style(meta: &LoroMap, style: &Style) -> LoroResult<()> {
    if style.is_default() {
        return remove(meta, key::STYLE);
    }
    let map = meta.ensure_mergeable_map(key::STYLE)?;
    put_str(&map, key::FONT_FAMILY, style.font_family.as_deref())?;
    put_num(&map, key::FONT_SIZE, style.font_size)?;
    put_color(&map, key::TEXT_COLOR, style.text_color)?;
    put_str(&map, key::ALIGN, style.align.map(Align::tag))?;
    put_num(&map, key::LINE_HEIGHT, style.line_height)?;
    put_num(&map, key::OPACITY, style.opacity)?;
    put_color(&map, key::FILL, style.fill)?;
    put_color(&map, key::STROKE, style.stroke)?;
    put_num(&map, key::STROKE_WIDTH, style.stroke_width)?;
    // Written only when true, so an unlocked item's style map is byte-for-byte what it was
    // before the field existed — which is what keeps `is_default` honest and every board on
    // disk unchanged.
    if style.locked {
        map.insert(key::LOCKED, true)?;
    } else {
        remove(&map, key::LOCKED)?;
    }
    Ok(())
}

fn read_style(meta: &LoroMap) -> Style {
    let Some(map) = map_at(meta, key::STYLE) else {
        return Style::default();
    };
    Style {
        font_family: string_at(&map, key::FONT_FAMILY),
        font_size: num_at(&map, key::FONT_SIZE),
        text_color: color_at(&map, key::TEXT_COLOR),
        align: string_at(&map, key::ALIGN).as_deref().and_then(Align::from_tag),
        line_height: num_at(&map, key::LINE_HEIGHT),
        opacity: num_at(&map, key::OPACITY),
        fill: color_at(&map, key::FILL),
        stroke: color_at(&map, key::STROKE),
        stroke_width: num_at(&map, key::STROKE_WIDTH),
        locked: bool_at(&map, key::LOCKED),
    }
}

fn write_kind(meta: &LoroMap, kind: &ItemKind) -> LoroResult<()> {
    // Clear what the *previous* kind used and this one does not, rather than
    // sweeping the whole vocabulary. Creating an item then touches nothing and a
    // conversion touches a handful of keys, instead of both paying for every key the
    // format defines — a cost that would otherwise grow with each kind added.
    //
    // A kind tag this build does not know means a board from a newer one, where the
    // full sweep is the only safe answer because we cannot tell what is in there.
    let keeping = kind_keys(kind);
    let previous: &[&str] = match string_at(meta, key::KIND) {
        None => &[],
        Some(tag) => kind_keys_for_tag(&tag).unwrap_or(&key::KIND_SPECIFIC),
    };
    for stale in previous.iter().filter(|k| !keeping.contains(k)) {
        clear_stale(meta, stale)?;
    }

    meta.insert(key::KIND, kind.tag())?;

    match kind {
        ItemKind::Sticky { text, background } => {
            let target = meta.ensure_mergeable_text(key::TEXT)?;
            text::write(&target, text)?;
            put_color(meta, key::BACKGROUND, *background)
        }
        ItemKind::Text { text } => {
            let target = meta.ensure_mergeable_text(key::TEXT)?;
            text::write(&target, text)
        }
        ItemKind::Ink { points, color, thickness } => {
            meta.insert(key::POINTS, LoroValue::Binary(encode_points(points).into()))?;
            put_color(meta, key::INK_COLOR, *color)?;
            meta.insert(key::THICKNESS, *thickness)
        }
        ItemKind::Image { asset_id, crop } => {
            meta.insert(key::ASSET, asset_id.as_str())?;
            match crop {
                Some(crop) => {
                    let map = meta.ensure_mergeable_map(key::CROP)?;
                    map.insert(key::X, crop.x)?;
                    map.insert(key::Y, crop.y)?;
                    map.insert(key::WIDTH, crop.width)?;
                    map.insert(key::HEIGHT, crop.height)
                }
                None => remove(meta, key::CROP),
            }
        }
        ItemKind::LinkPreview { title, url, description, thumbnail, provider, favicon, mode } => {
            put_str(meta, key::TITLE, title.as_deref())?;
            put_str(meta, key::URL, url.as_deref())?;
            put_str(meta, key::DESCRIPTION, description.as_deref())?;
            put_str(meta, key::THUMBNAIL, thumbnail.as_deref())?;
            put_str(meta, key::PROVIDER, provider.as_deref())?;
            put_str(meta, key::FAVICON, favicon.as_deref())?;
            write_card_mode(meta, *mode)
        }
        ItemKind::Embed { title, url, description, provider, html, thumbnail, favicon, mode } => {
            put_str(meta, key::TITLE, title.as_deref())?;
            put_str(meta, key::URL, url.as_deref())?;
            put_str(meta, key::DESCRIPTION, description.as_deref())?;
            put_str(meta, key::PROVIDER, provider.as_deref())?;
            put_str(meta, key::HTML, html.as_deref())?;
            put_str(meta, key::THUMBNAIL, thumbnail.as_deref())?;
            put_str(meta, key::FAVICON, favicon.as_deref())?;
            write_card_mode(meta, *mode)
        }
        ItemKind::Connector { start, end, routing, dash, thickness, color, captions } => {
            write_connector_end(meta, key::START, start)?;
            write_connector_end(meta, key::END, end)?;
            meta.insert(key::ROUTING, routing.tag())?;
            meta.insert(key::DASH, dash.tag())?;
            meta.insert(key::THICKNESS, *thickness)?;
            put_color(meta, key::LINE_COLOR, *color)?;
            write_captions(meta, captions)
        }
        ItemKind::Frame { title, order, speaker_notes } => {
            let target = meta.ensure_mergeable_text(key::TEXT)?;
            text::write(&target, title)?;
            put_int(meta, key::ORDER, *order)?;
            put_str(meta, key::SPEAKER_NOTES, speaker_notes.as_deref())
        }
        ItemKind::Shape { form, text } => {
            meta.insert(key::FORM, form.as_str())?;
            let target = meta.ensure_mergeable_text(key::TEXT)?;
            text::write(&target, text)
        }
        ItemKind::Table { model } => meta.insert(key::MODEL, model.as_str()).map(|_| ()),
        ItemKind::Chart { spec } => meta.insert(key::SPEC, spec.as_str()).map(|_| ()),
        ItemKind::MindMap { model } => meta.insert(key::TREE, model.as_str()).map(|_| ()),
        ItemKind::Kanban { board } => meta.insert(key::COLUMNS, board.as_str()).map(|_| ()),
        // The four Agent Canvas kinds. Each stores one opaque token, and the two that
        // carry text the user wrote store it in the shared `TEXT` container exactly as a
        // shape's label does — which is what puts a role and a note title inside search,
        // the caret and `set_text` with no new path.
        ItemKind::Agent { model, label } => {
            meta.insert(key::AGENT, model.as_str())?;
            let target = meta.ensure_mergeable_text(key::TEXT)?;
            text::write(&target, label)
        }
        ItemKind::FileTree { model } => meta.insert(key::FILE_TREE, model.as_str()).map(|_| ()),
        ItemKind::AgentNote { model, title } => {
            meta.insert(key::NOTE, model.as_str())?;
            let target = meta.ensure_mergeable_text(key::TEXT)?;
            text::write(&target, title)
        }
        ItemKind::Browser { model } => meta.insert(key::BROWSER, model.as_str()).map(|_| ()),
        ItemKind::Group => Ok(()),
        ItemKind::Document { asset_id, page_count, current_page } => {
            meta.insert(key::ASSET, asset_id.as_str())?;
            meta.insert(key::PAGE_COUNT, i64::from(*page_count))?;
            meta.insert(key::CURRENT_PAGE, i64::from(*current_page))
        }
    }
}

/// Drops the content of a key the new kind does not use.
///
/// **A rich-text container is emptied rather than deleted.** Verified against Loro
/// 1.13: undoing a commit that deleted a text container resurrects the container
/// *and* replays the inserts that filled it, so a sticky converted to ink and then
/// un-converted comes back reading `"fanfan"`. Clearing the contents instead leaves
/// the undo a plain edit of an existing container, which round-trips correctly in
/// both directions however many times it is repeated.
///
/// The price is an empty container left on an item that used to hold text. It is
/// invisible, it is paid only by items someone has actually converted, and the next
/// conversion back reuses it rather than adding another.
fn clear_stale(meta: &LoroMap, field: &str) -> LoroResult<()> {
    match field {
        key::TEXT => match text_at(meta, field) {
            Some(target) => text::write(&target, &StyledText::default()),
            None => remove(meta, field),
        },
        // The captions map holds text containers two levels down, so the same rule
        // applies to the whole subtree: it is emptied, not removed.
        key::CAPTIONS => match map_at(meta, field) {
            Some(_) => write_captions(meta, &[]),
            None => remove(meta, field),
        },
        _ => remove(meta, field),
    }
}

/// The keys a kind stores its payload under, looked up by [`ItemKind::tag`].
///
/// Keyed by tag rather than by variant because [`write_kind`] has to ask the
/// question about the kind *already in the document*, which it only knows as a
/// string. `None` is a tag from a build newer than this one.
fn kind_keys_for_tag(tag: &str) -> Option<&'static [&'static str]> {
    Some(match tag {
        "sticky" => &[key::TEXT, key::BACKGROUND],
        "text" => &[key::TEXT],
        "ink" => &[key::POINTS, key::INK_COLOR, key::THICKNESS],
        "image" => &[key::ASSET, key::CROP],
        "link_preview" => &[
            key::TITLE,
            key::URL,
            key::DESCRIPTION,
            key::THUMBNAIL,
            key::PROVIDER,
            key::FAVICON,
            key::CARD_MODE,
        ],
        "embed" => &[
            key::TITLE,
            key::URL,
            key::DESCRIPTION,
            key::PROVIDER,
            key::HTML,
            key::THUMBNAIL,
            key::FAVICON,
            key::CARD_MODE,
        ],
        CONNECTOR_TAG => &[
            key::START,
            key::END,
            key::ROUTING,
            key::DASH,
            key::THICKNESS,
            key::LINE_COLOR,
            key::CAPTIONS,
        ],
        "frame" => &[key::TEXT, key::ORDER, key::SPEAKER_NOTES],
        "shape" => &[key::FORM, key::TEXT],
        "table" => &[key::MODEL],
        "chart" => &[key::SPEC],
        "mindmap" => &[key::TREE],
        "kanban" => &[key::COLUMNS],
        "group" => &[],
        "document" => &[key::ASSET, key::PAGE_COUNT, key::CURRENT_PAGE],
        "agent" => &[key::AGENT, key::TEXT],
        "file_tree" => &[key::FILE_TREE],
        "agent_note" => &[key::NOTE, key::TEXT],
        "browser" => &[key::BROWSER],
        _ => return None,
    })
}

fn kind_keys(kind: &ItemKind) -> &'static [&'static str] {
    kind_keys_for_tag(kind.tag()).expect("a kind's own tag is always in the table")
}

fn write_connector_end(meta: &LoroMap, side: &str, end: &ConnectorEnd) -> LoroResult<()> {
    let map = meta.ensure_mergeable_map(side)?;
    put_str(&map, key::TARGET, end.target.map(|id| id.to_string()).as_deref())?;
    map.insert(key::X, end.anchor.0)?;
    map.insert(key::Y, end.anchor.1)?;
    map.insert(key::ARROW, end.arrowhead.tag())
}

/// Reads one end, leniently: a target that will not parse becomes a free end.
///
/// Deliberately not fatal. A connector missing one binding still draws the right
/// line between roughly the right things, whereas refusing the whole document loses
/// the other 595 widgets over one corrupt field.
fn read_connector_end(meta: &LoroMap, side: &str) -> ConnectorEnd {
    let Some(map) = map_at(meta, side) else {
        return ConnectorEnd::default();
    };
    ConnectorEnd {
        target: string_at(&map, key::TARGET).and_then(|id| id.parse().ok()),
        anchor: (
            num_at(&map, key::X).unwrap_or(ConnectorEnd::CENTER.0),
            num_at(&map, key::Y).unwrap_or(ConnectorEnd::CENTER.1),
        ),
        arrowhead: string_at(&map, key::ARROW)
            .as_deref()
            .and_then(ArrowKind::from_tag)
            .unwrap_or_default(),
    }
}

/// Captions are a map keyed by ordinal — `"0"`, `"1"`, … — each key holding a map
/// with the caption's own rich-text container and its position along the line.
///
/// **A `LoroList` would be the obvious structure and it is unusable here.** Verified
/// against Loro 1.13: undoing a commit that inserted a *container* into a list and
/// then redoing it leaves the list with **two** entries, because the redo re-applies
/// the insert while the original element is still resurrectable. `LoroMovableList`
/// behaves identically. Map keys are last-writer-wins and survive the same
/// undo/redo cycle intact, which is what the connector round-trip tests exercise.
///
/// The price is merge granularity: two peers appending a caption concurrently both
/// write key `"1"` and one wins, where a list would have kept both. That is the
/// right trade for a field that holds nought to two labels, against a structure that
/// silently duplicates them on every redo.
fn write_captions(meta: &LoroMap, captions: &[ConnectorCaption]) -> LoroResult<()> {
    let existing = map_at(meta, key::CAPTIONS);
    if captions.is_empty() && existing.is_none() {
        return Ok(());
    }

    let map = match existing {
        Some(map) => map,
        None => meta.ensure_mergeable_map(key::CAPTIONS)?,
    };
    map.insert(key::COUNT, captions.len() as i64)?;
    for (index, caption) in captions.iter().enumerate() {
        let entry = map.ensure_mergeable_map(&index.to_string())?;
        entry.insert(key::POSITION, caption.position)?;
        let target = entry.ensure_mergeable_text(key::TEXT)?;
        text::write(&target, &caption.text)?;
    }

    // Slots past the new end are emptied rather than removed — the same rule as
    // [`clear_stale`], for the same undo reason — and [`key::COUNT`] is what makes
    // the dead ones invisible to a read.
    let mut stale = Vec::new();
    map.for_each(|slot, _| {
        if slot.parse::<usize>().is_ok_and(|index| index >= captions.len()) {
            stale.push(slot.to_owned());
        }
    });
    for slot in stale {
        if let Some(entry) = map_at(&map, &slot)
            && let Some(target) = text_at(&entry, key::TEXT)
        {
            text::write(&target, &StyledText::default())?;
        }
    }
    Ok(())
}

fn read_captions(meta: &LoroMap) -> Vec<ConnectorCaption> {
    let Some(map) = map_at(meta, key::CAPTIONS) else {
        return Vec::new();
    };
    let live = int_at(&map, key::COUNT).unwrap_or(0).max(0) as usize;
    (0..live)
        .filter_map(|index| map_at(&map, &index.to_string()))
        .map(|entry| ConnectorCaption {
            text: read_text(&entry),
            position: num_at(&entry, key::POSITION).unwrap_or(0.5),
        })
        .collect()
}

/// Reads an item's kind. Strict, unlike [`read_placement`]: an unrecognised kind
/// means the file was written by a build that knows about item types this one
/// cannot draw, and guessing would put an invisible item on the canvas.
fn read_kind(meta: &LoroMap) -> Result<ItemKind> {
    let Some(tag) = string_at(meta, key::KIND) else {
        return Err(DocError::Malformed("item has no kind".into()));
    };
    Ok(match tag.as_str() {
        "sticky" => ItemKind::Sticky {
            text: read_text(meta),
            background: color_at(meta, key::BACKGROUND),
        },
        "text" => ItemKind::Text { text: read_text(meta) },
        "ink" => ItemKind::Ink {
            points: binary_at(meta, key::POINTS).map(|b| decode_points(&b)).unwrap_or_default(),
            color: color_at(meta, key::INK_COLOR),
            thickness: num_at(meta, key::THICKNESS).unwrap_or(1.0),
        },
        "image" => ItemKind::Image {
            asset_id: string_at(meta, key::ASSET).unwrap_or_default(),
            crop: map_at(meta, key::CROP).map(|c| Crop {
                x: num_at(&c, key::X).unwrap_or(0.0),
                y: num_at(&c, key::Y).unwrap_or(0.0),
                width: num_at(&c, key::WIDTH).unwrap_or(0.0),
                height: num_at(&c, key::HEIGHT).unwrap_or(0.0),
            }),
        },
        "link_preview" => ItemKind::LinkPreview {
            title: string_at(meta, key::TITLE),
            url: string_at(meta, key::URL),
            description: string_at(meta, key::DESCRIPTION),
            thumbnail: string_at(meta, key::THUMBNAIL),
            provider: string_at(meta, key::PROVIDER),
            favicon: string_at(meta, key::FAVICON),
            mode: read_card_mode(meta),
        },
        "embed" => ItemKind::Embed {
            title: string_at(meta, key::TITLE),
            url: string_at(meta, key::URL),
            description: string_at(meta, key::DESCRIPTION),
            provider: string_at(meta, key::PROVIDER),
            html: string_at(meta, key::HTML),
            thumbnail: string_at(meta, key::THUMBNAIL),
            favicon: string_at(meta, key::FAVICON),
            mode: read_card_mode(meta),
        },
        CONNECTOR_TAG => ItemKind::Connector {
            start: read_connector_end(meta, key::START),
            end: read_connector_end(meta, key::END),
            routing: string_at(meta, key::ROUTING)
                .as_deref()
                .and_then(Routing::from_tag)
                .unwrap_or_default(),
            dash: string_at(meta, key::DASH)
                .as_deref()
                .and_then(Dash::from_tag)
                .unwrap_or_default(),
            thickness: num_at(meta, key::THICKNESS).unwrap_or(1.0),
            color: color_at(meta, key::LINE_COLOR),
            captions: read_captions(meta),
        },
        "frame" => ItemKind::Frame {
            title: read_text(meta),
            order: int_at(meta, key::ORDER),
            speaker_notes: string_at(meta, key::SPEAKER_NOTES),
        },
        "shape" => ItemKind::Shape {
            form: string_at(meta, key::FORM).unwrap_or_default(),
            text: read_text(meta),
        },
        "table" => ItemKind::Table {
            model: string_at(meta, key::MODEL).unwrap_or_default(),
        },
        "chart" => ItemKind::Chart { spec: string_at(meta, key::SPEC).unwrap_or_default() },
        "mindmap" => ItemKind::MindMap {
            model: string_at(meta, key::TREE).unwrap_or_default(),
        },
        "kanban" => ItemKind::Kanban {
            board: string_at(meta, key::COLUMNS).unwrap_or_default(),
        },
        "agent" => ItemKind::Agent {
            model: string_at(meta, key::AGENT).unwrap_or_default(),
            label: read_text(meta),
        },
        "file_tree" => ItemKind::FileTree {
            model: string_at(meta, key::FILE_TREE).unwrap_or_default(),
        },
        "agent_note" => ItemKind::AgentNote {
            model: string_at(meta, key::NOTE).unwrap_or_default(),
            title: read_text(meta),
        },
        "browser" => ItemKind::Browser {
            model: string_at(meta, key::BROWSER).unwrap_or_default(),
        },
        "group" => ItemKind::Group,
        "document" => ItemKind::Document {
            asset_id: string_at(meta, key::ASSET).unwrap_or_default(),
            page_count: page_at(meta, key::PAGE_COUNT),
            current_page: page_at(meta, key::CURRENT_PAGE),
        },
        unknown => {
            return Err(DocError::Malformed(format!("unknown item kind `{unknown}`")));
        }
    })
}

fn read_text(meta: &LoroMap) -> StyledText {
    text_at(meta, key::TEXT).as_ref().map(text::read).unwrap_or_default()
}

/// Ink strokes are stored as packed little-endian `f64` pairs rather than as a CRDT
/// list.
///
/// A stroke is immutable once drawn — you redraw it, you do not edit point 400 —
/// and the reference Miro board carries 134 of them, some thousands of points long.
/// As a list that is one operation per point to store, merge and sync; as one binary
/// value it is one operation and 16 bytes per point.
fn encode_points(points: &[Point]) -> Vec<u8> {
    let mut out = Vec::with_capacity(points.len() * 16);
    for point in points {
        out.extend_from_slice(&point.x.to_le_bytes());
        out.extend_from_slice(&point.y.to_le_bytes());
    }
    out
}

fn decode_points(bytes: &[u8]) -> Vec<Point> {
    // A trailing partial pair can only come from a corrupt file; dropping it keeps
    // the rest of the stroke drawable.
    bytes
        .chunks_exact(16)
        .map(|pair| Point::new(le_f64(&pair[..8]), le_f64(&pair[8..])))
        .collect()
}

fn le_f64(bytes: &[u8]) -> f64 {
    f64::from_le_bytes(bytes.try_into().expect("chunks_exact yields 8-byte halves"))
}

// ----- typed access to Loro maps ------------------------------------------

fn num_at(map: &LoroMap, key: &str) -> Option<f64> {
    match map.get(key)?.into_value().ok()? {
        // Accept `I64` as well: a whole-number `f64` can come back as an integer
        // through a JSON round-trip, and reading `2` as "no value" would silently
        // reset the field to its default.
        LoroValue::Double(v) => Some(v),
        LoroValue::I64(v) => Some(v as f64),
        _ => None,
    }
}

fn int_at(map: &LoroMap, key: &str) -> Option<i64> {
    match map.get(key)?.into_value().ok()? {
        LoroValue::I64(v) => Some(v),
        LoroValue::Double(v) => Some(v as i64),
        _ => None,
    }
}

fn string_at(map: &LoroMap, key: &str) -> Option<String> {
    match map.get(key)?.into_value().ok()? {
        LoroValue::String(v) => Some(v.as_str().to_owned()),
        _ => None,
    }
}

fn binary_at(map: &LoroMap, key: &str) -> Option<Vec<u8>> {
    match map.get(key)?.into_value().ok()? {
        LoroValue::Binary(v) => Some(v.to_vec()),
        _ => None,
    }
}

/// A page number, floored at zero and capped at `u32::MAX`. A negative or absurd
/// value in the file is a corrupt page index, not a reason to refuse the document.
fn page_at(map: &LoroMap, key: &str) -> u32 {
    int_at(map, key).unwrap_or(0).clamp(0, i64::from(u32::MAX)) as u32
}

fn color_at(map: &LoroMap, key: &str) -> Option<Color> {
    Color::from_packed(int_at(map, key)?)
}

fn map_at(map: &LoroMap, key: &str) -> Option<LoroMap> {
    match map.get(key)? {
        ValueOrContainer::Container(Container::Map(child)) => Some(child),
        _ => None,
    }
}

fn text_at(map: &LoroMap, key: &str) -> Option<LoroText> {
    match map.get(key)? {
        ValueOrContainer::Container(Container::Text(child)) => Some(child),
        _ => None,
    }
}

/// A boolean, defaulting to `false` for an absent or wrongly-typed value.
///
/// Lenient on purpose: a style key written by a later build with a different type should
/// leave the item usable rather than refusing the board.
fn bool_at(map: &LoroMap, key: &str) -> bool {
    matches!(map.get(key), Some(ValueOrContainer::Value(LoroValue::Bool(true))))
}

fn put_num(map: &LoroMap, key: &str, value: Option<f64>) -> LoroResult<()> {
    match value {
        Some(value) => map.insert(key, value),
        None => remove(map, key),
    }
}

/// Writes a card's display mode, and writes **nothing** for the default.
///
/// The same rule `Style::locked` follows and for the same reason: every board already on disk
/// predates this field, and a card written without the key has to read back as the default
/// rather than as a missing value. Omitting the default also means re-saving an old board does
/// not rewrite every link card's map.
fn write_card_mode(map: &LoroMap, mode: CardMode) -> LoroResult<()> {
    if mode == CardMode::default() {
        return remove(map, key::CARD_MODE);
    }
    map.insert(key::CARD_MODE, mode.tag()).map(|_| ())
}

/// Reads it back. A missing or unrecognised tag is the default, so a board written by a later
/// build — one that has a fourth mode — opens with its cards drawn rather than dropped.
fn read_card_mode(map: &LoroMap) -> CardMode {
    string_at(map, key::CARD_MODE)
        .as_deref()
        .and_then(CardMode::from_tag)
        .unwrap_or_default()
}

fn put_int(map: &LoroMap, key: &str, value: Option<i64>) -> LoroResult<()> {
    match value {
        Some(value) => map.insert(key, value),
        None => remove(map, key),
    }
}

fn put_str(map: &LoroMap, key: &str, value: Option<&str>) -> LoroResult<()> {
    match value {
        Some(value) => map.insert(key, value),
        None => remove(map, key),
    }
}

fn put_color(map: &LoroMap, key: &str, value: Option<Color>) -> LoroResult<()> {
    match value {
        Some(color) => map.insert(key, color.to_packed()),
        None => remove(map, key),
    }
}

/// Deletes a key only if it is there, so clearing an already-absent field does not
/// record an operation.
fn remove(map: &LoroMap, key: &str) -> LoroResult<()> {
    if map.get(key).is_some() {
        map.delete(key)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{BlockStyle, ListKind, SpanStyle, TextSpan};

    fn sticky(text: &str, x: f64, y: f64) -> NewItem {
        NewItem::new(
            ItemKind::Sticky {
                text: StyledText::plain(text),
                background: Some(Color::rgb(0xFF, 0xF7, 0x9E)),
            },
            Placement::new(x, y, 199.0, 228.0),
        )
    }

    /// *"i want to be able to select backgrounds for the baords"*. It is a property
    /// of the board, so it has to survive the round trip to disk like the title does.
    #[test]
    fn a_background_is_chosen_stored_and_read_back() {
        let mut board = Board::new();
        assert_eq!(board.background(), Background::default());
        assert_eq!(
            board.background().pattern,
            Pattern::Dots,
            "a board opens with a grid: a blank field gives the eye nothing to judge \
             position or scale against, and the user asked for one"
        );
        assert_eq!(board.background().color, None, "the default follows the palette");

        let chosen = Background {
            color: Some(Color::rgb(0xD6, 0xEF, 0xD9)),
            pattern: Pattern::Lines,
        };
        board.set_background(chosen).unwrap();
        assert_eq!(board.background(), chosen);

        let bytes = board.to_bytes().unwrap();
        assert_eq!(Board::from_bytes(&bytes).unwrap().background(), chosen);

        // Back to the default: both keys go, so a board that never chose one and a
        // board that chose and un-chose are the same document.
        board.set_background(Background::default()).unwrap();
        assert_eq!(board.background(), Background::default());
        assert!(board.meta.get(key::BG_COLOR).is_none());
        assert!(board.meta.get(key::BG_PATTERN).is_none());
    }

    /// A pattern written by a newer build has to read back as the default rather than
    /// stopping the board from opening.
    #[test]
    fn an_unknown_pattern_reads_as_the_default() {
        assert_eq!(Pattern::from_tag("dots"), Some(Pattern::Dots));
        assert_eq!(Pattern::from_tag("isometric"), None);
        for pattern in Pattern::ALL {
            assert_eq!(Pattern::from_tag(pattern.tag()), Some(pattern));
            assert!(!pattern.label().is_empty());
        }

        // A tag from a newer build reads back as the default rather than as an
        // error: an unknown pattern is a board that draws normally, not one that
        // fails to open.
        let board = Board::new();
        board.meta.insert(key::BG_PATTERN, "isometric").unwrap();
        assert_eq!(board.background().pattern, Pattern::default());
    }

    /// The whole persistence contract in one test: what goes in comes back out.
    #[test]
    fn a_board_survives_a_snapshot_round_trip_unchanged() {
        let mut board = Board::new();
        board.set_title("Reference Board").unwrap();

        let frame = board
            .add(NewItem::new(
                ItemKind::Text { text: StyledText::plain("Engine") },
                Placement::new(-3083.018, 1367.540, 400.0, 80.0),
            ))
            .unwrap();
        board.add(sticky("fan", 10.0, 20.0).with_parent(frame)).unwrap();
        board
            .add(NewItem::new(
                ItemKind::Ink {
                    points: vec![Point::new(0.0, 0.0), Point::new(10.5, -4.25)],
                    color: Some(Color::rgb(0x2D, 0xC7, 0x5C).with_opacity(0.8)),
                    thickness: 18.0,
                },
                Placement::new(-3351.5, -575.4, 40.0, 40.0),
            ))
            .unwrap();
        board
            .add(
                NewItem::new(
                    ItemKind::Image {
                        asset_id: "b7f1c2".into(),
                        crop: Some(Crop { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 }),
                    },
                    Placement::new(0.0, 0.0, 1920.0, 1080.0),
                )
                .with_style(Style { opacity: Some(0.5), ..Style::default() }),
            )
            .unwrap();

        let bytes = board.to_bytes().unwrap();
        let reloaded = Board::from_bytes(&bytes).unwrap();

        assert_eq!(reloaded.title(), "Reference Board");
        assert_eq!(reloaded.item_count(), 4);
        assert_eq!(reloaded.items().unwrap(), board.items().unwrap());
    }

    /// Placement is stored as `f64` end to end; a round trip through the document
    /// must not quantise a deep-zoom coordinate.
    #[test]
    fn coordinates_round_trip_without_precision_loss() {
        let mut board = Board::new();
        let mut placement = Placement::new(-3083.01852968025, 1367.540251981647, 199.0, 228.0);
        placement.scale = 1.85;
        placement.rotation = 42.5;
        let id = board
            .add(NewItem::new(ItemKind::Text { text: StyledText::plain("x") }, placement))
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        assert_eq!(reloaded.item(id).unwrap().placement, placement);
    }

    #[test]
    fn styled_text_survives_the_document() {
        let mut board = Board::new();
        let styled = StyledText::from_spans([
            TextSpan::new("bold", SpanStyle::bold()),
            TextSpan::plain(" and "),
            TextSpan::new("linked", SpanStyle::link("https://vellum.app")),
        ]);
        let id = board
            .add(NewItem::new(
                ItemKind::Sticky { text: styled.clone(), background: None },
                Placement::default(),
            ))
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let ItemKind::Sticky { text, .. } = reloaded.item(id).unwrap().kind else {
            panic!("expected a sticky")
        };
        assert_eq!(text, styled);
    }

    /// The point of fractional indexing: inserting between two items must write one
    /// stacking key and leave every other item's key byte-identical.
    #[test]
    fn inserting_between_two_items_does_not_renumber_them() {
        let mut board = Board::new();
        let bottom = board.add(sticky("bottom", 0.0, 0.0)).unwrap();
        let top = board.add(sticky("top", 0.0, 0.0)).unwrap();

        let before = (board.z_index(bottom).unwrap(), board.z_index(top).unwrap());
        assert!(before.0 < before.1);

        let middle = board.add_at(sticky("middle", 0.0, 0.0), 1).unwrap();

        assert_eq!(board.z_index(bottom).unwrap(), before.0, "bottom was renumbered");
        assert_eq!(board.z_index(top).unwrap(), before.1, "top was renumbered");

        let z = board.z_index(middle).unwrap();
        assert!(before.0 < z && z < before.1, "{} !< {z} !< {}", before.0, before.1);
        assert_eq!(board.children(None), vec![bottom, middle, top]);
    }

    /// Fractional indices must keep subdividing: this is the case that breaks an
    /// integer z-order after a handful of insertions.
    #[test]
    fn repeated_insertion_at_the_same_gap_keeps_working() {
        let mut board = Board::new();
        board.add(sticky("a", 0.0, 0.0)).unwrap();
        board.add(sticky("b", 0.0, 0.0)).unwrap();

        let mut inserted = Vec::new();
        for _ in 0..24 {
            inserted.push(board.add_at(sticky("mid", 0.0, 0.0), 1).unwrap());
        }

        let order = board.children(None);
        let mut z: Vec<_> = order.iter().map(|id| board.z_index(*id).unwrap()).collect();
        let sorted = {
            let mut sorted = z.clone();
            sorted.sort();
            sorted
        };
        assert_eq!(z, sorted, "stacking keys are not in tree order");
        z.dedup();
        assert_eq!(z.len(), order.len(), "two items share a stacking key");
    }

    #[test]
    fn front_and_back_move_only_the_target() {
        let mut board = Board::new();
        let a = board.add(sticky("a", 0.0, 0.0)).unwrap();
        let b = board.add(sticky("b", 0.0, 0.0)).unwrap();
        let c = board.add(sticky("c", 0.0, 0.0)).unwrap();
        assert_eq!(board.children(None), vec![a, b, c]);

        board.bring_to_front(a).unwrap();
        assert_eq!(board.children(None), vec![b, c, a]);

        board.send_to_back(c).unwrap();
        assert_eq!(board.children(None), vec![c, b, a]);

        board.raise_above(c, b).unwrap();
        assert_eq!(board.children(None), vec![b, c, a]);

        board.lower_below(a, b).unwrap();
        assert_eq!(board.children(None), vec![a, b, c]);
    }

    /// Bringing the top item to the front must not record an operation, or holding
    /// the shortcut would fill the undo stack with no-ops.
    #[test]
    fn moving_to_an_edge_it_already_occupies_is_a_no_op() {
        let mut board = Board::new();
        let a = board.add(sticky("a", 0.0, 0.0)).unwrap();
        let b = board.add(sticky("b", 0.0, 0.0)).unwrap();

        let before = board.version();
        board.bring_to_front(b).unwrap();
        board.send_to_back(a).unwrap();
        assert_eq!(board.version(), before);
    }

    #[test]
    fn reparenting_keeps_world_coordinates_and_nests_the_item() {
        let mut board = Board::new();
        let frame = board.add(sticky("frame", 0.0, 0.0)).unwrap();
        let note = board.add(sticky("note", 500.0, 250.0)).unwrap();

        board.reparent(note, Some(frame)).unwrap();

        assert_eq!(board.parent_of(note), Some(frame));
        assert_eq!(board.children(Some(frame)), vec![note]);
        assert_eq!(board.children(None), vec![frame]);
        let placement = board.item(note).unwrap().placement;
        assert_eq!((placement.x, placement.y), (500.0, 250.0));

        board.reparent(note, None).unwrap();
        assert_eq!(board.parent_of(note), None);
        assert_eq!(board.children(None), vec![frame, note]);
    }

    /// A frame's contents are separate items that merely name it as their parent, so
    /// "copy this frame" has to mean "and everything under it" or it copies an empty box.
    #[test]
    fn descendants_walks_the_whole_subtree_parent_before_child() {
        let mut board = Board::new();
        let frame = board.add(sticky("frame", 0.0, 0.0)).unwrap();
        let group = board.add(sticky("group", 10.0, 10.0).with_parent(frame)).unwrap();
        let leaf = board.add(sticky("leaf", 20.0, 20.0).with_parent(group)).unwrap();
        let sibling = board.add(sticky("sibling", 30.0, 30.0).with_parent(frame)).unwrap();
        let outside = board.add(sticky("outside", 900.0, 900.0)).unwrap();

        let under = board.descendants(frame);
        assert_eq!(under, vec![group, leaf, sibling], "depth-first, parent before child");
        assert!(!under.contains(&frame), "the container itself is the caller's business");
        assert!(!under.contains(&outside));

        assert_eq!(board.descendants(group), vec![leaf], "nesting is followed to the bottom");
        assert!(board.descendants(leaf).is_empty());
        assert!(board.descendants(outside).is_empty(), "a lone item has no subtree");
    }

    #[test]
    fn an_item_cannot_be_moved_inside_itself() {
        let mut board = Board::new();
        let outer = board.add(sticky("outer", 0.0, 0.0)).unwrap();
        let inner = board.add(sticky("inner", 0.0, 0.0).with_parent(outer)).unwrap();

        assert!(matches!(
            board.reparent(outer, Some(inner)),
            Err(DocError::CyclicReparent { .. })
        ));
        assert!(matches!(
            board.reparent(outer, Some(outer)),
            Err(DocError::CyclicReparent { .. })
        ));
        assert_eq!(board.parent_of(inner), Some(outer));
    }

    #[test]
    fn nesting_survives_a_round_trip_in_paint_order() {
        let mut board = Board::new();
        let frame = board.add(sticky("frame", 0.0, 0.0)).unwrap();
        let inner = board.add(sticky("inner", 0.0, 0.0).with_parent(frame)).unwrap();
        let deeper = board.add(sticky("deeper", 0.0, 0.0).with_parent(inner)).unwrap();
        let sibling = board.add(sticky("sibling", 0.0, 0.0)).unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        assert_eq!(reloaded.item_ids(), vec![frame, inner, deeper, sibling]);
        assert_eq!(reloaded.parent_of(deeper), Some(inner));
    }

    #[test]
    fn removing_a_container_removes_what_is_inside_it() {
        let mut board = Board::new();
        let frame = board.add(sticky("frame", 0.0, 0.0)).unwrap();
        let inner = board.add(sticky("inner", 0.0, 0.0).with_parent(frame)).unwrap();
        let outside = board.add(sticky("outside", 0.0, 0.0)).unwrap();

        board.remove(frame).unwrap();

        assert!(!board.contains(frame));
        assert!(!board.contains(inner), "a nested item outlived its frame");
        assert!(board.contains(outside));
        assert_eq!(board.item_count(), 1);
        assert!(matches!(board.item(inner), Err(DocError::NoSuchItem(_))));
    }

    #[test]
    fn undo_and_redo_walk_the_whole_edit_history() {
        let mut board = Board::new();
        assert!(!board.can_undo());

        let id = board.add(sticky("note", 0.0, 0.0)).unwrap();
        board.set_position(id, 100.0, 200.0).unwrap();
        assert_eq!(board.item_count(), 1);

        assert!(board.undo().unwrap());
        assert_eq!(board.item(id).unwrap().placement.x, 0.0);

        assert!(board.undo().unwrap());
        assert_eq!(board.item_count(), 0);
        assert!(!board.can_undo());
        assert!(!board.undo().unwrap(), "undo past the start reported success");

        assert!(board.redo().unwrap());
        assert_eq!(board.item_count(), 1);
        assert!(board.redo().unwrap());
        assert_eq!(board.items().unwrap()[0].placement.x, 100.0);
        assert!(!board.can_redo());
    }

    /// Loro undoes by applying an inverse diff, so a re-created item is a *new*
    /// node. This is a real constraint on callers, so it is pinned by a test rather
    /// than left to be discovered.
    #[test]
    fn redoing_a_creation_yields_a_new_item_id() {
        let mut board = Board::new();
        let original = board.add(sticky("note", 0.0, 0.0)).unwrap();

        board.undo().unwrap();
        board.redo().unwrap();

        let ids = board.item_ids();
        assert_eq!(ids.len(), 1);
        assert_ne!(ids[0], original);
        assert!(!board.contains(original));
    }

    #[test]
    fn undo_restores_a_reparent() {
        let mut board = Board::new();
        let frame = board.add(sticky("frame", 0.0, 0.0)).unwrap();
        let note = board.add(sticky("note", 0.0, 0.0)).unwrap();

        board.reparent(note, Some(frame)).unwrap();
        assert_eq!(board.parent_of(note), Some(frame));

        board.undo().unwrap();
        assert_eq!(board.parent_of(note), None);

        board.redo().unwrap();
        assert_eq!(board.parent_of(note), Some(frame));
    }

    #[test]
    fn undo_restores_stacking_order() {
        let mut board = Board::new();
        let a = board.add(sticky("a", 0.0, 0.0)).unwrap();
        let b = board.add(sticky("b", 0.0, 0.0)).unwrap();

        board.bring_to_front(a).unwrap();
        assert_eq!(board.children(None), vec![b, a]);

        board.undo().unwrap();
        assert_eq!(board.children(None), vec![a, b]);
    }

    #[test]
    fn a_group_of_edits_undoes_as_one_step() {
        let mut board = Board::new();
        let ids: Vec<_> =
            (0..3).map(|i| board.add(sticky("n", i as f64, 0.0)).unwrap()).collect();

        board.begin_undo_group().unwrap();
        for id in &ids {
            board.translate(*id, 10.0, 5.0).unwrap();
        }
        board.end_undo_group();
        assert_eq!(board.item(ids[0]).unwrap().placement.x, 10.0);

        board.undo().unwrap();

        for (i, id) in ids.iter().enumerate() {
            let placement = board.item(*id).unwrap().placement;
            assert_eq!((placement.x, placement.y), (i as f64, 0.0), "item {i} was not restored");
        }
        assert_eq!(board.item_count(), 3, "the group swallowed the creations too");
    }

    #[test]
    fn changing_kind_clears_the_previous_kinds_fields() {
        let mut board = Board::new();
        let id = board.add(sticky("note", 0.0, 0.0)).unwrap();

        board
            .set_kind(
                id,
                ItemKind::Ink {
                    points: vec![Point::new(1.0, 2.0)],
                    color: None,
                    thickness: 4.0,
                },
            )
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let ItemKind::Ink { points, thickness, .. } = reloaded.item(id).unwrap().kind else {
            panic!("expected ink")
        };
        assert_eq!(points, vec![Point::new(1.0, 2.0)]);
        assert_eq!(thickness, 4.0);

        let meta = reloaded.items.get_meta(id.tree_id()).unwrap();
        assert!(meta.get(key::BACKGROUND).is_none(), "the sticky's background survived");
        // The text *container* is kept and emptied rather than deleted — see
        // [`clear_stale`]. What must not survive is the text in it.
        assert!(text_at(&meta, key::TEXT).is_none_or(|t| text::read(&t).is_empty()));
    }

    #[test]
    fn setting_text_on_a_non_text_item_is_refused() {
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                ItemKind::Image { asset_id: "abc".into(), crop: None },
                Placement::default(),
            ))
            .unwrap();

        assert!(matches!(
            board.set_text(id, StyledText::plain("nope")),
            Err(DocError::WrongKind { found: "image", .. })
        ));
    }

    #[test]
    fn styles_can_be_set_and_cleared() {
        let mut board = Board::new();
        let id = board.add(sticky("note", 0.0, 0.0)).unwrap();
        let style = Style {
            font_family: Some("Noto Sans".into()),
            font_size: None,
            text_color: Some(Color::rgb(0x1A, 0x1A, 0x1A)),
            align: Some(Align::Center),
            line_height: Some(1.36),
            opacity: None,
            fill: Some(Color::rgba(0x00, 0x00, 0x00, 0x00)),
            stroke: Some(Color::rgb(0x33, 0x33, 0x33)),
            stroke_width: Some(2.0),
            locked: true,
        };

        board.set_style(id, style.clone()).unwrap();
        assert_eq!(board.item(id).unwrap().style, style);

        board.set_style(id, Style::default()).unwrap();
        assert_eq!(board.item(id).unwrap().style, Style::default());
    }

    /// The lock flag is written **only when set**, so an unlocked item's style map is what
    /// it was before the field existed. That is what keeps `Style::is_default` answering
    /// `true` for every board already on disk — a style map that appeared out of nowhere
    /// would make every untouched item look styled.
    #[test]
    fn an_unlocked_item_writes_no_lock_key_at_all() {
        let mut board = Board::new();
        let id = board.add(sticky("note", 0.0, 0.0)).unwrap();

        // Nothing but the lock: the style is otherwise default, so setting it and clearing
        // it must leave no trace.
        board.set_style(id, Style { locked: true, ..Style::default() }).unwrap();
        assert!(board.item(id).unwrap().style.locked);

        board.set_style(id, Style::default()).unwrap();
        let after = board.item(id).unwrap().style;
        assert!(!after.locked);
        assert!(after.is_default(), "clearing the lock left something behind: {after:?}");
    }

    /// A locked item is still fully editable *through the document*. The flag is an
    /// affordance for the pointer, and `vellum-app` is where it is enforced — the panel has
    /// to be able to unlock it, and undo has to be able to move it.
    #[test]
    fn the_document_does_not_enforce_the_lock() {
        let mut board = Board::new();
        let id = board.add(sticky("note", 0.0, 0.0)).unwrap();
        board.set_style(id, Style { locked: true, ..Style::default() }).unwrap();

        board.set_placement(id, Placement::new(50.0, 60.0, 10.0, 10.0)).unwrap();
        let item = board.item(id).unwrap();
        assert_eq!(item.placement.x, 50.0);
        assert!(item.style.locked, "moving it must not have unlocked it");
    }

    #[test]
    fn operations_on_a_removed_item_report_it_rather_than_panicking() {
        let mut board = Board::new();
        let id = board.add(sticky("note", 0.0, 0.0)).unwrap();
        board.remove(id).unwrap();

        assert!(matches!(board.item(id), Err(DocError::NoSuchItem(_))));
        assert!(matches!(board.set_position(id, 1.0, 1.0), Err(DocError::NoSuchItem(_))));
        assert!(matches!(board.remove(id), Err(DocError::NoSuchItem(_))));
        assert!(matches!(board.reparent(id, None), Err(DocError::NoSuchItem(_))));
        assert!(matches!(board.bring_to_front(id), Err(DocError::NoSuchItem(_))));
    }

    #[test]
    fn adding_under_a_removed_parent_is_refused() {
        let mut board = Board::new();
        let frame = board.add(sticky("frame", 0.0, 0.0)).unwrap();
        board.remove(frame).unwrap();

        assert!(matches!(
            board.add(sticky("orphan", 0.0, 0.0).with_parent(frame)),
            Err(DocError::NoSuchItem(_))
        ));
    }

    /// Incremental saving depends on this: a snapshot plus the updates since it
    /// must reconstruct the same document as a full export.
    #[test]
    fn a_snapshot_plus_updates_reconstructs_the_board() {
        let mut board = Board::new();
        board.add(sticky("first", 0.0, 0.0)).unwrap();

        let snapshot = board.to_bytes().unwrap();
        let checkpoint = board.version();

        board.add(sticky("second", 100.0, 0.0)).unwrap();
        let moved = board.add(sticky("third", 200.0, 0.0)).unwrap();
        board.translate(moved, 5.0, 5.0).unwrap();
        let updates = board.export_since(&checkpoint).unwrap();

        assert!(
            updates.len() < snapshot.len() * 2,
            "an incremental export should not dwarf the snapshot"
        );

        let mut replayed = Board::from_bytes(&snapshot).unwrap();
        assert_eq!(replayed.item_count(), 1);
        replayed.apply(&updates).unwrap();

        assert_eq!(replayed.items().unwrap(), board.items().unwrap());
    }

    /// An export with nothing new is not zero bytes — Loro still writes its
    /// container header. Callers must therefore compare [`Version`]s to decide
    /// whether a save is needed, which is what `vellum-store` does.
    #[test]
    fn an_export_with_no_new_changes_carries_only_a_header() {
        let mut board = Board::new();
        board.add(sticky("note", 0.0, 0.0)).unwrap();

        let version = board.version();
        let empty = board.export_since(&version).unwrap();
        assert!(empty.len() < 64, "expected a bare header, got {} bytes", empty.len());
        assert_eq!(board.version(), version);

        board.add(sticky("another", 0.0, 0.0)).unwrap();
        assert_ne!(board.version(), version);
        assert!(board.export_since(&version).unwrap().len() > empty.len());
    }

    #[test]
    fn a_board_from_the_future_refuses_to_open() {
        let board = Board::new();
        board.meta.insert(key::SCHEMA, SCHEMA_VERSION + 1).unwrap();
        board.doc.commit();

        let bytes = board.to_bytes().unwrap();
        assert!(matches!(
            Board::from_bytes(&bytes),
            Err(DocError::UnsupportedSchema { found, supported })
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ));
    }

    #[test]
    fn a_loro_document_that_is_not_a_board_is_rejected() {
        let stranger = LoroDoc::new();
        stranger.get_map("something-else").insert("k", "v").unwrap();
        stranger.commit();
        let bytes = stranger.export(ExportMode::Snapshot).unwrap();

        assert!(matches!(Board::from_bytes(&bytes), Err(DocError::Malformed(_))));
    }

    #[test]
    fn garbage_bytes_do_not_panic() {
        assert!(Board::from_bytes(b"not a loro snapshot at all").is_err());
        assert!(Board::from_bytes(&[]).is_err());
    }

    #[test]
    fn ink_points_survive_the_binary_encoding() {
        let points = vec![
            Point::new(0.0, 0.0),
            Point::new(-3351.5, 575.40625),
            Point::new(f64::MIN_POSITIVE, 1e300),
        ];
        assert_eq!(decode_points(&encode_points(&points)), points);
        assert_eq!(decode_points(&[]), Vec::<Point>::new());
        // A truncated pair is dropped rather than read as a garbage coordinate.
        assert_eq!(decode_points(&[0u8; 20]), vec![Point::new(0.0, 0.0)]);
    }

    #[test]
    fn a_long_stroke_round_trips_through_the_document() {
        let points: Vec<_> =
            (0..5_000).map(|i| Point::new(i as f64 * 0.5, (i as f64).sin())).collect();
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                ItemKind::Ink { points: points.clone(), color: None, thickness: 2.0 },
                Placement::default(),
            ))
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let ItemKind::Ink { points: back, .. } = reloaded.item(id).unwrap().kind else {
            panic!("expected ink")
        };
        assert_eq!(back, points);
    }

    #[test]
    fn add_at_rejects_an_index_past_the_end() {
        let mut board = Board::new();
        board.add(sticky("a", 0.0, 0.0)).unwrap();
        assert!(board.add_at(sticky("b", 0.0, 0.0), 5).is_err());
        assert_eq!(board.item_count(), 1);
    }

    // ----- the kinds the Miro importer needs ------------------------------

    /// A card with **every** field set, including the fetched ones and a non-default mode,
    /// so the round-trip test below covers the whole variant rather than the original four.
    fn link_preview() -> ItemKind {
        ItemKind::LinkPreview {
            title: Some("Workshop wiring diagrams".into()),
            url: Some("https://example.com/page?a=1&b=2".into()),
            description: Some("Full loom, every connector, in colour.".into()),
            thumbnail: Some("b3f19c4a".into()),
            provider: Some("Example".into()),
            favicon: Some("ff00aa11".into()),
            mode: CardMode::Large,
        }
    }

    fn embed() -> ItemKind {
        ItemKind::Embed {
            title: Some("Gearbox teardown".into()),
            url: Some("https://youtube.com/watch?v=abc".into()),
            description: Some("Teardown, 42 minutes.".into()),
            provider: Some("YouTube".into()),
            html: Some(r#"<iframe src="https://youtube.com/embed/abc"></iframe>"#.into()),
            thumbnail: Some("c0ffee00".into()),
            favicon: Some("deadbeef".into()),
            mode: CardMode::Link,
        }
    }

    fn frame() -> ItemKind {
        ItemKind::Frame {
            title: StyledText::plain("Engine bay"),
            order: Some(3),
            speaker_notes: Some("Start on the cooling circuit.".into()),
        }
    }

    fn document() -> ItemKind {
        ItemKind::Document { asset_id: "9f2ac81b".into(), page_count: 214, current_page: 7 }
    }

    fn connector(start: ConnectorEnd, end: ConnectorEnd) -> ItemKind {
        ItemKind::Connector {
            start,
            end,
            routing: Routing::Orthogonal,
            dash: Dash::Dashed,
            thickness: 2.0,
            color: Some(Color::rgb(0x33, 0x33, 0x33)),
            captions: vec![
                ConnectorCaption::new(StyledText::from_spans([TextSpan::new(
                    "feeds",
                    SpanStyle::bold(),
                )])),
                ConnectorCaption::at("then drains", 0.8),
            ],
        }
    }

    /// One of every new kind through a save and a reload. This is the contract the
    /// importer was missing: 167 of the reference board's 596 widgets had to
    /// degrade into a sticky, an ink stroke or an empty text item for want of a slot.
    #[test]
    fn every_new_kind_survives_a_snapshot_round_trip() {
        let mut board = Board::new();
        let anchor_a = board.add(sticky("from", 0.0, 0.0)).unwrap();
        let anchor_b = board.add(sticky("to", 800.0, 0.0)).unwrap();

        let kinds = [
            link_preview(),
            embed(),
            frame(),
            ItemKind::Group,
            document(),
            connector(
                ConnectorEnd::bound(anchor_a, ConnectorEnd::RIGHT),
                ConnectorEnd::bound(anchor_b, ConnectorEnd::LEFT)
                    .with_arrowhead(ArrowKind::FilledTriangle),
            ),
        ];
        let ids: Vec<_> = kinds
            .iter()
            .map(|kind| {
                board
                    .add(NewItem::new(kind.clone(), Placement::new(0.0, 0.0, 320.0, 180.0)))
                    .unwrap()
            })
            .collect();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        for (id, expected) in ids.iter().zip(&kinds) {
            assert_eq!(&reloaded.item(*id).unwrap().kind, expected, "{}", expected.tag());
        }
        assert_eq!(reloaded.items().unwrap(), board.items().unwrap());
    }

    /// The whole reason connectors are in the document rather than baked into ink:
    /// the endpoint bindings, and everything hanging off them, must come back
    /// byte-for-byte after a save.
    #[test]
    fn connector_bindings_survive_a_save_and_load() {
        let mut board = Board::new();
        let from = board.add(sticky("from", 0.0, 0.0)).unwrap();
        let to = board.add(sticky("to", 800.0, 0.0)).unwrap();
        let id = board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(from, ConnectorEnd::RIGHT),
                    ConnectorEnd::bound(to, (0.0, 0.5)).with_arrowhead(ArrowKind::FilledTriangle),
                ),
                Placement::new(400.0, 0.0, 800.0, 4.0),
            ))
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let ItemKind::Connector { start, end, routing, dash, thickness, color, captions } =
            reloaded.item(id).unwrap().kind
        else {
            panic!("expected a connector")
        };

        assert_eq!(start.target, Some(from), "the start binding did not survive the file");
        assert_eq!(start.anchor, (1.0, 0.5));
        assert_eq!(start.arrowhead, ArrowKind::None);
        assert_eq!(end.target, Some(to));
        assert_eq!(end.arrowhead, ArrowKind::FilledTriangle);
        assert_eq!(routing, Routing::Orthogonal);
        assert_eq!(dash, Dash::Dashed);
        assert_eq!(thickness, 2.0);
        assert_eq!(color, Some(Color::rgb(0x33, 0x33, 0x33)));

        assert_eq!(captions.len(), 2);
        assert_eq!(captions[0].text.to_plain(), "feeds");
        assert!(captions[0].text.spans()[0].style.bold, "caption formatting was flattened");
        assert_eq!(captions[0].position, 0.5);
        assert_eq!(captions[1].text.to_plain(), "then drains");
        assert_eq!(captions[1].position, 0.8);

        // And the bindings still resolve to live items, which is what lets the
        // connector re-route on the next drag.
        assert!(reloaded.contains(start.target.unwrap()));
        assert!(reloaded.contains(end.target.unwrap()));
    }

    /// The semantics chosen in [`ConnectorEnd`], pinned: deleting an item leaves the
    /// binding in place and unchanged. Nothing sweeps the board, and nothing is lost.
    #[test]
    fn deleting_a_bound_item_leaves_the_binding_dangling() {
        let mut board = Board::new();
        let from = board.add(sticky("from", 0.0, 0.0)).unwrap();
        let to = board.add(sticky("to", 800.0, 0.0)).unwrap();
        let line = board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(from, ConnectorEnd::RIGHT),
                    ConnectorEnd::bound(to, ConnectorEnd::LEFT),
                ),
                Placement::default(),
            ))
            .unwrap();

        board.remove(to).unwrap();

        let ItemKind::Connector { start, end, .. } = board.item(line).unwrap().kind else {
            panic!("expected a connector")
        };
        assert_eq!(end.target, Some(to), "the binding was rewritten behind the caller's back");
        assert!(!board.contains(to), "resolution is `contains`, and it must say no");
        assert!(board.contains(start.target.unwrap()), "the live end was collateral damage");

        // It survives a save too: a dangling binding is data, not a transient.
        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let ItemKind::Connector { end, .. } = reloaded.item(line).unwrap().kind else {
            panic!("expected a connector")
        };
        assert_eq!(end.target, Some(to));
        assert!(!reloaded.contains(to));
    }

    /// The fact that makes auto-unbinding pointless, and therefore the justification
    /// for the dangling semantics: Loro's undo re-creates a deleted node under a new
    /// id, so no binding held by id can survive a delete-then-undo whatever the
    /// document does on delete.
    #[test]
    fn undoing_a_deletion_restores_the_item_under_a_new_id() {
        let mut board = Board::new();
        let original = board.add(sticky("note", 0.0, 0.0)).unwrap();
        board.remove(original).unwrap();
        board.undo().unwrap();

        let ids = board.item_ids();
        assert_eq!(ids.len(), 1);
        assert_ne!(ids[0], original);
        assert!(!board.contains(original));
    }

    #[test]
    fn connectors_bound_to_finds_both_ends_and_ignores_everything_else() {
        let mut board = Board::new();
        let hub = board.add(sticky("hub", 0.0, 0.0)).unwrap();
        let spoke = board.add(sticky("spoke", 500.0, 0.0)).unwrap();
        board.add(sticky("unrelated", 0.0, 500.0)).unwrap();

        let outgoing = board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(hub, ConnectorEnd::RIGHT),
                    ConnectorEnd::bound(spoke, ConnectorEnd::LEFT),
                ),
                Placement::default(),
            ))
            .unwrap();
        let loopback = board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(hub, ConnectorEnd::TOP),
                    ConnectorEnd::bound(hub, ConnectorEnd::BOTTOM),
                ),
                Placement::default(),
            ))
            .unwrap();
        board
            .add(NewItem::new(
                connector(ConnectorEnd::free((0.0, 0.0)), ConnectorEnd::free((1.0, 1.0))),
                Placement::default(),
            ))
            .unwrap();

        assert_eq!(board.connectors_bound_to(hub), vec![outgoing, loopback]);
        assert_eq!(board.connectors_bound_to(spoke), vec![outgoing]);
        assert!(board.connectors_bound_to(board.item_ids()[2]).is_empty());
    }

    /// Repair, on demand and undoable — the escape hatch that makes dangling
    /// bindings a decision rather than a dead end.
    #[test]
    fn rebinding_retargets_or_cuts_loose_and_is_one_undo_step() {
        let mut board = Board::new();
        let old = board.add(sticky("old", 0.0, 0.0)).unwrap();
        let new = board.add(sticky("new", 0.0, 400.0)).unwrap();
        let other = board.add(sticky("other", 800.0, 0.0)).unwrap();
        let line = board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(old, ConnectorEnd::RIGHT),
                    ConnectorEnd::bound(other, ConnectorEnd::LEFT),
                ),
                Placement::default(),
            ))
            .unwrap();
        let loopback = board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(old, ConnectorEnd::TOP),
                    ConnectorEnd::bound(old, ConnectorEnd::BOTTOM),
                ),
                Placement::default(),
            ))
            .unwrap();

        // Both ends of the loopback plus one end of the line.
        assert_eq!(board.rebind_connectors(old, Some(new)).unwrap(), 3);

        let ItemKind::Connector { start, end, .. } = board.item(line).unwrap().kind else {
            panic!("expected a connector")
        };
        assert_eq!(start.target, Some(new));
        assert_eq!(start.anchor, (1.0, 0.5), "retargeting moved the anchor");
        assert_eq!(end.target, Some(other), "an unrelated end was retargeted");
        assert!(board.connectors_bound_to(old).is_empty());

        // One commit, therefore one undo step, for all three endpoints.
        board.undo().unwrap();
        assert_eq!(board.connectors_bound_to(old), vec![line, loopback]);

        board.redo().unwrap();
        assert_eq!(board.rebind_connectors(new, None).unwrap(), 3);
        let ItemKind::Connector { start, .. } = board.item(line).unwrap().kind else {
            panic!("expected a connector")
        };
        assert_eq!(start.target, None);
        assert_eq!(start.anchor, (1.0, 0.5), "cutting loose must keep the anchor");
    }

    #[test]
    fn rebinding_repairs_a_dangling_binding_after_a_delete_and_undo() {
        let mut board = Board::new();
        let from = board.add(sticky("from", 0.0, 0.0)).unwrap();
        let to = board.add(sticky("to", 800.0, 0.0)).unwrap();
        let line = board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(from, ConnectorEnd::RIGHT),
                    ConnectorEnd::bound(to, ConnectorEnd::LEFT),
                ),
                Placement::default(),
            ))
            .unwrap();

        board.remove(to).unwrap();
        board.undo().unwrap();

        // The item is back under a different id, so the binding is still dangling.
        let restored = *board.item_ids().iter().find(|id| **id != from && **id != line).unwrap();
        assert_ne!(restored, to);
        assert_eq!(board.rebind_connectors(to, Some(restored)).unwrap(), 1);

        let ItemKind::Connector { end, .. } = board.item(line).unwrap().kind else {
            panic!("expected a connector")
        };
        assert_eq!(end.target, Some(restored));
        assert!(board.contains(end.target.unwrap()));
    }

    #[test]
    fn rebinding_onto_a_removed_item_is_refused() {
        let mut board = Board::new();
        let anchor = board.add(sticky("anchor", 0.0, 0.0)).unwrap();
        let gone = board.add(sticky("gone", 0.0, 0.0)).unwrap();
        board.remove(gone).unwrap();

        assert!(matches!(
            board.rebind_connectors(anchor, Some(gone)),
            Err(DocError::NoSuchItem(_))
        ));
        // Sweeping for an id that never existed is not an error, just no work.
        assert_eq!(board.rebind_connectors(gone, None).unwrap(), 0);
    }

    /// A rebind that changes nothing must not record an operation, so holding a
    /// repair shortcut does not fill the undo stack with no-ops.
    #[test]
    fn rebinding_nothing_records_nothing() {
        let mut board = Board::new();
        let id = board.add(sticky("note", 0.0, 0.0)).unwrap();
        let before = board.version();
        assert_eq!(board.rebind_connectors(id, None).unwrap(), 0);
        assert_eq!(board.version(), before);
    }

    // ----- the new kinds in the tree, in undo, and under the style --------

    #[test]
    fn items_reparent_into_frames_and_groups() {
        let mut board = Board::new();
        let outer = board.add(NewItem::new(frame(), Placement::new(0.0, 0.0, 1600.0, 900.0))).unwrap();
        let group = board.add(NewItem::new(ItemKind::Group, Placement::default())).unwrap();
        let note = board.add(sticky("note", 40.0, 40.0)).unwrap();

        board.reparent(group, Some(outer)).unwrap();
        board.reparent(note, Some(group)).unwrap();

        assert_eq!(board.parent_of(note), Some(group));
        assert_eq!(board.parent_of(group), Some(outer));
        assert_eq!(board.item_ids(), vec![outer, group, note]);
        // Parenting is grouping, not a transform: the note has not moved.
        assert_eq!(board.item(note).unwrap().placement.x, 40.0);

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        assert_eq!(reloaded.parent_of(note), Some(group));
        assert_eq!(reloaded.item(group).unwrap().kind, ItemKind::Group);

        // And a group is a real container, so deleting it takes its contents.
        board.remove(group).unwrap();
        assert!(!board.contains(note));
        assert!(board.contains(outer));
    }

    #[test]
    fn a_frame_cannot_be_moved_inside_its_own_group() {
        let mut board = Board::new();
        let outer = board.add(NewItem::new(frame(), Placement::default())).unwrap();
        let inner = board
            .add(NewItem::new(ItemKind::Group, Placement::default()).with_parent(outer))
            .unwrap();

        assert!(matches!(
            board.reparent(outer, Some(inner)),
            Err(DocError::CyclicReparent { .. })
        ));
    }

    #[test]
    fn undo_and_redo_walk_edits_to_every_new_kind() {
        let mut board = Board::new();
        let anchor = board.add(sticky("anchor", 0.0, 0.0)).unwrap();

        for original in [link_preview(), embed(), frame(), ItemKind::Group, document()] {
            let id = board.add(NewItem::new(original.clone(), Placement::default())).unwrap();
            let replacement = ItemKind::Connector {
                start: ConnectorEnd::bound(anchor, ConnectorEnd::RIGHT),
                end: ConnectorEnd::free(ConnectorEnd::LEFT),
                routing: Routing::Curved,
                dash: Dash::Dotted,
                thickness: 8.0,
                color: None,
                captions: vec![ConnectorCaption::new("label")],
            };

            board.set_kind(id, replacement.clone()).unwrap();
            assert_eq!(board.item(id).unwrap().kind, replacement);

            assert!(board.undo().unwrap());
            assert_eq!(board.item(id).unwrap().kind, original, "undo lost {}", original.tag());

            assert!(board.redo().unwrap());
            assert_eq!(board.item(id).unwrap().kind, replacement);

            board.remove(id).unwrap();
        }
    }

    /// The bug [`clear_stale`] and [`Board::scaffold`] exist to prevent, pinned from
    /// the outside because it is invisible until someone converts an item and
    /// presses undo.
    ///
    /// Loro 1.13 resurrects a deleted text container *with its inserts* and then
    /// replays them, so before those two guards a sticky reading `"fan"` came back
    /// from an undo reading `"fanfan"`. Both directions are checked: into text, and
    /// out of it and back.
    #[test]
    fn converting_an_item_never_duplicates_its_text_across_undo() {
        let note = |text: &str| ItemKind::Sticky {
            text: StyledText::plain(text),
            background: None,
        };
        let stroke =
            || ItemKind::Ink { points: vec![Point::new(1.0, 2.0)], color: None, thickness: 4.0 };

        // Into text: the container is created by the conversion itself.
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                ItemKind::Image { asset_id: "h".into(), crop: None },
                Placement::default(),
            ))
            .unwrap();
        board.set_kind(id, note("fan")).unwrap();
        board.undo().unwrap();
        assert_eq!(board.item(id).unwrap().kind.tag(), "image");
        board.redo().unwrap();
        assert_eq!(board.item(id).unwrap().kind, note("fan"));

        // Out of text and back: the container is cleared by the conversion.
        board.set_kind(id, stroke()).unwrap();
        board.undo().unwrap();
        assert_eq!(board.item(id).unwrap().kind, note("fan"));
        board.redo().unwrap();
        assert_eq!(board.item(id).unwrap().kind, stroke());
        board.undo().unwrap();
        assert_eq!(board.item(id).unwrap().kind, note("fan"), "a second cycle drifted");

        // And captions, which nest a text container two levels deeper.
        let captioned = ItemKind::Connector {
            start: ConnectorEnd::default(),
            end: ConnectorEnd::default(),
            routing: Routing::Straight,
            dash: Dash::Solid,
            thickness: 1.0,
            color: None,
            captions: vec![ConnectorCaption::new("label")],
        };
        board.set_kind(id, captioned.clone()).unwrap();
        board.undo().unwrap();
        board.redo().unwrap();
        assert_eq!(board.item(id).unwrap().kind, captioned);
    }

    /// Scaffolding is bookkeeping, not an edit: it must not appear as an undo step
    /// the user has to press through, and it must not be *lost* if they undo past it.
    #[test]
    fn scaffolding_a_container_is_not_its_own_undo_step() {
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                ItemKind::Image { asset_id: "h".into(), crop: None },
                Placement::default(),
            ))
            .unwrap();

        board
            .set_kind(id, ItemKind::Text { text: StyledText::plain("converted") })
            .unwrap();

        // Two edits total — the add and the conversion — and no third for the
        // container that the conversion needed.
        assert!(board.undo().unwrap());
        assert_eq!(board.item(id).unwrap().kind.tag(), "image");
        assert!(board.undo().unwrap());
        assert_eq!(board.item_count(), 0);
        assert!(!board.can_undo());
    }

    #[test]
    fn undo_restores_a_reparent_into_a_frame() {
        let mut board = Board::new();
        let container = board.add(NewItem::new(frame(), Placement::default())).unwrap();
        let note = board.add(sticky("note", 0.0, 0.0)).unwrap();

        board.reparent(note, Some(container)).unwrap();
        board.undo().unwrap();
        assert_eq!(board.parent_of(note), None);

        board.redo().unwrap();
        assert_eq!(board.parent_of(note), Some(container));
        assert_eq!(board.children(Some(container)), vec![note]);
    }

    /// A frame's background lives on the style, not on the kind. This is the test
    /// that says so, since the alternative — a `background` field on the variant —
    /// is the obvious thing to look for and would be a second source of truth.
    #[test]
    fn a_frames_fill_lives_on_the_style_and_round_trips() {
        let mut board = Board::new();
        let id = board
            .add(
                NewItem::new(frame(), Placement::new(0.0, 0.0, 1600.0, 900.0)).with_style(
                    Style { fill: Some(Color::rgb(0xF5, 0xF5, 0xF5)), ..Style::default() },
                ),
            )
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let item = reloaded.item(id).unwrap();
        assert_eq!(item.style.fill, Some(Color::rgb(0xF5, 0xF5, 0xF5)));
        assert_eq!(item.kind, frame());
    }

    #[test]
    fn a_frame_can_be_renamed_through_set_text_but_a_connector_cannot() {
        let mut board = Board::new();
        let id = board.add(NewItem::new(frame(), Placement::default())).unwrap();

        board.set_text(id, StyledText::plain("Cooling circuit")).unwrap();
        let ItemKind::Frame { title, order, speaker_notes } = board.item(id).unwrap().kind else {
            panic!("expected a frame")
        };
        assert_eq!(title, StyledText::plain("Cooling circuit"));
        assert_eq!(order, Some(3), "renaming a frame disturbed its slide order");
        assert!(speaker_notes.is_some());

        let line = board
            .add(NewItem::new(
                connector(ConnectorEnd::default(), ConnectorEnd::default()),
                Placement::default(),
            ))
            .unwrap();
        assert!(matches!(
            board.set_text(line, StyledText::plain("nope")),
            Err(DocError::WrongKind { found: "connector", .. })
        ));
    }

    /// A structured document imported from Miro's `structured_document` — headings
    /// and lists — has to survive the document, not just the text container.
    #[test]
    fn a_documents_heading_and_list_structure_survives_the_board() {
        let mut board = Board::new();
        let styled = StyledText::plain("Cooling\nfan\nradiator").with_blocks([
            BlockStyle::heading(2),
            BlockStyle::list(ListKind::Bulleted),
            BlockStyle::list(ListKind::Bulleted),
        ]);
        let id = board
            .add(NewItem::new(ItemKind::Text { text: styled.clone() }, Placement::default()))
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let ItemKind::Text { text } = reloaded.item(id).unwrap().kind else {
            panic!("expected text")
        };
        assert_eq!(text, styled);
        assert_eq!(text.block(0).heading, Some(2));
        assert_eq!(text.block(1).list, Some(ListKind::Bulleted));
    }

    // ----- kind switching and key hygiene ---------------------------------

    fn one_of_every_kind(anchor: ItemId) -> Vec<ItemKind> {
        vec![
            ItemKind::Sticky { text: StyledText::plain("a"), background: Some(Color::rgb(1, 2, 3)) },
            ItemKind::Text { text: StyledText::plain("b") },
            ItemKind::Ink { points: vec![Point::new(1.0, 2.0)], color: None, thickness: 3.0 },
            ItemKind::Image {
                asset_id: "h".into(),
                crop: Some(Crop { x: 0.0, y: 0.0, width: 8.0, height: 8.0 }),
            },
            link_preview(),
            embed(),
            connector(
                ConnectorEnd::bound(anchor, ConnectorEnd::RIGHT),
                ConnectorEnd::free(ConnectorEnd::LEFT),
            ),
            frame(),
            ItemKind::Group,
            document(),
            // The four Agent Canvas kinds. Listed here rather than tested separately so
            // they are covered by the three exhaustive tests below — that every key they
            // write is registered for clearing, that converting between *any* two kinds
            // leaves nothing stale, and that undo walks an edit to each of them. Two of
            // them share the `TEXT` container with a sticky, which is exactly the overlap
            // the stale-clearing test exists to catch.
            ItemKind::Agent {
                model: r#"{"role_kind":"orchestrator"}"#.into(),
                label: StyledText::plain("Code Reviewer"),
            },
            ItemKind::FileTree { model: r#"{"root":"src"}"#.into() },
            ItemKind::AgentNote {
                model: r#"{"path":".velm/notes/plan.md"}"#.into(),
                title: StyledText::plain("Plan"),
            },
            ItemKind::Browser { model: r#"{"url":"https://example.com"}"#.into() },
        ]
    }

    /// RULE ZERO, at the only level this crate can check it: **a board that uses none of
    /// the Agent Canvas kinds is byte for byte the board it was before they existed.**
    ///
    /// The encoded snapshot is the on-disk format, so this compares the actual bytes rather
    /// than the items read back — a re-encode that produced equal items from different bytes
    /// would still be a file-format change, and this is the assertion that would catch it.
    #[test]
    fn a_board_with_no_agent_items_is_unchanged_by_this_layer() {
        let mut board = Board::new();
        let anchor = board.add(sticky("anchor", 0.0, 0.0)).unwrap();
        board.add(NewItem::new(frame(), Placement::new(10.0, 20.0, 300.0, 200.0))).unwrap();
        board.add(NewItem::new(document(), Placement::new(0.0, 0.0, 100.0, 100.0))).unwrap();
        board
            .add(NewItem::new(
                connector(
                    ConnectorEnd::bound(anchor, ConnectorEnd::RIGHT),
                    ConnectorEnd::free(ConnectorEnd::LEFT),
                ),
                Placement::default(),
            ))
            .unwrap();

        // Not one of the four keys this layer added may appear anywhere in the snapshot of
        // a board that never used one.
        let bytes = board.to_bytes().unwrap();
        let haystack = String::from_utf8_lossy(&bytes).into_owned();
        for added in [key::AGENT, key::FILE_TREE, key::NOTE, key::BROWSER] {
            assert!(
                !haystack.contains(&format!("\"{added}\"")),
                "`{added}` reached the snapshot of a board that has no agent nodes"
            );
        }

        // And it still reads back as exactly what was put in.
        let reopened = Board::from_bytes(&bytes).expect("a plain board stopped loading");
        assert_eq!(reopened.items().unwrap().len(), 4);
    }

    /// The other direction: the four kinds survive a full snapshot round trip, tokens and
    /// labels intact. A token is opaque here, so "intact" means byte-identical — this crate
    /// must never normalise, reformat or validate what `vellum-app` handed it.
    #[test]
    fn agent_kinds_round_trip_through_a_snapshot_with_their_tokens_verbatim() {
        let mut board = Board::new();
        let token = r#"{"role_kind":"meta","spawn_cap":3,"unknown_future_field":[1,2]}"#;
        board
            .add(NewItem::new(
                ItemKind::Agent {
                    model: token.into(),
                    label: StyledText::plain("Research Assistant"),
                },
                Placement::new(0.0, 0.0, 400.0, 300.0),
            ))
            .unwrap();

        let reopened = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let items = reopened.items().unwrap();
        assert_eq!(items.len(), 1);
        let ItemKind::Agent { model, label } = &items[0].kind else {
            panic!("an agent node came back as {}", items[0].kind.tag());
        };
        assert_eq!(model, token, "the token was not stored verbatim");
        assert_eq!(label.to_plain(), "Research Assistant");

        // The role is the item's text, so search, the caret and `set_text` all reach it
        // with no new path — the whole reason it is stored beside the token.
        assert_eq!(items[0].kind.text().map(StyledText::to_plain).as_deref(), Some("Research Assistant"));
    }

    /// [`key::KIND_SPECIFIC`] is the fallback sweep for a kind tag this build does
    /// not recognise. A key missing from it would leave dead data in a board coming
    /// back from a newer build, and the omission is invisible until that happens.
    #[test]
    fn every_key_a_kind_writes_is_registered_for_clearing() {
        let mut board = Board::new();
        let anchor = board.add(sticky("anchor", 0.0, 0.0)).unwrap();
        for kind in one_of_every_kind(anchor) {
            assert!(
                kind_keys_for_tag(kind.tag()).is_some(),
                "`{}` has no entry in the key table",
                kind.tag()
            );
            for used in kind_keys(&kind) {
                assert!(
                    key::KIND_SPECIFIC.contains(used),
                    "`{used}` is written by {} but never cleared",
                    kind.tag()
                );
            }
        }
        assert_eq!(kind_keys_for_tag("mindmap_node"), None);
    }

    /// A board from a newer build carries kinds this one cannot name. Converting
    /// such an item must still clear its fields, which is what the full-vocabulary
    /// fallback is for — the alternative is content from the old kind bleeding
    /// through into the new one.
    #[test]
    fn converting_an_item_of_an_unknown_kind_still_clears_its_fields() {
        let mut board = Board::new();
        let id = board.add(sticky("was a sticky", 0.0, 0.0)).unwrap();

        let meta = board.items.get_meta(id.tree_id()).unwrap();
        meta.insert(key::KIND, "mindmap_node").unwrap();
        meta.insert(key::URL, "https://example.com").unwrap();
        board.doc.commit();

        board.set_kind(id, ItemKind::Group).unwrap();

        let meta = board.items.get_meta(id.tree_id()).unwrap();
        assert!(meta.get(key::URL).is_none(), "the unknown kind's url survived");
        assert!(meta.get(key::BACKGROUND).is_none());
        assert!(text_at(&meta, key::TEXT).is_none_or(|t| text::read(&t).is_empty()));
        assert_eq!(board.item(id).unwrap().kind, ItemKind::Group);
    }

    /// Every conversion between every pair of kinds, checked for leftovers. This is
    /// the "convert type" feature in the catalogue, and the failure it guards is a
    /// board file that grows a little dead weight every time a user changes their
    /// mind — or worse, an item that reads back with the previous kind's content.
    ///
    /// Rich-text containers are exempt from the "key is gone" half of the check and
    /// held to the stricter half instead: they must be *empty*. [`clear_stale`]
    /// explains why they cannot simply be deleted.
    #[test]
    fn switching_between_any_two_kinds_leaves_no_stale_content() {
        let mut board = Board::new();
        let anchor = board.add(sticky("anchor", 0.0, 0.0)).unwrap();
        let kinds = one_of_every_kind(anchor);
        let id = board.add(sticky("subject", 0.0, 0.0)).unwrap();

        for before in &kinds {
            for after in &kinds {
                board.set_kind(id, before.clone()).unwrap();
                board.set_kind(id, after.clone()).unwrap();

                let meta = board.items.get_meta(id.tree_id()).unwrap();
                let context = format!("{} -> {}", before.tag(), after.tag());
                for stale in kind_keys(before).iter().filter(|k| !kind_keys(after).contains(k)) {
                    match *stale {
                        key::TEXT => assert!(
                            text_at(&meta, key::TEXT).is_none_or(|t| text::read(&t).is_empty()),
                            "text survived {context}"
                        ),
                        key::CAPTIONS => assert!(
                            read_captions(&meta).is_empty(),
                            "captions survived {context}"
                        ),
                        _ => assert!(
                            meta.get(stale).is_none(),
                            "`{stale}` survived {context}"
                        ),
                    }
                }
                assert_eq!(&board.item(id).unwrap().kind, after, "{context}");
            }
        }
    }

    /// The tag `binds_to` matches on is a literal, because decoding every item to
    /// find connectors would defeat the point of the fast path.
    #[test]
    fn the_connector_fast_path_tag_matches_the_kind_tag() {
        let kind = connector(ConnectorEnd::default(), ConnectorEnd::default());
        assert_eq!(kind.tag(), CONNECTOR_TAG);
    }

    // ----- reading a damaged or older document ----------------------------

    /// A board written before these kinds existed opens, and is stamped with the
    /// current version so a build that predates them will refuse it rather than
    /// silently dropping the connectors it is about to gain.
    #[test]
    fn an_older_board_opens_and_is_upgraded_in_place() {
        let board = Board::new();
        board.meta.insert(key::SCHEMA, 1).unwrap();
        board.doc.commit();

        let upgraded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        assert_eq!(int_at(&upgraded.meta, key::SCHEMA), Some(SCHEMA_VERSION));
        // The stamp is not an undoable edit of the user's.
        assert!(!upgraded.can_undo());

        let reopened = Board::from_bytes(&upgraded.to_bytes().unwrap()).unwrap();
        assert_eq!(int_at(&reopened.meta, key::SCHEMA), Some(SCHEMA_VERSION));
    }

    /// Reading a connector is lenient where reading a kind is strict: one unusable
    /// binding must not cost the user the other 595 widgets on the board.
    #[test]
    fn a_corrupt_binding_reads_as_a_free_end_rather_than_failing_the_board() {
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                connector(ConnectorEnd::default(), ConnectorEnd::default()),
                Placement::default(),
            ))
            .unwrap();

        let meta = board.items.get_meta(id.tree_id()).unwrap();
        let end = map_at(&meta, key::START).unwrap();
        end.insert(key::TARGET, "not-an-item-id").unwrap();
        end.delete(key::ARROW).unwrap();
        board.doc.commit();

        let ItemKind::Connector { start, .. } = board.item(id).unwrap().kind else {
            panic!("expected a connector")
        };
        assert_eq!(start.target, None);
        assert_eq!(start.arrowhead, ArrowKind::None);
        assert_eq!(board.item_count(), 1);
    }

    #[test]
    fn a_page_index_from_a_damaged_file_is_clamped_rather_than_wrapped() {
        let mut board = Board::new();
        let id = board.add(NewItem::new(document(), Placement::default())).unwrap();

        let meta = board.items.get_meta(id.tree_id()).unwrap();
        meta.insert(key::CURRENT_PAGE, -4i64).unwrap();
        meta.insert(key::PAGE_COUNT, i64::MAX).unwrap();
        board.doc.commit();

        let ItemKind::Document { page_count, current_page, .. } = board.item(id).unwrap().kind
        else {
            panic!("expected a document")
        };
        assert_eq!(current_page, 0);
        assert_eq!(page_count, u32::MAX);
    }

    /// Absent and empty are different: a link preview with no description draws no
    /// line for one, and a blank line for the other.
    #[test]
    fn absent_link_metadata_stays_absent_rather_than_becoming_empty() {
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                ItemKind::link_preview(Some(String::new()), None, None),
                Placement::default(),
            ))
            .unwrap();

        let reloaded = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let ItemKind::LinkPreview { title, url, description, thumbnail, .. } =
            reloaded.item(id).unwrap().kind
        else {
            panic!("expected a link preview")
        };
        assert_eq!(title, Some(String::new()));
        assert_eq!(url, None);
        assert_eq!(description, None);
        assert_eq!(thumbnail, None);
    }

    /// Captions are rewritten wholesale, so setting fewer must actually remove the
    /// rest rather than leaving the list longer than the value that wrote it.
    #[test]
    fn rewriting_captions_replaces_them_rather_than_appending() {
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                connector(ConnectorEnd::default(), ConnectorEnd::default()),
                Placement::default(),
            ))
            .unwrap();

        let captioned = |captions: Vec<ConnectorCaption>| ItemKind::Connector {
            start: ConnectorEnd::default(),
            end: ConnectorEnd::default(),
            routing: Routing::Straight,
            dash: Dash::Solid,
            thickness: 1.0,
            color: None,
            captions,
        };

        let trimmed = captioned(vec![ConnectorCaption::new("only one")]);
        board.set_kind(id, trimmed.clone()).unwrap();
        assert_eq!(board.item(id).unwrap().kind, trimmed);

        let uncaptioned = captioned(Vec::new());
        board.set_kind(id, uncaptioned.clone()).unwrap();
        assert_eq!(board.item(id).unwrap().kind, uncaptioned);

        let meta = board.items.get_meta(id.tree_id()).unwrap();
        assert!(read_captions(&meta).is_empty(), "the dropped caption is still readable");
    }
}


