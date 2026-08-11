//! The input contract: the least a document can say and still be searchable.
//!
//! # Why a trait, and why not `vellum_doc::Board`
//!
//! `vellum-search` does not depend on `vellum-doc`. The reasons, in order of
//! weight:
//!
//! 1. **The index outlives any one document.** Cross-board search covers 20–100
//!    boards; loading a hundred Loro CRDTs to answer a keystroke is precisely the
//!    behaviour this app exists to avoid. The index stores everything a hit needs —
//!    ids, kind, colour, tags, and the text itself — so a search touches no board
//!    file at all, and only the board the user actually jumps into is opened.
//! 2. **Indexing is a pure function of a snapshot.** A `Board` is a live CRDT with
//!    undo state and history. Taking a narrow view of it means an index can equally
//!    be built from a decoded Miro clipboard payload before it has ever become a
//!    document, from the board library's SQLite row, or from a test fixture.
//! 3. **The dependency graph stays acyclic and shallow**, per
//!    `docs/01-architecture.md` §2. A search crate reaching back into the document
//!    layer would be the first exception to "strictly downward".
//!
//! # How a `Board` maps onto it
//!
//! Written out here so it does not have to be rediscovered when the document layer
//! wires it up. Every row is a field rename; nothing needs computing.
//!
//! | Board concept | Becomes |
//! |---|---|
//! | the board's id in the library index (`vellum-store`) | [`SearchItem::board`] |
//! | `vellum_doc::ItemId`, via its `Display` (`counter@peer`) | [`SearchItem::item`] |
//! | `ItemKind::tag()` — `"sticky"`, `"frame"`, `"ink"`, … | [`SearchItem::kind`] |
//! | `ItemKind::text()` → `StyledText`, flattened to plain text | [`SearchItem::text`] |
//! | a frame's `title` | that frame's [`SearchItem::text`] |
//! | `Sticky { background }`, `Frame { background }`, a shape's fill | [`SearchItem::colour`] |
//! | the item's tag set (`docs/features/README.md` §1, "Tags") | [`SearchItem::tags`] |
//! | an item with no text at all — ink, an image | still indexed; facets only |
//!
//! Two mappings are worth calling out because getting them wrong is invisible:
//!
//! - **Styled spans flatten to plain text, and the span structure is discarded.**
//!   `docs/01-architecture.md` §4 stores text as spans, never a `String`; searching
//!   is over the concatenated characters, so a term split across a bold boundary
//!   (`**cool**ing`) is one term, which is what the user sees on the canvas.
//! - **An item with no text is still indexed.** 219 of the reference board's 596
//!   widgets are ink strokes with nothing to read, and "every drawing on this
//!   board" is a query somebody will type. An empty [`SearchItem::text`] costs one
//!   document record and no postings.
//!
//! # Kinds are strings, not an enum
//!
//! [`SearchItem::kind`] returns `&str` rather than a closed enum, because a closed
//! enum here would have to be kept in lockstep with `vellum_doc::ItemKind`,
//! `vellum_import::WidgetKind` and `vellum_export::Kind` — four places to edit to
//! add a widget, three of which are owned by someone else. The strings are the tags
//! those types already produce and already promise to keep stable, so a new widget
//! kind becomes searchable with no change to this crate at all.
//!
//! The cost is that a typo in a kind string is a silently unmatched filter rather
//! than a compile error. [`Index::kinds`](crate::Index::kinds) exists so a UI can
//! offer the kinds that are genuinely present instead of a hardcoded list.

use crate::colour::Colour;

/// Everything the index needs from one item.
///
/// The first four methods are the whole contract; the last two have defaults
/// because a caller that has neither colours nor tags should not have to say so.
pub trait SearchItem {
    /// The board this item lives on. Opaque to this crate, and returned verbatim in
    /// a [`Hit`](crate::Hit) so the caller can open it.
    fn board(&self) -> &str;

    /// The item's id within its board. `(board, item)` is the index's primary key:
    /// indexing the same pair twice replaces the first record rather than
    /// duplicating it.
    fn item(&self) -> &str;

    /// The item's readable text, or `""`. See the module docs on flattening.
    fn text(&self) -> &str;

    /// A stable lowercase discriminant — `"sticky"`, `"frame"`, `"ink"`. Matches
    /// `vellum_doc::ItemKind::tag`.
    fn kind(&self) -> &str;

