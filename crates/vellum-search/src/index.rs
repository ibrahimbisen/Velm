//! The index itself: documents, postings, facets, and the incremental update path.
//!
//! # One item changing must cost one item
//!
//! This is the requirement the whole structure is arranged around. The user is
//! importing 20–100 Miro boards, one of which alone holds 596 widgets and 429
//! distinct text strings; if editing a sticky meant re-tokenising every board, the
//! index would be rebuilt on every keystroke of every edit and the feature would be
//! unusable within a day of real work.
//!
//! So the write path is exact rather than approximate. Every structure that an item
//! contributes to can be *un*-contributed to:
//!
//! - The document record keeps the item's text, so removing it needs no cooperation
//!   from the caller — [`Index::remove`] takes only the ids.
//! - Posting lists are kept sorted by document id, so inserting or deleting one
//!   document is a binary search and a memmove, not a scan.
//! - A term whose last posting goes away is dropped from the dictionary, so a word
//!   deleted from the last item that used it stops costing every later fuzzy query.
//! - Facet lists are the same shape, so a sticky changing colour moves one id
//!   between two vectors.
//!
//! [`Index::upsert`] on an item that has not actually changed does none of this and
//! returns [`Update::Unchanged`]. That matters more than it looks: the natural way
//! to wire a CRDT up to an index is to re-push every item in a changed subtree, and
//! most of them will be identical.
//!
//! # The index holds the text, and that is the point
//!
//! A document record carries the item's text, not a pointer back to the board it
//! came from. It costs about a megabyte for the whole 100-board corpus, and it buys
//! the property that makes cross-board search viable at all: **answering a query,
//! ranking it and rendering its snippets opens no board files.** Only the board the
//! user actually jumps into is loaded. The alternative — storing offsets and
//! reading text back out of documents at render time — would mean touching up to
//! 100 SQLite files and 100 Loro snapshots to draw one result panel.
//!
//! # Facets are not derived from the text
//!
//! Kind, colour and tag get their own posting lists rather than being folded into
//! the term index as magic tokens like `__kind_sticky`. Two reasons: a magic token
//! is reachable by an ordinary text query, so a sticky whose text happened to say
//! `__kind_sticky` would be indistinguishable from one that is; and a filter needs
//! *set* semantics, not *scored* semantics — `kind:frame` is not a relevance signal
//! that competes with the words, it is a constraint that removes everything else.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::colour::Colour;
use crate::dictionary::{Dictionary, TermId};
use crate::source::SearchItem;
use crate::token;

/// A document's slot in this index.
///
/// Internal and unstable across a save/load cycle: [`crate::persist`] renumbers
/// documents densely when it writes, which is what keeps a file from carrying the
/// holes left by a session's worth of deletions. Nothing outside the crate ever
/// sees one; a [`Hit`](crate::Hit) carries the caller's own ids.
pub(crate) type DocId = u32;

/// Everything the index stores about one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Doc {
    pub board: Box<str>,
    pub item: Box<str>,
    pub kind: Box<str>,
    pub text: Box<str>,
    pub colour: Option<Colour>,
    pub tags: Vec<Box<str>>,
}

impl Doc {
    fn of<I: SearchItem + ?Sized>(item: &I) -> Self {
        // Tags are sorted and deduplicated on the way in so that two items with the
        // same tags in different orders compare equal, which is what makes
        // `Update::Unchanged` reliable rather than order-dependent.
        let mut tags: Vec<Box<str>> = item.tags().iter().map(|tag| tag.as_str().into()).collect();
        tags.sort_unstable();
        tags.dedup();
        Self {
            board: item.board().into(),
            item: item.item().into(),
            kind: item.kind().into(),
            text: item.text().into(),
            colour: item.colour(),
            tags,
        }
    }
}

/// One term's occurrences in one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Posting {
    pub doc: DocId,
    /// Token ordinals, ascending. Phrase matching compares these directly, and
    /// ranking takes the first.
    pub positions: Vec<u32>,
}

/// A term's documents, kept sorted by [`DocId`].
///
/// Sorted rather than hashed because both things done with a posting list want
/// order: an incremental edit finds its document by binary search, and a query
/// walks lists in the same order so that accumulating across terms is a linear
/// sweep over a dense scratch buffer rather than a hash lookup per posting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PostingList {
    entries: Vec<Posting>,
}

impl PostingList {
    pub fn entries(&self) -> &[Posting] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn positions(&self, doc: DocId) -> Option<&[u32]> {
        let at = self.entries.binary_search_by_key(&doc, |e| e.doc).ok()?;
        Some(&self.entries[at].positions)
    }

