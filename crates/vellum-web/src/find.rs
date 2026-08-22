//! Finding a word on the board, and knowing where to point the camera at it.
//!
//! The desktop has had a find bar since `vellum-app`'s `actions::find`; a browser tab has
//! had nothing. This is that feature, and it is deliberately **not** the desktop's
//! implementation carried across.
//!
//! # Why this uses `vellum-search` and the desktop does not
//!
//! `actions::search` is a substring scan: it lowercases the query, walks every projected
//! item, and asks `words::contains`. That is honest for what it is, and it answers only the
//! query the user typed exactly — `coolent` finds nothing at all, and there is no way to ask
//! for *"every frame"*.
//!
//! `crates/vellum-search` is a finished inverted index with prefix, substring, phrase and
//! bounded-Levenshtein matching, kind and colour facets, and a ranked, total order. It has
//! exactly one dependency (`thiserror`) and it compiles for `wasm32-unknown-unknown`.
//!
//! ⚠ **It also has no caller anywhere in the workspace — this module is its first.** That is
//! worth stating rather than discovering: uncalled code is this repository's signature
//! defect, and a crate nothing calls is a crate whose wiring has never been exercised. Treat
//! a *"search finds nothing"* report as a wiring problem in this file before suspecting the
//! index.
//!
//! # One document per **label**, not per item
//!
//! `vellum_project::words::of` answers a `Vec<String>` — one entry per *label*: a cell, a
//! card, a node, a category. Its own module note explains why they must not be joined:
//! joining invents adjacencies, so a search for `"todo measure"` would match a column called
//! *To do* followed by a card called *Measure the atlas*, a phrase that appears nowhere on
//! the board.
//!
//! A single index document per item would have exactly that bug, because the index scores
//! phrases by token *position* and two labels concatenated are adjacent positions. So each
//! label becomes its own document and [`Finder::search`] deduplicates back to one [`Match`]
//! per item, keeping the best-ranked label. The seam is un-matchable **by construction**
//! rather than by a guard somebody could remove.
//!
//! An item with no words at all still contributes one empty document, so `every image` and
//! `all yellow stickies` — facet queries, which is how people search a *visual* board — find
//! items that have no text to match. That is `vellum_search`'s own documented behaviour, not
//! an invention here.
//!
//! # The bounds, named
//!
//! A search runs on the frame thread, so it has to be bounded and the bound has to be
//! stated rather than implied:
//!
//! | Bound | Value | What it protects |
//! |---|---|---|
//! | [`MAX_INDEXED_ITEMS`] | 20,000 | The index build. Items past it are dropped and [`Finder::truncated`] answers `true`. |
//! | [`MAX_LABELS_PER_ITEM`] | 512 | One pathological table cannot dominate the index. |
//! | [`MAX_LABEL_CHARS`] | 1,024 | A note title pasted from a document. **Characters, not bytes.** |
//! | [`MAX_RESULTS`] | 200 | What comes back, after dedup. |
//! | `MAX_ANCESTOR_HOPS` | 16 | The walk to a match's enclosing frame. |
//!
//! **The index is built once per board version, not once per keystroke.** [`Finder::search`]
//! compares [`Projection::generation`] against what it last built for and rebuilds only when
//! the board has actually moved — which in this client is `live.rs`'s merge calling
//! `Projection::rebuild`, the only mutation path a reader has. An empty query returns before
//! it looks at the generation at all, so a find bar nobody has opened costs nothing.
//!
//! ⚠ **What is *not* bounded is honest to say: the build is O(all text on the board)** and it
//! happens on the frame that follows a merge. At the reference board's 1,306 items it is one
//! pass over text that was already being shaped every frame. At 100,000 items it would be a
//! visible stall on the frame after each sync, and the answer then is a worker, not a
//! smaller constant. No measurement of the build time exists yet — see the report.
//!
//! # Two passes, and only the second one guesses
//!
//! The first pass is `Matching::interactive()` — the index's default: prefix, substring and
//! facets, and **`fuzzy: 0`**. That last part is easy to misread, and it was: the crate can
//! do bounded-Levenshtein matching and does not do it unless asked.
//!
//! So a query that comes back empty is retried once with `Matching::forgiving()`, which is
//! the use its own doc comment names — *"for a query that returned nothing, or a deliberate
//! 'did you mean' pass"*. `coolent` finds `coolant` that way and `cool` never has to,
//! because the second pass runs **only when the first found nothing**. A typo costs one
//! extra sweep; a word that is on the board costs none.
//!
//! # Ranked order, not paint order
//!
//! The desktop steps through matches in paint order, *"so stepping through matches walks the
//! board the way it reads"*. This returns them in `vellum-search`'s rank order — exact
//! before prefix before substring before fuzzy, earliest position first. That is a
//! deliberate divergence, and the second pass is why it has to be: walking the board in
//! reading order would put a two-edit guess ahead of a word the user typed exactly. The
//! order is total and stable, so stepping is still repeatable.
//!
//! # ⚠ Nothing in this file is browser-specific, and that is on purpose
//!
//! No `wasm_bindgen`, no `web_sys`, no `js_sys` — only the document, the projection, the
//! scene and the index. `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so **the tests at
//! the bottom of this file cannot run here**: a native `cargo test` compiles this crate away
//! to nothing and reports success having executed none of them.
//!
//! That is not a reason to write none. This module is shaped to move to `vellum-project`
//! beside [`card`](vellum_project::card), whose own doc comment records the identical
//! situation — *"pure, and tested here because `vellum-web` is `cfg(target_arch = "wasm32")`
//! and can hold no runnable test"*. Moving it is a `git mv` and one `mod` line; the tests
//! begin running the moment it lands.