    /// The item's dominant colour: a sticky's background, a shape's fill, a frame's
    /// background. `None` means "no colour worth searching by", which is *not* the
    /// same as white — an unstyled item must not turn up in `colour:white`.
    fn colour(&self) -> Option<Colour> {
        None
    }

    /// The item's tags. Order is irrelevant; duplicates are collapsed.
    fn tags(&self) -> &[String] {
        &[]
    }
}

/// A plain owned item, for callers with nothing better to hand.
///
/// This is what the tests, the benchmark and the importer use. It is deliberately
/// not the trait's only implementation — a document that already holds this data
/// should implement [`SearchItem`] on its own type and copy nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    pub board: String,
    pub item: String,
    pub kind: String,
    pub text: String,
    pub colour: Option<Colour>,
    pub tags: Vec<String>,
}

impl Item {
    /// The four required fields. Colour and tags are added with the builders below.
    pub fn new(
        board: impl Into<String>,
        item: impl Into<String>,
        kind: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            board: board.into(),
            item: item.into(),
            kind: kind.into(),
            text: text.into(),
            colour: None,
            tags: Vec::new(),
        }
    }

    pub fn with_colour(mut self, colour: Colour) -> Self {
        self.colour = Some(colour);
        self
    }

    pub fn with_tags<S: Into<String>>(mut self, tags: impl IntoIterator<Item = S>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }
}

impl SearchItem for Item {
    fn board(&self) -> &str {
        &self.board
    }

    fn item(&self) -> &str {
        &self.item
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    fn colour(&self) -> Option<Colour> {
        self.colour
    }

    fn tags(&self) -> &[String] {
        &self.tags
    }
}

impl<T: SearchItem + ?Sized> SearchItem for &T {
    fn board(&self) -> &str {
        (**self).board()
    }

    fn item(&self) -> &str {
        (**self).item()
    }

    fn text(&self) -> &str {
        (**self).text()
    }

    fn kind(&self) -> &str {
        (**self).kind()
    }

    fn colour(&self) -> Option<Colour> {
        (**self).colour()
    }

    fn tags(&self) -> &[String] {
        (**self).tags()
    }
}

/// Kind names that mean the same thing as one of the canonical tags.
///
/// People do not type schema discriminants. They type "notes", "pictures",
/// "arrows", "drawings" — and, above all, plurals. The table below is consulted
/// only after [`depluralise`] has failed to produce a kind that is actually present
/// in the index, so it holds genuine synonyms rather than morphology.
///
/// Entries map **query word → canonical tag**. A synonym for a kind that no board
/// in the index contains simply never matches; nothing here asserts a kind exists.
const KIND_SYNONYMS: &[(&str, &str)] = &[
    ("arrow", "connector"),
    ("card", "link_preview"),
    ("drawing", "ink"),
    ("group", "group"),
    ("iframe", "embed"),
    ("image", "image"),
    ("line", "connector"),
    ("link", "link_preview"),
    ("note", "sticky"),
    ("path", "ink"),
    ("pdf", "document"),
    ("pen", "ink"),
    ("photo", "image"),
    ("picture", "image"),
    ("preview", "link_preview"),
    ("sticker", "sticky"),
    ("stickie", "sticky"),
    ("stroke", "ink"),
    ("video", "embed"),
];

/// English plural → singular, for the shapes that actually occur in kind names.
///
/// Not a general stemmer, and deliberately so: a stemmer would also fold `frames`
/// and `framing` together, and `framing` is a word somebody might have written on a
/// sticky. This does one job — undoing the `s` a person adds when they mean "all of
/// them" — and it is applied only when the result names a kind that exists.
pub fn depluralise(word: &str) -> Option<String> {
    // "stickies" -> "sticky", "copies" -> "copy".
    if let Some(stem) = word.strip_suffix("ies")
        && !stem.is_empty()
    {
        return Some(format!("{stem}y"));
    }
    for suffix in ["ches", "shes", "xes", "zes", "ses"] {
        if let Some(stem) = word.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return Some(format!("{stem}{}", &suffix[..suffix.len() - 2]));
        }
    }
    word.strip_suffix('s').filter(|stem| !stem.is_empty()).map(str::to_owned)
}