    fn insert(&mut self, doc: DocId, positions: Vec<u32>) {
        match self.entries.binary_search_by_key(&doc, |e| e.doc) {
            Ok(at) => self.entries[at].positions = positions,
            Err(at) => self.entries.insert(at, Posting { doc, positions }),
        }
    }

    fn remove(&mut self, doc: DocId) {
        if let Ok(at) = self.entries.binary_search_by_key(&doc, |e| e.doc) {
            self.entries.remove(at);
        }
    }

    pub(crate) fn from_sorted(entries: Vec<Posting>) -> Self {
        Self { entries }
    }
}

/// An attribute's inverted index: value → the documents carrying it.
///
/// A `BTreeMap` rather than a `HashMap` so that listing the values — which a UI
/// does to populate a filter menu — comes out in a stable order without a sort, and
/// so that nothing in this crate depends on hash iteration order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Facet {
    values: BTreeMap<Box<str>, Vec<DocId>>,
}

impl Facet {
    fn add(&mut self, value: &str, doc: DocId) {
        let docs = self.values.entry(value.into()).or_default();
        if let Err(at) = docs.binary_search(&doc) {
            docs.insert(at, doc);
        }
    }

    fn remove(&mut self, value: &str, doc: DocId) {
        let Some(docs) = self.values.get_mut(value) else {
            return;
        };
        if let Ok(at) = docs.binary_search(&doc) {
            docs.remove(at);
        }
        // A value with no documents is removed outright: it would otherwise show up
        // in a filter menu as a kind or colour the board no longer contains.
        if docs.is_empty() {
            self.values.remove(value);
        }
    }

    pub fn docs(&self, value: &str) -> &[DocId] {
        self.values.get(value).map_or(&[], Vec::as_slice)
    }

    pub fn values(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(|value| &**value)
    }

    pub fn contains(&self, value: &str) -> bool {
        self.values.contains_key(value)
    }
}

/// What [`Index::upsert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The `(board, item)` pair was not in the index.
    Inserted,
    /// It was, and something about it differed.
    Replaced,
    /// It was, and every indexed field was identical. No work was done.
    Unchanged,
}

/// Counts, for diagnostics and for the benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStats {
    pub documents: usize,
    pub boards: usize,
    /// Distinct terms in the dictionary.
    pub terms: usize,
    /// Total `(term, document)` pairs.
    pub postings: usize,
    /// Total token positions recorded.
    pub positions: usize,
}

/// A full-text and faceted index over items from any number of boards.
#[derive(Debug, Default)]
pub struct Index {
    docs: Vec<Option<Doc>>,
    free: Vec<DocId>,
    /// `board → item → doc`. Nested so that both levels can be looked up by `&str`
    /// without building a composite key on every query.
    lookup: HashMap<Box<str>, HashMap<Box<str>, DocId>>,
    dictionary: Dictionary,
    /// Indexed by [`TermId`], so it is as long as the dictionary's id space
    /// including tombstones.
    postings: Vec<PostingList>,
    boards: Facet,
    kinds: Facet,
    /// Keyed by `#rrggbb`.
    colours: Facet,
    /// Keyed by the names in [`crate::colour::COLOUR_NAMES`]. Separate from
    /// `colours` so that `colour:yellow` and `colour:#fff79e` are different lookups
    /// rather than one lookup with a guess about which the caller meant.
    colour_names: Facet,
    tags: Facet,
    live: usize,
    dirty: bool,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an index from an iterator of items.
    pub fn build<I: SearchItem>(items: impl IntoIterator<Item = I>) -> Self {
        let mut index = Self::new();
        index.extend(items);
        index
    }

    pub fn extend<I: SearchItem>(&mut self, items: impl IntoIterator<Item = I>) {
        for item in items {
            self.upsert(&item);
        }
    }