use std::collections::HashSet;

use serde::Serialize;
use vellum_doc::ItemKind;
use vellum_project::project::Projection;
use vellum_scene::{ItemId as SceneId, WorldRect};
use vellum_search::{Colour, Index, Item, Matching, Query};

/// The most items that are indexed. Past this the board is searched partially and
/// [`Finder::truncated`] says so, which is the difference between a bounded feature and a
/// silently wrong one.
pub const MAX_INDEXED_ITEMS: usize = 20_000;

/// The most labels taken from any one item. A table with more cells than this is searchable
/// down to its five-hundred-and-twelfth.
pub const MAX_LABELS_PER_ITEM: usize = 512;

/// The most **characters** — never bytes — kept from a single label.
pub const MAX_LABEL_CHARS: usize = 1_024;

/// The most matches returned, after deduplicating labels back to items.
pub const MAX_RESULTS: usize = 200;

/// How many raw hits to ask the index for per result wanted.
///
/// Hits are per *label* and results are per *item*, so a table whose sixty cells all match
/// would otherwise fill the whole answer with one item. Four is enough that a board of
/// structured widgets still returns a useful spread without asking the index for thousands.
const OVERSAMPLE: usize = 4;

/// A ceiling on the oversampled ask, so `MAX_RESULTS * OVERSAMPLE` cannot become a large
/// allocation if either constant is raised later.
const MAX_RAW_HITS: usize = 2_000;

/// How far up the parent chain [`Finder::search`] looks for an enclosing frame.
///
/// A projection's parent pointers form a tree and should not cycle. `should not` is not
/// `cannot` — this is a `while` loop over data that came off a CRDT, and a bounded walk
/// costs one comparison where an unbounded one costs the tab.
const MAX_ANCESTOR_HOPS: usize = 16;

/// The board id given to the index.
///
/// `vellum-search` is a *cross-board* index and keys every document by board and item; this
/// client holds one board at a time, so the board half is a constant. It is not dropped,
/// because the crate's own filters and `Hit::board` are built around it — and because a
/// second board in a tab later is then a change of this constant rather than a change of
/// shape.
const BOARD: &str = "board";

/// How far in [`focus`]'s rectangle should be zoomed to, at most.
///
/// The desktop's `show_match` fits the match and then backs off: *"Fitting one sticky fills
/// the screen with it; back off to something a person can read in context."* The same number,
/// exported rather than reimplemented, so the browser's find bar and the desktop's agree
/// about what "show me this" means.
pub const FOCUS_MAX_ZOOM: f64 = 1.5;
// `FOCUS_MARGIN` was defined here and is gone: it duplicated `lib.rs`'s `FIT_MARGIN`, same
// 0.02, and the export uses that one. Two spellings of a number that is a **fraction of the
// rectangle** is exactly how a margin in pixels gets passed instead — which drove every board
// to open at 1.0% once already.