/// Resolves a query word onto a kind tag that `known` reports as present.
///
/// The order is exact, then depluralised, then synonym, then depluralised synonym.
/// `known` is asked at each step rather than a fixed vocabulary being assumed, so a
/// widget kind this crate has never heard of is still searchable by its own name
/// and its own plural.
pub fn resolve_kind(word: &str, known: impl Fn(&str) -> bool) -> Option<String> {
    let word = word.to_ascii_lowercase();
    if known(&word) {
        return Some(word);
    }
    // "link previews" and "link_previews" are both reasonable to type; the query
    // parser has already folded a quoted `kind:"link preview"` to one word, but a
    // bare term arrives as written.
    let singular = depluralise(&word);
    if let Some(singular) = &singular
        && known(singular)
    {
        return Some(singular.clone());
    }
    let synonym = |w: &str| KIND_SYNONYMS.iter().find(|(from, _)| *from == w).map(|(_, to)| *to);
    if let Some(tag) = synonym(&word)
        && known(tag)
    {
        return Some(tag.to_owned());
    }
    if let Some(singular) = &singular
        && let Some(tag) = synonym(singular)
        && known(tag)
    {
        return Some(tag.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present<'a>(kinds: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |k| kinds.contains(&k)
    }

    #[test]
    fn depluralisation_handles_the_shapes_kind_names_take() {
        assert_eq!(depluralise("frames").as_deref(), Some("frame"));
        assert_eq!(depluralise("stickies").as_deref(), Some("sticky"));
        assert_eq!(depluralise("boxes").as_deref(), Some("box"));
        assert_eq!(depluralise("sketches").as_deref(), Some("sketch"));
        assert_eq!(depluralise("frame"), None, "already singular");
        assert_eq!(depluralise("s"), None, "not a plural of anything");
        assert_eq!(depluralise(""), None);
    }

    #[test]
    fn a_kind_resolves_from_its_plural_and_from_everyday_words() {
        let kinds = ["sticky", "frame", "ink", "image", "connector", "link_preview"];
        let known = present(&kinds);
        assert_eq!(resolve_kind("frames", &known).as_deref(), Some("frame"));
        assert_eq!(resolve_kind("stickies", &known).as_deref(), Some("sticky"));
        assert_eq!(resolve_kind("notes", &known).as_deref(), Some("sticky"));
        assert_eq!(resolve_kind("Drawings", &known).as_deref(), Some("ink"));
        assert_eq!(resolve_kind("arrows", &known).as_deref(), Some("connector"));
        assert_eq!(resolve_kind("photos", &known).as_deref(), Some("image"));
    }

    /// The synonym table must never invent a kind the index does not hold, or a
    /// search for "cards" on a board with no cards would silently return every link
    /// preview under a name the user did not mean.
    #[test]
    fn a_synonym_for_an_absent_kind_resolves_to_nothing() {
        let known = present(&["sticky"]);
        assert_eq!(resolve_kind("arrows", &known), None);
        assert_eq!(resolve_kind("photos", &known), None);
        assert_eq!(resolve_kind("cooling", &known), None);
    }

    /// A kind this crate has never heard of is still searchable by its own name.
    #[test]
    fn an_unknown_kind_is_reachable_without_a_synonym_entry() {
        let known = present(&["mindmap_node"]);
        assert_eq!(resolve_kind("mindmap_node", &known).as_deref(), Some("mindmap_node"));
        assert_eq!(resolve_kind("mindmap_nodes", &known).as_deref(), Some("mindmap_node"));
    }

    #[test]
    fn the_synonym_table_is_sorted_and_free_of_duplicate_sources() {
        assert!(KIND_SYNONYMS.windows(2).all(|w| w[0].0 < w[1].0), "kept sorted for review");
    }

    #[test]
    fn the_default_trait_methods_cover_a_minimal_implementation() {
        struct Minimal;
        impl SearchItem for Minimal {
            fn board(&self) -> &str {
                "b"
            }
            fn item(&self) -> &str {
                "i"
            }
            fn text(&self) -> &str {
                "hello"
            }
            fn kind(&self) -> &str {
                "text"
            }
        }
        assert_eq!(Minimal.colour(), None);
        assert!(Minimal.tags().is_empty());
    }
}