    /// Adds or replaces one item, keyed on `(board, item)`.
    ///
    /// See the module docs: this is `O(tokens in this item × log(documents per
    /// term))`, and touches nothing belonging to any other item.
    pub fn upsert<I: SearchItem + ?Sized>(&mut self, item: &I) -> Update {
        let doc = Doc::of(item);
        match self.id_of(&doc.board, &doc.item) {
            Some(id) => {
                if self.docs[id as usize].as_ref() == Some(&doc) {
                    return Update::Unchanged;
                }
                let previous = self.docs[id as usize].take().expect("id_of found it");
                self.unindex(id, &previous);
                self.index(id, doc);
                self.dirty = true;
                Update::Replaced
            }
            None => {
                let id = match self.free.pop() {
                    Some(id) => id,
                    None => {
                        self.docs.push(None);
                        (self.docs.len() - 1) as DocId
                    }
                };
                self.lookup
                    .entry(doc.board.clone())
                    .or_default()
                    .insert(doc.item.clone(), id);
                self.index(id, doc);
                self.live += 1;
                self.dirty = true;
                Update::Inserted
            }
        }
    }

    /// Removes one item. Returns whether it was there.
    pub fn remove(&mut self, board: &str, item: &str) -> bool {
        let Some(id) = self.id_of(board, item) else {
            return false;
        };
        let doc = self.docs[id as usize].take().expect("id_of found it");
        self.unindex(id, &doc);
        if let Some(items) = self.lookup.get_mut(board) {
            items.remove(item);
            if items.is_empty() {
                self.lookup.remove(board);
            }
        }
        self.free.push(id);
        self.live -= 1;
        self.dirty = true;
        true
    }

    /// Removes every item of one board — what happens when a board is deleted, and
    /// the first half of a re-import.
    ///
    /// Returns how many items went.
    pub fn remove_board(&mut self, board: &str) -> usize {
        let Some(items) = self.lookup.remove(board) else {
            return 0;
        };
        let count = items.len();
        for id in items.into_values() {
            let doc = self.docs[id as usize].take().expect("lookup holds live ids");
            self.unindex(id, &doc);
            self.free.push(id);
        }
        self.live -= count;
        self.dirty = true;
        count
    }

    /// Replaces a board's items wholesale, in one pass, without disturbing any
    /// other board.
    ///
    /// This is the re-import path, and it is not `remove_board` followed by
    /// `extend`: doing it that way would delete and re-create every posting for
    /// every item, including the overwhelming majority that did not change.
    /// Instead each incoming item goes through [`Index::upsert`] — so unchanged
    /// ones cost a comparison — and only the items the new revision no longer
    /// contains are removed.
    pub fn replace_board<I: SearchItem>(
        &mut self,
        board: &str,
        items: impl IntoIterator<Item = I>,
    ) {
        let mut stale: std::collections::HashSet<Box<str>> =
            self.lookup.get(board).map(|items| items.keys().cloned().collect()).unwrap_or_default();
        for item in items {
            debug_assert_eq!(item.board(), board, "item does not belong to the board given");
            stale.remove(item.item());
            self.upsert(&item);
        }
        for item in stale {
            self.remove(board, &item);
        }
    }