/// One match, already reduced to the item the user wants to be taken to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Match {
    /// The scene id, which is what [`focus`] and the viewer's selection both speak.
    pub item: SceneId,
    /// Where this is on the board, for a result row: the kind, and the frame it sits on if
    /// it sits on one — `"Sticky note in Engine bay"`.
    ///
    /// Serialised as `where`, which is what it would have been called if that were not a
    /// keyword.
    #[serde(rename = "where")]
    pub where_: String,
    /// A window of the matched label with the match in it, ellipsed at either end if the
    /// label is longer than the window.
    ///
    /// Built by [`vellum_search::Snippet`], which is character-safe — see the note on
    /// [`Finder::search`].
    pub excerpt: String,
}

/// The index over one board, and the query path onto it.
///
/// Held by the viewer across frames. Rebuilt lazily, from [`Projection::generation`].
pub struct Finder {
    index: Index,
    /// One entry per index document, in document order: which item that document's label
    /// belongs to.
    ///
    /// This is why a document's id is its decimal position here rather than an encoded
    /// `item:ordinal` pair. Parsing a compound id means splitting a string that came back
    /// from another crate; an index into a vector this module owns is a `parse::<usize>`
    /// and a `Vec::get`, both of which answer `None` instead of panicking.
    docs: Vec<SceneId>,
    /// The projection generation the index was built from, or `None` for never built.
    built_for: Option<u64>,
    indexed: usize,
    truncated: bool,
}

impl Default for Finder {
    fn default() -> Self {
        Self::new()
    }
}

impl Finder {
    pub fn new() -> Self {
        Self {
            index: Index::new(),
            docs: Vec::new(),
            built_for: None,
            indexed: 0,
            truncated: false,
        }
    }

    /// How many items the last build actually indexed.
    pub fn indexed(&self) -> usize {
        self.indexed
    }

    /// Whether the board was larger than [`MAX_INDEXED_ITEMS`] and has been searched only in
    /// part.
    ///
    /// A caller that does not surface this is showing partial results as though they were
    /// complete, which is the failure this constant exists to make visible rather than to
    /// hide.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Every item matching `query`, best first, at most [`MAX_RESULTS`] of them.
    ///
    /// An empty or whitespace-only query answers empty **without touching the index**, so
    /// the cost of a find bar that is open and blank is one `trim`.
    ///
    /// # Panic safety
    ///
    /// This takes a string a person typed and text out of somebody else's board, on a target
    /// where `panic = "abort"` kills the tab with no error. Nothing here indexes, slices or
    /// unwraps: the query goes to [`Query::parse`] whole, the only truncation is
    /// [`MAX_LABEL_CHARS`] and it counts characters, and every lookup is a `get` or a
    /// `parse` whose `None` is handled.
    ///
    /// The excerpt comes from [`vellum_search::Snippet`], and that was **verified rather
    /// than assumed**: `snippet::build` clamps every incoming range with
    /// `floor_boundary`/`ceil_boundary`, windows with `char_indices()` rather than byte
    /// arithmetic, and carries tests for a multi-byte window, a range landing mid-character,
    /// an inverted range and an out-of-range one.
    pub fn search(&mut self, projection: &Projection, query: &str) -> Vec<Match> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        if self.built_for != Some(projection.generation()) {
            self.rebuild(projection);
        }

        let ask = MAX_RESULTS.saturating_mul(OVERSAMPLE).min(MAX_RAW_HITS);
        let parsed = Query::parse(query).with_limit(ask);
        let mut hits = self.index.search(&parsed);
        // The "did you mean" pass. Deliberately conditional: running it always would let a
        // two-edit guess crowd out a word that is genuinely on the board, and it would cost
        // a dictionary walk on every keystroke that was working perfectly well.
        if hits.is_empty() {
            hits = self.index.search(&parsed.with_matching(Matching::forgiving()));
        }

        let mut out: Vec<Match> = Vec::new();
        let mut seen: HashSet<SceneId> = HashSet::new();
        for hit in hits {
            // Both of these answer `None` rather than panicking, and both are reachable:
            // an id this module did not write, and a stale id after a rebuild.
            let Ok(doc) = hit.item.parse::<usize>() else { continue };
            let Some(&id) = self.docs.get(doc) else { continue };
            // The first hit for an item is its best one, because the index has already
            // ordered them — so keeping the first and dropping the rest keeps the best
            // label rather than an arbitrary one.
            if !seen.insert(id) {
                continue;
            }
            let excerpt = hit.snippet.render("", "");
            let where_ = describe(projection, id);
            out.push(Match {
                item: id,
                excerpt: if excerpt.trim().is_empty() { where_.clone() } else { excerpt },
                where_,
            });
            if out.len() >= MAX_RESULTS {
                break;
            }
        }
        out
    }

    /// Throws the index away and builds it from the projection as it stands.
    ///
    /// Called only from [`Self::search`], and only when the generation has moved. It is
    /// private for that reason: an eager caller would pay for a board the user is only
    /// looking at.
    fn rebuild(&mut self, projection: &Projection) {
        self.docs.clear();
        self.truncated = false;

        // ⚠ `Projection::iter` walks a `HashMap`, so its order is not stable between runs.
        // Sorting by paint order first is what makes the truncation deterministic — which
        // items get dropped past `MAX_INDEXED_ITEMS` must not depend on hash seeding — and
        // it costs one small allocation on a path that runs once per board version.
        let mut order: Vec<(i32, SceneId)> =
            projection.iter().map(|(id, projected)| (projected.z, *id)).collect();
        order.sort_unstable();

        let mut items: Vec<Item> = Vec::new();
        let mut indexed = 0usize;
        for (_, id) in order {
            if indexed >= MAX_INDEXED_ITEMS {
                self.truncated = true;
                break;
            }
            let Some(projected) = projection.get(id) else { continue };
            indexed += 1;

            let kind = kind_tag(&projected.item.kind);
            let colour = colour_of(&projected.item.kind, &projected.item.style);
            let mut wrote = false;
            for label in labels(&projected.item.kind).into_iter().take(MAX_LABELS_PER_ITEM) {
                let text = clip(&label, MAX_LABEL_CHARS);
                if text.trim().is_empty() {
                    continue;
                }
                items.push(document(&mut self.docs, id, kind, text, colour));
                wrote = true;
            }
            // An item with no words still gets one empty document, so a facet query — `every
            // frame`, `all yellow stickies` — can find something that has nothing to read.
            if !wrote {
                items.push(document(&mut self.docs, id, kind, String::new(), colour));
            }
        }

        self.index = Index::build(items);
        self.indexed = indexed;
        self.built_for = Some(projection.generation());
    }
}

/// The rectangle the camera should be pointed at to show `item`, in world units.
///
/// `None` for an item that is not in the projection — a match held across a board version in
/// which it was deleted, which a find bar that keeps its results across a sync will hit.
///
/// This answers *where*, not *how far in*. Apply [`FOCUS_MAX_ZOOM`] afterwards, as the
/// desktop does: fitting one sticky fills the window with it, which is technically correct
/// and useless. And pass [`FOCUS_MARGIN`] — a fraction — to `Camera::fit_to_rect`, never a
/// pixel count.
pub fn focus(projection: &Projection, item: SceneId) -> Option<WorldRect> {
    projection.get(item).map(|projected| projected.bounds)
}

/// Pushes one document and records which item it belongs to, keeping the two in step.
///
/// The id **is** the position in `docs`, so these two writes cannot be separated; that is
/// the whole reason this is a function rather than two statements at the call site.
fn document(
    docs: &mut Vec<SceneId>,
    id: SceneId,
    kind: &str,
    text: String,
    colour: Option<Colour>,
) -> Item {
    let doc = docs.len();
    docs.push(id);
    let item = Item::new(BOARD, doc.to_string(), kind, text);
    match colour {
        Some(colour) => item.with_colour(colour),
        None => item,
    }
}

/// ⚠ **The one call out of this file, and the one line that has to be re-pointed if the
/// decoder moves.**
///
/// `vellum-app`'s `words::of` is the decoder for every item's text, including the four
/// structured widgets whose words live inside a JSON token `vellum-doc` deliberately cannot
/// parse. It reaches six `vellum-app` modules, so it cannot be called from here as it
/// stands, and it must not be copied: this crate's `widgets.rs` already holds a second
/// reader of those same four tokens, and a third would be the copy that drifts.
///
/// The contract this file depends on is one function and one signature:
///
/// ```ignore
/// pub fn of(kind: &vellum_doc::ItemKind) -> Vec<String>
/// ```
///
/// Everything else about the split — which crate it lands in, what the module is called — is
/// the integrator's, and re-pointing this is a one-line edit by design.
fn labels(kind: &ItemKind) -> Vec<String> {
    vellum_project::words::of(kind)
}