    /// Number of indexed items.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn contains(&self, board: &str, item: &str) -> bool {
        self.id_of(board, item).is_some()
    }

    /// Whether anything has changed since the last [`save`](Index::save) or
    /// [`load`](Index::load).
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Board ids present in the index, in sorted order.
    pub fn boards(&self) -> impl Iterator<Item = &str> {
        self.boards.values()
    }

    /// Item kinds present in the index, in sorted order. A UI should offer these
    /// rather than a hardcoded list — see [`crate::source`] on why kinds are
    /// strings.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.kinds.values()
    }

    /// Distinct colours present in the index, in ascending hex order.
    pub fn colours(&self) -> impl Iterator<Item = Colour> {
        self.colours.values().filter_map(Colour::parse)
    }

    /// Tags present in the index, in sorted order.
    pub fn tags(&self) -> impl Iterator<Item = &str> {
        self.tags.values()
    }

    pub fn stats(&self) -> IndexStats {
        let (postings, positions) = self.postings.iter().fold((0, 0), |(entries, positions), list| {
            (
                entries + list.len(),
                positions + list.entries().iter().map(|e| e.positions.len()).sum::<usize>(),
            )
        });
        IndexStats {
            documents: self.live,
            boards: self.lookup.len(),
            terms: self.dictionary.len(),
            postings,
            positions,
        }
    }

    fn id_of(&self, board: &str, item: &str) -> Option<DocId> {
        self.lookup.get(board)?.get(item).copied()
    }

    /// Writes `doc` into slot `id` and adds every structure it belongs to.
    fn index(&mut self, id: DocId, doc: Doc) {
        for (term, positions) in group_terms(&doc.text) {
            let term_id = self.dictionary.intern(&term);
            // Posting storage is indexed by term id, so it tracks the dictionary's
            // whole id space including the slots tombstoned terms left behind.
            if self.postings.len() < self.dictionary.capacity() {
                self.postings.resize_with(self.dictionary.capacity(), PostingList::default);
            }
            self.postings[term_id as usize].insert(id, positions);
        }
        self.boards.add(&doc.board, id);
        self.kinds.add(&doc.kind, id);
        if let Some(colour) = doc.colour {
            self.colours.add(&colour.to_hex(), id);
            for name in colour.names() {
                self.colour_names.add(name, id);
            }
        }
        for tag in &doc.tags {
            self.tags.add(tag, id);
        }
        self.docs[id as usize] = Some(doc);
    }

    /// The exact inverse of [`Index::index`]. `doc` must already have been taken
    /// out of its slot, so that the borrow checker enforces what the invariant
    /// requires anyway: the document's own text is the authority on what to remove,
    /// and it must not be read through `self` while `self` is being mutated.
    fn unindex(&mut self, id: DocId, doc: &Doc) {
        for (term, _) in group_terms(&doc.text) {
            let Some(term_id) = self.dictionary.lookup(&term) else {
                continue;
            };
            self.postings[term_id as usize].remove(id);
            if self.postings[term_id as usize].is_empty() {
                self.dictionary.remove(term_id);
            }
        }
        self.boards.remove(&doc.board, id);
        self.kinds.remove(&doc.kind, id);
        if let Some(colour) = doc.colour {
            self.colours.remove(&colour.to_hex(), id);
            for name in colour.names() {
                self.colour_names.remove(name, id);
            }
        }
        for tag in &doc.tags {
            self.tags.remove(tag, id);
        }
    }

    // Accessors used by `search` and `persist`, which are separate modules for
    // readability rather than separate components.

    pub(crate) fn doc(&self, id: DocId) -> Option<&Doc> {
        self.docs.get(id as usize)?.as_ref()
    }

    pub(crate) fn doc_slots(&self) -> usize {
        self.docs.len()
    }

    pub(crate) fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    pub(crate) fn postings_of(&self, term: TermId) -> &PostingList {
        static EMPTY: PostingList = PostingList { entries: Vec::new() };
        self.postings.get(term as usize).unwrap_or(&EMPTY)
    }

    pub(crate) fn facet_boards(&self) -> &Facet {
        &self.boards
    }

    pub(crate) fn facet_kinds(&self) -> &Facet {
        &self.kinds
    }

    pub(crate) fn facet_colours(&self) -> &Facet {
        &self.colours
    }

    pub(crate) fn facet_colour_names(&self) -> &Facet {
        &self.colour_names
    }

    pub(crate) fn facet_tags(&self) -> &Facet {
        &self.tags
    }

    /// Assembles an index from parts already in their final form.
    ///
    /// Only [`crate::persist`] calls this. Facets are rebuilt from the documents
    /// rather than stored in the file: they are entirely derivable, rebuilding them
    /// costs one pass over the document table with no tokenisation, and a file that
    /// stored them could disagree with the documents it also stored — a class of
    /// corruption with no way to detect it and no way to repair it.
    pub(crate) fn from_parts(
        docs: Vec<Doc>,
        dictionary: Dictionary,
        postings: Vec<PostingList>,
    ) -> Self {
        let mut index = Self {
            docs: Vec::with_capacity(docs.len()),
            free: Vec::new(),
            lookup: HashMap::with_capacity(docs.len()),
            dictionary,
            postings,
            ..Self::default()
        };
        for (id, doc) in docs.into_iter().enumerate() {
            let id = id as DocId;
            index.docs.push(None);
            index
                .lookup
                .entry(doc.board.clone())
                .or_default()
                .insert(doc.item.clone(), id);
            index.boards.add(&doc.board, id);
            index.kinds.add(&doc.kind, id);
            if let Some(colour) = doc.colour {
                index.colours.add(&colour.to_hex(), id);
                for name in colour.names() {
                    index.colour_names.add(name, id);
                }
            }
            for tag in &doc.tags {
                index.tags.add(tag, id);
            }
            index.live += 1;
            index.docs[id as usize] = Some(doc);
        }
        index
    }

    /// Live documents in id order, for [`crate::persist`].
    pub(crate) fn live_docs(&self) -> impl Iterator<Item = (DocId, &Doc)> {
        self.docs
            .iter()
            .enumerate()
            .filter_map(|(id, slot)| slot.as_ref().map(|doc| (id as DocId, doc)))
    }

    pub(crate) fn mark_clean(&mut self) {
        self.dirty = false;
    }
}