/// The kind tag the index files an item under, and the word a person types to ask for it.
///
/// These are the strings `vellum_search::resolve_kind` matches a bare query word against,
/// through `depluralise` — so `stickies` finds `sticky` and `frames` finds `frame` with
/// nothing further to wire up.
///
/// ⚠ **Exhaustive on purpose.** A new `ItemKind` should stop the build here rather than
/// quietly file itself under a catch-all as something it is not; an item indexed under the
/// wrong kind is invisible to the facet search that would have found it.
fn kind_tag(kind: &ItemKind) -> &'static str {
    match kind {
        ItemKind::Sticky { .. } => "sticky",
        ItemKind::Text { .. } => "text",
        ItemKind::Ink { .. } => "ink",
        ItemKind::Image { .. } => "image",
        ItemKind::LinkPreview { .. } => "link",
        ItemKind::Embed { .. } => "embed",
        ItemKind::Connector { .. } => "connector",
        ItemKind::Frame { .. } => "frame",
        ItemKind::Shape { .. } => "shape",
        ItemKind::Table { .. } => "table",
        ItemKind::Chart { .. } => "chart",
        ItemKind::MindMap { .. } => "mindmap",
        ItemKind::Kanban { .. } => "kanban",
        ItemKind::Group => "group",
        ItemKind::Agent { .. } => "agent",
        ItemKind::FileTree { .. } => "filetree",
        ItemKind::AgentNote { .. } => "note",
        ItemKind::Browser { .. } => "browser",
        ItemKind::Document { .. } => "document",
    }
}

/// What a result row calls the kind — the tag above, in the words a person reads.
fn kind_name(kind: &ItemKind) -> &'static str {
    match kind {
        ItemKind::Sticky { .. } => "Sticky note",
        ItemKind::Text { .. } => "Text",
        ItemKind::Ink { .. } => "Pen stroke",
        ItemKind::Image { .. } => "Image",
        ItemKind::LinkPreview { .. } => "Link card",
        ItemKind::Embed { .. } => "Embed",
        ItemKind::Connector { .. } => "Connector",
        ItemKind::Frame { .. } => "Frame",
        ItemKind::Shape { .. } => "Shape",
        ItemKind::Table { .. } => "Table",
        ItemKind::Chart { .. } => "Chart",
        ItemKind::MindMap { .. } => "Mind map",
        ItemKind::Kanban { .. } => "Kanban board",
        ItemKind::Group => "Group",
        ItemKind::Agent { .. } => "Agent",
        ItemKind::FileTree { .. } => "File tree",
        ItemKind::AgentNote { .. } => "Note",
        ItemKind::Browser { .. } => "Browser",
        ItemKind::Document { .. } => "Document",
    }
}

/// The colour the index files an item under, so `all yellow stickies` works.
///
/// A sticky's colour is on the *kind*, not on the style — `Style::fill`'s own doc comment
/// says why: in Miro the colour packs **are** the widget. So a sticky is asked first and
/// everything else falls back to its fill, which is where a frame and a shape keep theirs.
///
/// Alpha is dropped, because [`Colour`] is `0xRRGGBB`. A colour name is a hue, and a
/// half-transparent yellow is still what a person would call yellow.
///
/// Takes the two fields it reads rather than the whole `vellum_doc::Item`, which is not
/// constructible in a test: an `ItemId` wraps a Loro `TreeID` and has no `Default`, so a
/// signature over the item would have made this the one function here nothing could check.
fn colour_of(kind: &ItemKind, style: &vellum_doc::Style) -> Option<Colour> {
    let colour = match kind {
        ItemKind::Sticky { background, .. } => background.or(style.fill),
        _ => style.fill,
    }?;
    Some(Colour::from_rgb(colour.r, colour.g, colour.b))
}

/// `text`, cut to at most `limit` **characters**.
///
/// ⚠ Characters, never bytes. `&text[..limit]` is the panic that has shipped in this
/// repository twice on real board text, and on this target it aborts the tab rather than
/// unwinding. `chars().take(n)` cannot land off a boundary because it never computes one.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        // A byte length at or under the limit is a character count at or under it too, so
        // this is exact rather than approximate — and it is the case nearly every label
        // takes, which keeps the walk off the common path.
        return text.to_owned();
    }
    text.chars().take(limit).collect()
}

/// Where the item is, for a result row: its kind, and the frame it sits on if it sits on one.
///
/// The frame is worth the walk because it is how a person describes a location on a board —
/// *"the sticky in Engine bay"* — and it is the one piece of context a bare excerpt cannot
/// supply. The walk is over `Projected::parent`, which the projection has already resolved,
/// so this costs a hash lookup per hop and is bounded at [`MAX_ANCESTOR_HOPS`].
fn describe(projection: &Projection, id: SceneId) -> String {
    let Some(projected) = projection.get(id) else { return String::new() };
    let name = kind_name(&projected.item.kind);

    let mut parent = projected.parent;
    for _ in 0..MAX_ANCESTOR_HOPS {
        let Some(at) = parent else { break };
        let Some(ancestor) = projection.get(at) else { break };
        if let ItemKind::Frame { title, .. } = &ancestor.item.kind {
            let title = clip(title.to_plain().trim(), FRAME_NAME_CHARS);
            if !title.is_empty() {
                return format!("{name} in {title}");
            }
            // A frame with no name is still the innermost frame, so the walk stops here
            // rather than attributing the item to the frame above it.
            break;
        }
        parent = ancestor.parent;
    }
    name.to_owned()
}

/// How much of a frame's name a result row carries. A frame title is styled text and can be
/// a paragraph.
const FRAME_NAME_CHARS: usize = 40;