/// The distinct terms of `text` with their ascending positions.
///
/// Sorting and grouping rather than hashing: an item's text is a sticky's worth of
/// words, where a sort of a handful of elements beats building a hash map, and the
/// worst case — a long document widget — stays `n log n` instead of degrading on
/// hash collisions. It also makes the term order deterministic, which the tests
/// rely on.
fn group_terms(text: &str) -> Vec<(String, Vec<u32>)> {
    let mut tokens: Vec<(std::borrow::Cow<'_, str>, u32)> =
        token::tokenise(text).map(|token| (token.text, token.index)).collect();
    tokens.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut grouped: Vec<(String, Vec<u32>)> = Vec::new();
    for (term, position) in tokens {
        match grouped.last_mut() {
            Some((last, positions)) if *last == *term => positions.push(position),
            _ => grouped.push((term.into_owned(), vec![position])),
        }
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Item;

    fn sticky(board: &str, item: &str, text: &str) -> Item {
        Item::new(board, item, "sticky", text)
    }

    #[test]
    fn upserting_the_same_item_twice_does_no_work_the_second_time() {
        let mut index = Index::new();
        assert_eq!(index.upsert(&sticky("b", "1", "cooling fan")), Update::Inserted);
        assert_eq!(index.upsert(&sticky("b", "1", "cooling fan")), Update::Unchanged);
        assert_eq!(index.upsert(&sticky("b", "1", "cooling pump")), Update::Replaced);
        assert_eq!(index.len(), 1, "the pair (board, item) is the key");
    }

    #[test]
    fn tag_order_does_not_make_an_item_look_changed() {
        let mut index = Index::new();
        index.upsert(&sticky("b", "1", "x").with_tags(["review", "done"]));
        let update = index.upsert(&sticky("b", "1", "x").with_tags(["done", "review", "done"]));
        assert_eq!(update, Update::Unchanged);
    }

    /// The heart of the incremental requirement: editing one item must leave the
    /// index in exactly the state a full rebuild would have produced.
    #[test]
    fn an_edit_leaves_the_index_identical_to_a_rebuild() {
        let after = [
            sticky("b", "1", "cooling fan"),
            sticky("b", "2", "water pump"),
            sticky("b", "3", "cooling pump"),
        ];
        let mut edited = Index::build([
            sticky("b", "1", "cooling fan"),
            sticky("b", "2", "radiator hose"),
            sticky("b", "3", "cooling pump"),
        ]);
        edited.upsert(&after[1]);

        let rebuilt = Index::build(after);
        assert_eq!(edited.stats(), rebuilt.stats());
        for term in ["radiator", "hose"] {
            assert_eq!(edited.dictionary().lookup(term), None, "{term} is gone entirely");
        }
        for term in ["water", "pump", "cooling"] {
            assert!(edited.dictionary().lookup(term).is_some(), "{term} survives");
        }
    }

    #[test]
    fn a_term_used_by_no_remaining_item_leaves_the_dictionary() {
        let mut index = Index::build([sticky("b", "1", "unique"), sticky("b", "2", "shared")]);
        assert_eq!(index.stats().terms, 2);
        index.remove("b", "1");
        assert_eq!(index.stats().terms, 1);
        assert_eq!(index.dictionary().lookup("unique"), None);
        assert_eq!(index.stats().postings, 1);
    }

    #[test]
    fn removing_an_absent_item_is_not_an_error_and_changes_nothing() {
        let mut index = Index::build([sticky("b", "1", "x")]);
        assert!(!index.remove("b", "nope"));
        assert!(!index.remove("nope", "1"));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn removing_a_board_leaves_the_others_untouched() {
        let mut index = Index::build([
            sticky("a", "1", "cooling"),
            sticky("a", "2", "fan"),
            sticky("b", "1", "cooling"),
        ]);
        assert_eq!(index.remove_board("a"), 2);
        assert_eq!(index.len(), 1);
        assert_eq!(index.boards().collect::<Vec<_>>(), ["b"]);
        assert!(index.dictionary().lookup("cooling").is_some(), "board b still has it");
        assert_eq!(index.dictionary().lookup("fan"), None);
        assert_eq!(index.remove_board("a"), 0, "twice is a no-op");
    }

    #[test]
    fn replacing_a_board_adds_removes_and_leaves_the_unchanged_alone() {
        let mut index = Index::build([
            sticky("a", "1", "keep"),
            sticky("a", "2", "drop"),
            sticky("b", "1", "other"),
        ]);
        index.replace_board("a", [sticky("a", "1", "keep"), sticky("a", "3", "add")]);

        assert!(index.contains("a", "1"));
        assert!(!index.contains("a", "2"));
        assert!(index.contains("a", "3"));
        assert!(index.contains("b", "1"));
        assert_eq!(index.dictionary().lookup("drop"), None);
        assert_eq!(index.stats(), Index::build([
            sticky("a", "1", "keep"),
            sticky("a", "3", "add"),
            sticky("b", "1", "other"),
        ])
        .stats());
    }

    #[test]
    fn freed_document_slots_are_reused_rather_than_the_table_growing() {
        let mut index = Index::build([sticky("b", "1", "x"), sticky("b", "2", "y")]);
        assert_eq!(index.doc_slots(), 2);
        index.remove("b", "1");
        index.upsert(&sticky("b", "3", "z"));
        assert_eq!(index.doc_slots(), 2);
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn facets_list_only_what_is_actually_present() {
        let yellow = Colour::parse("#fff79e").unwrap();
        let mut index = Index::build([
            Item::new("a", "1", "sticky", "note").with_colour(yellow).with_tags(["review"]),
            Item::new("a", "2", "frame", "Engine bay"),
        ]);
        assert_eq!(index.kinds().collect::<Vec<_>>(), ["frame", "sticky"]);
        assert_eq!(index.colours().collect::<Vec<_>>(), [yellow]);
        assert_eq!(index.tags().collect::<Vec<_>>(), ["review"]);

        index.remove("a", "1");
        assert_eq!(index.kinds().collect::<Vec<_>>(), ["frame"]);
        assert_eq!(index.colours().count(), 0);
        assert_eq!(index.tags().count(), 0);
    }

    #[test]
    fn recolouring_an_item_moves_it_between_colour_facets() {
        let yellow = Colour::parse("#fff79e").unwrap();
        let red = Colour::parse("#ff9e9e").unwrap();
        let mut index = Index::build([Item::new("a", "1", "sticky", "n").with_colour(yellow)]);
        assert_eq!(index.facet_colour_names().docs("yellow").len(), 1);

        index.upsert(&Item::new("a", "1", "sticky", "n").with_colour(red));
        assert!(index.facet_colour_names().docs("yellow").is_empty());
        assert_eq!(index.facet_colour_names().docs("pink").len(), 1, "pale red is also pink");
        assert_eq!(index.facet_colours().docs("#ff9e9e").len(), 1);
    }

    /// 219 of the reference board's 596 widgets are ink strokes with no text at
    /// all. They must still be findable.
    #[test]
    fn an_item_with_no_text_is_indexed_for_its_facets() {
        let index = Index::build([Item::new("a", "ink-1", "ink", "")]);
        assert_eq!(index.len(), 1);
        assert_eq!(index.stats().terms, 0);
        assert_eq!(index.facet_kinds().docs("ink").len(), 1);
    }

    #[test]
    fn repeated_terms_record_every_position_in_order() {
        let index = Index::build([sticky("b", "1", "fan and fan and fan")]);
        let term = index.dictionary().lookup("fan").unwrap();
        assert_eq!(index.postings_of(term).positions(0), Some([0u32, 2, 4].as_slice()));
        assert_eq!(index.stats().positions, 5);
    }

    #[test]
    fn grouping_terms_is_deterministic_and_keeps_positions_ascending() {
        assert_eq!(
            group_terms("b a b"),
            [("a".to_owned(), vec![1]), ("b".to_owned(), vec![0, 2])]
        );
        assert!(group_terms("").is_empty());
    }

    #[test]
    fn the_dirty_flag_tracks_real_changes_only() {
        let mut index = Index::new();
        assert!(!index.is_dirty());
        index.upsert(&sticky("b", "1", "x"));
        assert!(index.is_dirty());
        index.mark_clean();
        index.upsert(&sticky("b", "1", "x"));
        assert!(!index.is_dirty(), "an unchanged upsert is not a change");
        index.upsert(&sticky("b", "1", "y"));
        assert!(index.is_dirty());
    }

    #[test]
    fn emptying_the_index_leaves_nothing_behind() {
        let mut index = Index::build([sticky("a", "1", "x"), sticky("b", "1", "y")]);
        index.remove_board("a");
        index.remove("b", "1");
        assert!(index.is_empty());
        assert_eq!(index.stats(), IndexStats::default());
        assert_eq!(index.boards().count(), 0);
    }
}