// ⚠ These cannot run in this crate. `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so a
// native `cargo test` compiles the whole crate away and reports success having run none of
// them. They are written against the pure half of this module and begin running the moment
// it moves to `vellum-project`, which is the recommendation in the report — see the module
// docs.
#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{Color, Style, StyledText};

    /// The bug per-label indexing exists to prevent, as a test rather than a comment.
    /// Two labels concatenated are adjacent token positions, so a phrase query would match
    /// straight across the seam between them — a column called *To do* followed by a card
    /// called *Measure the atlas* would answer to a search for a phrase on neither.
    #[test]
    fn a_phrase_cannot_match_across_two_labels() {
        let mut docs = Vec::new();
        let items: Vec<Item> = ["To do", "Measure the atlas"]
            .into_iter()
            .map(|label| document(&mut docs, 7, "kanban", label.to_owned(), None))
            .collect();
        let index = Index::build(items);

        assert!(
            index.search(&Query::parse("\"do measure\"")).is_empty(),
            "that phrase is on neither label; joining them would invent it"
        );
        assert!(
            !index.search(&Query::parse("\"measure the\"")).is_empty(),
            "a phrase inside one label must still be found"
        );
    }

    /// Both documents above point at the same item, so one search must not report it twice.
    /// This is the half of the per-label decision the caller sees.
    #[test]
    fn two_labels_of_one_item_are_one_document_each_and_one_item_out() {
        let mut docs = Vec::new();
        let items: Vec<Item> = ["Measure the atlas", "Measure the sill"]
            .into_iter()
            .map(|label| document(&mut docs, 7, "kanban", label.to_owned(), None))
            .collect();
        assert_eq!(docs, vec![7, 7], "one entry per label, both naming the same item");

        let index = Index::build(items);
        let hits = index.search(&Query::parse("measure"));
        assert_eq!(hits.len(), 2, "the index answers per label");

        let unique: HashSet<SceneId> =
            hits.iter().filter_map(|hit| hit.item.parse::<usize>().ok()).filter_map(|d| docs.get(d).copied()).collect();
        assert_eq!(unique.len(), 1, "and the caller reduces them to one item");
    }

    /// A label longer than the cap is cut on a character, not on a byte. `&text[..n]` here
    /// does not return an error, it aborts the tab.
    #[test]
    fn a_long_multi_byte_label_is_cut_on_a_character() {
        let text = "\u{5927}".repeat(MAX_LABEL_CHARS * 2);
        let clipped = clip(&text, MAX_LABEL_CHARS);
        assert_eq!(clipped.chars().count(), MAX_LABEL_CHARS);
        assert!(text.starts_with(&clipped), "and it is a prefix of what it cut");
    }

    /// The short path returns the label whole, and the walking path lands on a boundary.
    #[test]
    fn a_short_label_is_untouched_and_a_cut_one_stays_valid() {
        assert_eq!(clip("Cooling fan relay", MAX_LABEL_CHARS), "Cooling fan relay");
        // 4 bytes would land inside the two-byte `u`-umlaut; 4 characters does not.
        assert_eq!(clip("k\u{fc}hlung", 4), "k\u{fc}hl");
    }

    /// A kind's tag is what a person types and its name is what they read, and neither may
    /// be empty — an item filed under `""` is one the facet search can never reach.
    #[test]
    fn every_kind_carries_a_tag_and_a_name() {
        let kinds = [
            ItemKind::Sticky { text: StyledText::plain("a"), background: None },
            ItemKind::Text { text: StyledText::plain("a") },
            ItemKind::Frame { title: StyledText::plain("Engine bay"), order: None, speaker_notes: None },
            ItemKind::Group,
            ItemKind::FileTree { model: String::new() },
        ];
        for kind in &kinds {
            assert!(!kind_tag(kind).is_empty(), "{kind:?}");
            assert!(!kind_name(kind).is_empty(), "{kind:?}");
            // The tag is what `depluralise` and `resolve_kind` fold a typed word onto, so it
            // has to be one lowercase word with nothing to trim.
            assert_eq!(kind_tag(kind), kind_tag(kind).trim().to_lowercase());
        }
    }

    /// A sticky keeps its colour on the *kind* and everything else on the *style*. Reading
    /// only one of the two loses half the board's colours, and `all yellow stickies` — the
    /// query this exists for — is exactly the half that would go.
    #[test]
    fn a_stickys_colour_comes_off_the_kind_and_a_frames_off_the_style() {
        let yellow = Color::rgb(0xFF, 0xF7, 0x9E);
        let note = ItemKind::Sticky { text: StyledText::plain("Water pump"), background: Some(yellow) };
        assert_eq!(colour_of(&note, &Style::default()), Some(Colour::from_rgb(0xFF, 0xF7, 0x9E)));

        let frame = ItemKind::Frame { title: StyledText::plain("Engine bay"), order: None, speaker_notes: None };
        let style = Style { fill: Some(Color::rgb(0x00, 0xA3, 0x8C)), ..Style::default() };
        assert_eq!(colour_of(&frame, &style), Some(Colour::from_rgb(0x00, 0xA3, 0x8C)));

        // No colour anywhere is `None`, not black. Black is a colour somebody chose.
        assert_eq!(colour_of(&frame, &Style::default()), None);
    }

    /// The second pass, and the condition on it. `Matching::interactive()` is `fuzzy: 0`, so
    /// without the retry a typo answers nothing at all — and with an *unconditional* retry a
    /// two-edit guess would rank alongside a word that is genuinely on the board.
    #[test]
    fn a_typo_is_only_forgiven_when_nothing_matched() {
        let mut docs = Vec::new();
        let items: Vec<Item> = ["Coolant reservoir", "Cooling fan relay"]
            .into_iter()
            .map(|label| document(&mut docs, 1, "sticky", label.to_owned(), None))
            .collect();
        let index = Index::build(items);

        let typo = Query::parse("coolent");
        assert!(index.search(&typo).is_empty(), "the default pass does not guess");
        assert!(
            !index.search(&typo.with_matching(Matching::forgiving())).is_empty(),
            "and the retry does"
        );

        // A word that is really there is answered by the first pass, so the retry never runs.
        assert!(!index.search(&Query::parse("cool")).is_empty(), "a prefix needs no forgiveness");
    }

    /// An item with no words still has to reach the index, or a facet query cannot find an
    /// image, a pen stroke or an empty frame — which is most of what is on a real board.
    #[test]
    fn a_wordless_item_is_still_filed_under_its_kind() {
        let mut docs = Vec::new();
        let index = Index::build(vec![document(&mut docs, 3, "image", String::new(), None)]);
        let hits = index.search(&Query::parse("every image"));
        assert_eq!(hits.len(), 1, "a facet query has to reach an item with nothing to read");
        assert_eq!(hits[0].item, "0");
    }
}
