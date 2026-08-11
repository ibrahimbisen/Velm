//! Executing a query.
//!
//! # The shape of the work
//!
//! Every query term is expanded into the dictionary terms it reaches, in tier
//! order, and each expansion's posting list is swept into a dense scratch buffer
//! indexed by document. Terms are AND-ed by *counting*: a document that has been
//! touched by every term so far is eligible for the next one, and a document that
//! misses a term can never be touched again. At the end, the documents whose count
//! equals the number of terms are the matches, and their accumulated tier, field,
//! position and occurrence count are already the [`Score`].
//!
//! Two details make this correct rather than merely fast:
//!
//! - **Expansions are processed best tier first** — exact, then facets, then
//!   prefix, substring and fuzzy. So the first time a document is touched within a
//!   term, it is touched at the best tier that term can reach it by, and the
//!   accumulator never has to revise a tier downward.
//! - **The buffer is dense, not a map.** One `Vec` the length of the document table
//!   is allocated per query — a few hundred kilobytes for the 100-board corpus —
//!   which is a single `memset` against a hash lookup for every one of the hundreds
//!   of thousands of postings a broad prefix query walks.
//!
//! # Bounded work per query
//!
//! A term that is one character long reaches most of the dictionary, and a search
//! box sees exactly that on the first keystroke of every query anyone ever types.
//! Two bounds keep it inside the interactive budget:
//!
//! - [`MAX_EXPANSIONS`] caps how many dictionary terms one query term may reach.
//!   Expansions are generated best-tier-first, so the cap sheds whole tiers from
//!   the bottom — fuzzy before substring, substring before prefix — and within the
//!   one tier that straddles it, the terms kept are the first in dictionary order.
//!   It is reached only by prefixes of one or two characters, which is to say only
//!   while the user is still typing the first syllable of the first word.
//! - Results are collected into a bounded heap of `limit` entries rather than
//!   sorted wholesale, so a query matching every item on 100 boards still sorts
//!   only the page that will be shown.
//!
//! Neither bound is a sampling heuristic: for any query that reaches fewer than
//! [`MAX_EXPANSIONS`] terms — which is every query with a term of three or more
//! characters, on the corpora this is built for — the result is exhaustive.

use std::collections::HashSet;
use std::ops::Range;

use crate::colour::is_colour_name;
use crate::dictionary::{TermId, bounded_levenshtein};
use crate::index::{DocId, Index};
use crate::query::{ColourFilter, Matching, Query, Term};
use crate::rank::{Field, Hit, MatchTier, Score};
use crate::snippet::Snippet;
use crate::source::resolve_kind;
use crate::token;

/// The most dictionary terms one query term may expand to. See the module docs.
///
/// Sized against the corpus this crate targets — roughly 40,000 distinct terms
/// across 100 boards — so that any prefix of three characters or more expands
/// exhaustively, and only the one- and two-character prefixes a user types on the
/// way to a real word are ever truncated.
pub const MAX_EXPANSIONS: usize = 4096;

impl Index {
    /// Runs `query` and returns its hits, best first.
    ///
    /// The order is total and deterministic: see [`crate::rank`]. Two calls with
    /// equal queries on an equal index return equal `Vec`s, whatever order the
    /// index was built in.
    pub fn search(&self, query: &Query) -> Vec<Hit> {
        if query.is_empty() || self.is_empty() {
            return Vec::new();
        }
        let ranked = if query.terms().is_empty() {
            self.by_filters_alone(query)
        } else {
            self.by_terms(query)
        };
        ranked.into_iter().map(|scored| self.hit(scored, query)).collect()
    }

    /// The general path: at least one term.
    fn by_terms(&self, query: &Query) -> Vec<Scored> {
        let mut accumulated = vec![Accumulator::UNTOUCHED; self.doc_slots()];
        for (ordinal, term) in query.terms().iter().enumerate() {
            let ordinal = ordinal as u32;
            match term {
                Term::Word(word) => {
                    self.accumulate_word(word, ordinal, query.matching(), &mut accumulated);
                }
                Term::Phrase(words) => self.accumulate_phrase(words, ordinal, &mut accumulated),
            }
        }

        let wanted = query.terms().len() as u32;
        let filters = query.filters();
        let mut best = TopK::new(query.limit());
        for (id, accumulator) in accumulated.iter().enumerate() {
            if accumulator.satisfied != wanted {
                continue;
            }
            let id = id as DocId;
            let Some(doc) = self.doc(id) else { continue };
            if !filters.accepts(&doc.board, &doc.kind, doc.colour, &doc.tags) {
                continue;
            }
            best.offer(Scored { doc: id, score: accumulator.score() });
        }
        best.finish(self)
    }

    /// The `kind:frame` path: filters and nothing to score.
    ///
    /// Enumerated from the facet posting lists rather than by walking every
    /// document, because this is how "select every yellow sticky" is answered and
    /// it must cost the size of the answer, not the size of the corpus.
    fn by_filters_alone(&self, query: &Query) -> Vec<Scored> {
        let filters = query.filters();
        let mut candidates: Option<Vec<DocId>> = None;
        let mut narrowest = |docs: Vec<DocId>| {
            if candidates.as_ref().is_none_or(|current| docs.len() < current.len()) {
                candidates = Some(docs);
            }
        };

        if !filters.boards.is_empty() {
            narrowest(union(filters.boards.iter().map(|b| self.facet_boards().docs(b))));
        }
        if !filters.kinds.is_empty() {
            narrowest(union(filters.kinds.iter().map(|k| self.facet_kinds().docs(k))));
        }
        if !filters.tags.is_empty() {
            narrowest(union(filters.tags.iter().map(|t| self.facet_tags().docs(t))));
        }
        if !filters.colours.is_empty() {
            narrowest(union(filters.colours.iter().map(|colour| match colour {
                ColourFilter::Exact(value) => self.facet_colours().docs(&value.to_hex()),
                ColourFilter::Named(name) => self.facet_colour_names().docs(name),
            })));
        }

        let candidates = candidates.unwrap_or_default();
        let score = Score {
            tier: MatchTier::Exact,
            field: Field::Facet,
            position: 0,
            occurrences: 0,
        };
        let mut best = TopK::new(query.limit());
        for id in candidates {
            let Some(doc) = self.doc(id) else { continue };
            if !filters.accepts(&doc.board, &doc.kind, doc.colour, &doc.tags) {
                continue;
            }
            best.offer(Scored { doc: id, score });
        }
        best.finish(self)
    }

    fn accumulate_word(
        &self,
        word: &str,
        ordinal: u32,
        matching: Matching,
        accumulated: &mut [Accumulator],
    ) {
        let expansions = self.expand(word, matching);
        self.sweep(expansions.exact.as_slice(), MatchTier::Exact, ordinal, accumulated);

        // Facets sit between exact text and prefix text: an item that *is* a frame
        // is an exact answer to `frame`, but an item whose text says "frame" is a
        // better one, and `crate::rank` §2 breaks that tie on the field.
        if matching.facets {
            for docs in self.facet_matches(word) {
                for &doc in docs {
                    accumulated[doc as usize].touch(
                        ordinal,
                        MatchTier::Exact,
                        Field::Facet,
                        0,
                        1,
                    );
                }
            }
        }

        self.sweep(&expansions.prefix, MatchTier::Prefix, ordinal, accumulated);
        self.sweep(&expansions.substring, MatchTier::Substring, ordinal, accumulated);
        // Ascending by distance, so a document reachable by both a one-edit and a
        // two-edit term is first touched at one edit.
        for &(term, edits) in &expansions.fuzzy {
            let tier = MatchTier::Fuzzy { edits: edits as u8 };
            self.sweep(std::slice::from_ref(&term), tier, ordinal, accumulated);
        }
    }

    /// Folds one tier's worth of expansions into the accumulator.
    fn sweep(
        &self,
        terms: &[TermId],
        tier: MatchTier,
        ordinal: u32,
        accumulated: &mut [Accumulator],
    ) {
        for &term in terms {
            for posting in self.postings_of(term).entries() {
                let first = posting.positions.first().copied().unwrap_or(0);
                accumulated[posting.doc as usize].touch(
                    ordinal,
                    tier,
                    Field::Text,
                    first,
                    posting.positions.len() as u32,
                );
            }
        }
    }

    /// Phrases match literally: every word exactly, adjacent, in order.
    fn accumulate_phrase(&self, words: &[String], ordinal: u32, accumulated: &mut [Accumulator]) {
        if words.is_empty() {
            return;
        }
        // A phrase containing a word that is nowhere in the corpus matches nothing,
        // and there is no point walking a single posting list to discover that.
        let mut lists = Vec::with_capacity(words.len());
        for word in words {
            match self.dictionary().lookup(word) {
                Some(term) => lists.push(self.postings_of(term)),
                None => return,
            }
        }

        // Drive from the rarest word: every document that can hold the phrase must
        // hold that word, so this is the smallest set that is guaranteed complete.
        let driver = lists
            .iter()
            .enumerate()
            .min_by_key(|(_, list)| list.len())
            .map(|(at, _)| at)
            .expect("words is not empty");

        let mut positions: Vec<&[u32]> = Vec::with_capacity(lists.len());
        for posting in lists[driver].entries() {
            positions.clear();
            let complete = lists.iter().all(|list| match list.positions(posting.doc) {
                Some(found) => {
                    positions.push(found);
                    true
                }
                None => false,
            });
            if !complete {
                continue;
            }

            let mut first = None;
            let mut occurrences = 0;
            for &start in positions[0] {
                let adjacent = positions[1..].iter().enumerate().all(|(offset, at)| {
                    at.binary_search(&(start + offset as u32 + 1)).is_ok()
                });
                if adjacent {
                    first.get_or_insert(start);
                    occurrences += 1;
                }
            }
            if let Some(position) = first {
                accumulated[posting.doc as usize].touch(
                    ordinal,
                    MatchTier::Exact,
                    Field::Text,
                    position,
                    occurrences,
                );
            }
        }
    }

    /// The dictionary terms `word` reaches, grouped by tier.
    fn expand(&self, word: &str, matching: Matching) -> Expansions {
        let mut expansions = Expansions::default();
        if self.dictionary().is_empty() {
            return expansions;
        }
        let mut seen: HashSet<TermId> = HashSet::new();
        let mut budget = MAX_EXPANSIONS;

        if let Some(term) = self.dictionary().lookup(word) {
            expansions.exact = Some(term);
            seen.insert(term);
            budget -= 1;
        }
        if matching.prefix && budget > 0 {
            for term in self.dictionary().with_prefix(word).take(budget) {
                if seen.insert(term) {
                    expansions.prefix.push(term);
                }
            }
            budget = MAX_EXPANSIONS - seen.len();
        }
        if matching.substring && word.len() > 1 && budget > 0 {
            for term in self.dictionary().containing(word).take(budget) {
                if seen.insert(term) {
                    expansions.substring.push(term);
                }
            }
            budget = MAX_EXPANSIONS - seen.len();
        }
        let edits = matching.budget_for(word);
        if edits > 0 && budget > 0 {
            for (term, distance) in self.dictionary().within_edits(word, edits).take(budget) {
                if seen.insert(term) {
                    expansions.fuzzy.push((term, distance));
                }
            }
            // A stable sort, so terms at the same distance keep dictionary order and
            // the expansion is the same on every run.
            expansions.fuzzy.sort_by_key(|&(_, distance)| distance);
        }
        expansions
    }

    /// The facet posting lists a bare word names — its kind, and its colour.
    ///
    /// Both are consulted: nothing stops a board from having a kind called `red`,
    /// and returning both lists is more useful than choosing between them.
    fn facet_matches(&self, word: &str) -> Vec<&[DocId]> {
        let mut lists = Vec::new();
        if let Some(kind) = resolve_kind(word, |kind| self.facet_kinds().contains(kind)) {
            lists.push(self.facet_kinds().docs(&kind));
        }
        if is_colour_name(word) {
            let docs = self.facet_colour_names().docs(word);
            if !docs.is_empty() {
                lists.push(docs);
            }
        }
        lists
    }

    fn hit(&self, scored: Scored, query: &Query) -> Hit {
        let doc = self.doc(scored.doc).expect("scored documents are live");
        let spans = matched_spans(&doc.text, query);
        Hit {
            board: doc.board.to_string(),
            item: doc.item.to_string(),
            kind: doc.kind.to_string(),
            colour: doc.colour,
            tags: doc.tags.iter().map(|tag| tag.to_string()).collect(),
            score: scored.score,
            snippet: Snippet::build(&doc.text, &spans),
        }
    }
}

/// Which dictionary terms one query word reaches, by tier.
#[derive(Debug, Default)]
struct Expansions {
    exact: Option<TermId>,
    prefix: Vec<TermId>,
    substring: Vec<TermId>,
    /// Paired with the edit distance, ascending, because the distance is part of
    /// the tier — see [`MatchTier::Fuzzy`].
    fuzzy: Vec<(TermId, u32)>,
}

/// Per-document state while a query runs.
#[derive(Debug, Clone, Copy)]
struct Accumulator {
    /// How many query terms this document has satisfied.
    satisfied: u32,
    /// The term currently accumulating into this document, so that a second
    /// expansion of the *same* term adds occurrences rather than counting the term
    /// twice.
    active: u32,
    tier: MatchTier,
    field: Field,
    position: u32,
    occurrences: u32,
}

impl Accumulator {
    const UNTOUCHED: Self = Self {
        satisfied: 0,
        active: u32::MAX,
        tier: Score::BEST.tier,
        field: Score::BEST.field,
        position: Score::BEST.position,
        occurrences: 0,
    };

    fn touch(
        &mut self,
        ordinal: u32,
        tier: MatchTier,
        field: Field,
        position: u32,
        occurrences: u32,
    ) {
        if self.active == ordinal {
            // Same term, another expansion of it. The tier and field are already
            // the best this term can reach the document by — expansions are swept
            // best-first — so only the position and the count can improve.
            self.position = self.position.min(position);
            self.occurrences += occurrences;
            return;
        }
        if self.satisfied != ordinal {
            // The document missed an earlier term, so the AND has already failed.
            return;
        }
        self.active = ordinal;
        self.satisfied = ordinal + 1;
        self.tier = self.tier.max(tier);
        self.field = self.field.max(field);
        self.position = self.position.min(position);
        self.occurrences += occurrences;
    }

    fn score(&self) -> Score {
        Score {
            tier: self.tier,
            field: self.field,
            // A document matched only by facets has no position of its own.
            position: if self.position == u32::MAX { 0 } else { self.position },
            occurrences: self.occurrences,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scored {
    doc: DocId,
    score: Score,
}

/// Keeps the best `limit` hits without sorting the rest.
///
/// The heap's ordering is *inverted* — its root is the weakest hit held — so the
/// loser of each overflow is the one dropped. The comparison is the full ranking
/// key including `(board, item)`, not a cheap prefix of it: truncating the key here
/// would make which of two equally-scored documents survives depend on the order
/// they were swept in, and that order depends on internal document ids, which are
/// not stable across a save and load. The whole determinism guarantee lives on this
/// comparison being the same one the final sort uses.
struct TopK {
    limit: usize,
    heap: std::collections::BinaryHeap<Weakest>,
}

impl TopK {
    fn new(limit: usize) -> Self {
        Self { limit, heap: std::collections::BinaryHeap::new() }
    }

    fn offer(&mut self, scored: Scored) {
        if self.limit == 0 {
            return;
        }
        self.heap.push(Weakest(scored));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    fn finish(self, index: &Index) -> Vec<Scored> {
        let mut hits: Vec<Scored> = self.heap.into_iter().map(|weakest| weakest.0).collect();
        hits.sort_unstable_by(|a, b| rank_key(index, a).cmp(&rank_key(index, b)));
        hits
    }
}

/// The total ranking key: the score, then the primary key that makes it total.
fn rank_key<'a>(index: &'a Index, scored: &Scored) -> (Score, &'a str, &'a str) {
    let doc = index.doc(scored.doc).expect("scored documents are live");
    (scored.score, &doc.board, &doc.item)
}

/// A `Scored` ordered so that `Greater` means "ranks worse".
///
/// The document ids stand in for `(board, item)` in the heap, which is sound only
/// because the heap is a *filter*, not the final order: it must agree with the real
/// key on strict inequalities, and ties among the last few entries are resolved by
/// the sort in [`TopK::finish`]. Where a tie straddles the `limit` boundary, which
/// of two identically-scored documents is kept is decided by document id — and the
/// pair is then reported in `(board, item)` order regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Weakest(Scored);

impl Ord for Weakest {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.score.cmp(&other.0.score).then(self.0.doc.cmp(&other.0.doc))
    }
}

impl PartialOrd for Weakest {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Merges sorted document lists into one sorted, deduplicated list.
fn union<'a>(lists: impl Iterator<Item = &'a [DocId]>) -> Vec<DocId> {
    let mut merged: Vec<DocId> = lists.flatten().copied().collect();
    merged.sort_unstable();
    merged.dedup();
    merged
}

/// Where in `text` the query matched, for highlighting.
///
/// Recomputed from the text rather than carried through the accumulator, and only
/// for the handful of documents that survived ranking. Carrying it would mean a
/// `Vec` of positions per candidate document — hundreds of thousands of small
/// allocations for a query that will show twenty results.
///
/// Whole tokens are highlighted, including for a prefix or fuzzy match: a search
/// for `cool` marks `cooling`, not `cool` inside it. That is what every search UI
/// does, and it is also the only choice that makes sense for a fuzzy match, where
/// the matching characters are not contiguous.
fn matched_spans(text: &str, query: &Query) -> Vec<Range<usize>> {
    if text.is_empty() {
        return Vec::new();
    }
    let tokens: Vec<token::Token<'_>> = token::tokenise(text).collect();
    let matching = query.matching();
    let mut spans = Vec::new();

    for term in query.terms() {
        match term {
            Term::Word(word) => {
                let chars: Vec<char> = word.chars().collect();
                let budget = matching.budget_for(word);
                for candidate in &tokens {
                    if word_matches(word, &chars, budget, &candidate.text, matching) {
                        spans.push(candidate.span());
                    }
                }
            }
            Term::Phrase(words) if !words.is_empty() => {
                let last = words.len() - 1;
                for start in 0..tokens.len().saturating_sub(last) {
                    let adjacent = words
                        .iter()
                        .enumerate()
                        .all(|(offset, word)| *tokens[start + offset].text == **word);
                    if adjacent {
                        spans.push(tokens[start].start..tokens[start + last].end);
                    }
                }
            }
            Term::Phrase(_) => {}
        }
    }
    spans
}

/// Whether one token satisfies one query word, by the same rules the dictionary
/// expansion used. Kept in one place so the highlight cannot disagree with the
/// match that produced the hit.
fn word_matches(
    word: &str,
    word_chars: &[char],
    edits: u32,
    candidate: &str,
    matching: Matching,
) -> bool {
    candidate == word
        || (matching.prefix && candidate.starts_with(word))
        || (matching.substring && word.len() > 1 && candidate.contains(word))
        || (edits > 0 && bounded_levenshtein(word_chars, candidate, edits).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colour::Colour;
    use crate::query::Matching;
    use crate::source::Item;

    const YELLOW: &str = "#fff79e";
    const RED: &str = "#ff9e9e";

    fn corpus() -> Index {
        let yellow = Colour::parse(YELLOW).unwrap();
        let red = Colour::parse(RED).unwrap();
        Index::build([
            Item::new("garage", "s1", "sticky", "Cooling fan relay").with_colour(yellow),
            Item::new("garage", "s2", "sticky", "Water pump").with_colour(yellow),
            Item::new("garage", "s3", "sticky", "Replace the cooling hose").with_colour(red),
            Item::new("garage", "f1", "frame", "Engine bay"),
            Item::new("garage", "f2", "frame", "Wiring"),
            Item::new("garage", "i1", "ink", ""),
            Item::new("keys", "s1", "sticky", "Cooling is not a keyboard concern")
                .with_colour(yellow)
                .with_tags(["review"]),
            Item::new("keys", "t1", "text", "Switch lubrication"),
        ])
    }

    fn ids(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|hit| format!("{}/{}", hit.board, hit.item)).collect()
    }

    fn find(index: &Index, query: &str) -> Vec<String> {
        ids(&index.search(&Query::parse(query)))
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(corpus().search(&Query::parse("")).is_empty());
        assert!(corpus().search(&Query::parse("   ")).is_empty());
        assert!(Index::new().search(&Query::parse("cooling")).is_empty());
    }

    /// Note the order: `garage/s3` says "cooling" in its third word, so it ranks below
    /// the two that open with it, and those two tie until `(board, item)` separates
    /// them. That is the ranking in `crate::rank` working, not an accident.
    #[test]
    fn a_word_matches_case_insensitively_across_boards() {
        assert_eq!(find(&corpus(), "cooling"), ["garage/s1", "keys/s1", "garage/s3"]);
        assert_eq!(find(&corpus(), "COOLING"), ["garage/s1", "keys/s1", "garage/s3"]);
    }

    #[test]
    fn multiple_terms_are_and_ed_not_or_ed() {
        assert_eq!(find(&corpus(), "cooling relay"), ["garage/s1"]);
        assert_eq!(find(&corpus(), "cooling nothing"), Vec::<String>::new());
    }

    #[test]
    fn a_prefix_finds_a_word_the_user_has_not_finished_typing() {
        assert_eq!(find(&corpus(), "cool"), ["garage/s1", "keys/s1", "garage/s3"]);
        assert_eq!(find(&corpus(), "lub"), ["keys/t1"]);
    }

    #[test]
    fn a_substring_finds_a_word_it_is_buried_in() {
        let index = Index::build([Item::new("b", "1", "sticky", "waterpump housing")]);
        let hits = index.search(&Query::parse("pump"));
        assert_eq!(ids(&hits), ["b/1"]);
        assert_eq!(hits[0].score.tier, MatchTier::Substring);
    }

    #[test]
    fn a_quoted_phrase_requires_adjacency_and_order() {
        let index = Index::build([
            Item::new("b", "1", "sticky", "cooling fan relay"),
            Item::new("b", "2", "sticky", "fan cooling"),
            Item::new("b", "3", "sticky", "cooling the fan"),
        ]);
        assert_eq!(ids(&index.search(&Query::parse("\"cooling fan\""))), ["b/1"]);
        assert_eq!(ids(&index.search(&Query::parse("cooling fan"))).len(), 3);
    }

    #[test]
    fn a_one_word_phrase_switches_prefix_matching_off() {
        let index = corpus();
        assert_eq!(find(&index, "cool").len(), 3);
        assert_eq!(ids(&index.search(&Query::parse("\"cool\""))), Vec::<String>::new());
        assert_eq!(ids(&index.search(&Query::parse("\"cooling\""))).len(), 3);
    }

    #[test]
    fn fuzzy_matching_is_off_by_default_and_tolerates_two_typos_when_asked() {
        let index = corpus();
        assert!(index.search(&Query::parse("colling")).is_empty(), "not by default");

        let forgiving = Query::parse("colling").with_matching(Matching::forgiving());
        let hits = index.search(&forgiving);
        assert_eq!(ids(&hits), ["garage/s1", "keys/s1", "garage/s3"]);
        assert!(hits.iter().all(|hit| hit.score.tier == MatchTier::Fuzzy { edits: 1 }));

        let two_edits = Query::parse("coolgni").with_matching(Matching::forgiving());
        assert_eq!(index.search(&two_edits).len(), 3);
    }

    #[test]
    fn the_four_tiers_rank_in_order_regardless_of_the_ids_that_carry_them() {
        // Deliberately named so that `(board, item)` order is the *reverse* of the
        // ranking: if the tiers were not doing the work, this order would invert.
        let index = Index::build([
            Item::new("b", "4", "sticky", "cooling"),
            Item::new("b", "3", "sticky", "coolingfan"),
            Item::new("b", "2", "sticky", "precooling"),
            Item::new("b", "1", "sticky", "colling"),
        ]);
        let query = Query::parse("cooling").with_matching(Matching::forgiving());
        let hits = index.search(&query);
        assert_eq!(ids(&hits), ["b/4", "b/3", "b/2", "b/1"]);
        let tiers: Vec<_> = hits.iter().map(|hit| hit.score.tier).collect();
        assert_eq!(
            tiers,
            [
                MatchTier::Exact,
                MatchTier::Prefix,
                MatchTier::Substring,
                MatchTier::Fuzzy { edits: 1 },
            ]
        );
    }

    #[test]
    fn one_typo_outranks_two() {
        let index = Index::build([
            Item::new("b", "two-edits", "sticky", "cooling"),
            Item::new("b", "one-edit", "sticky", "coolant"),
        ]);
        let hits = index.search(&Query::parse("coolent").with_matching(Matching::forgiving()));
        assert_eq!(ids(&hits), ["b/one-edit", "b/two-edits"]);
        assert_eq!(hits[0].score.tier, MatchTier::Fuzzy { edits: 1 });
        assert_eq!(hits[1].score.tier, MatchTier::Fuzzy { edits: 2 });
    }

    #[test]
    fn an_earlier_match_position_wins() {
        let index = Index::build([
            Item::new("b", "late", "sticky", "one two three four cooling"),
            Item::new("b", "early", "sticky", "cooling one two three four"),
        ]);
        assert_eq!(ids(&index.search(&Query::parse("cooling"))), ["b/early", "b/late"]);
    }

    #[test]
    fn a_multi_term_hit_takes_the_worst_tier_of_its_terms() {
        let index = Index::build([Item::new("b", "1", "sticky", "cooling fanbelt")]);
        let hits = index.search(&Query::parse("cooling fan"));
        assert_eq!(hits[0].score.tier, MatchTier::Prefix, "one term only matched by prefix");
    }

    #[test]
    fn filters_narrow_by_board_kind_colour_and_tag() {
        let index = corpus();
        assert_eq!(find(&index, "cooling board:keys"), ["keys/s1"]);
        assert_eq!(find(&index, "cooling kind:sticky").len(), 3);
        assert_eq!(find(&index, "cooling colour:red"), ["garage/s3"]);
        assert_eq!(find(&index, "cooling tag:review"), ["keys/s1"]);
        assert_eq!(find(&index, "cooling colour:#fff79e"), ["garage/s1", "keys/s1"]);
    }

    #[test]
    fn a_filter_with_no_terms_enumerates_the_facet() {
        let index = corpus();
        assert_eq!(find(&index, "kind:frame"), ["garage/f1", "garage/f2"]);
        assert_eq!(find(&index, "kind:ink"), ["garage/i1"], "no text, still findable");
        assert_eq!(find(&index, "board:keys").len(), 2);
        assert_eq!(find(&index, "kind:sticky colour:red"), ["garage/s3"]);
    }

    /// The two queries the brief names by hand.
    #[test]
    fn all_yellow_stickies_and_every_frame_work_as_typed() {
        let index = corpus();
        assert_eq!(find(&index, "all yellow stickies"), ["garage/s1", "garage/s2", "keys/s1"]);
        assert_eq!(find(&index, "every frame"), ["garage/f1", "garage/f2"]);
        assert_eq!(find(&index, "frames"), ["garage/f1", "garage/f2"]);
        assert_eq!(find(&index, "notes").len(), 4, "sticky, via a synonym");
    }

    #[test]
    fn a_text_match_outranks_a_facet_match_for_the_same_word() {
        let index = Index::build([
            Item::new("b", "says-frame", "sticky", "frame"),
            Item::new("b", "is-frame", "frame", "Engine bay"),
        ]);
        let hits = index.search(&Query::parse("frame"));
        assert_eq!(ids(&hits), ["b/says-frame", "b/is-frame"]);
        assert_eq!(hits[0].score.field, Field::Text);
        assert_eq!(hits[1].score.field, Field::Facet);
    }

    #[test]
    fn facet_matching_can_be_switched_off() {
        let index = corpus();
        let literal = Query::parse("frame")
            .with_matching(Matching { facets: false, ..Matching::interactive() });
        assert!(index.search(&literal).is_empty(), "no item's text says 'frame'");
    }

    #[test]
    fn a_hit_carries_the_ids_needed_to_jump_to_it_and_a_marked_snippet() {
        let index = corpus();
        let hits = index.search(&Query::parse("relay"));
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.board, "garage");
        assert_eq!(hit.item, "s1");
        assert_eq!(hit.kind, "sticky");
        assert_eq!(hit.colour, Colour::parse(YELLOW));
        assert_eq!(hit.snippet.render("[", "]"), "Cooling fan [relay]");
    }

    #[test]
    fn a_snippet_marks_every_term_of_a_multi_term_query() {
        let index = Index::build([Item::new("b", "1", "sticky", "Cooling fan relay")]);
        let hits = index.search(&Query::parse("cooling relay"));
        assert_eq!(hits[0].snippet.render("<", ">"), "<Cooling> fan <relay>");
    }

    #[test]
    fn a_facet_only_hit_still_carries_a_readable_snippet() {
        let index = corpus();
        let hits = index.search(&Query::parse("kind:frame"));
        assert_eq!(hits[0].snippet.text, "Engine bay");
        assert!(hits[0].snippet.spans.is_empty(), "nothing was matched by text");
    }

    #[test]
    fn the_limit_takes_the_best_hits_not_an_arbitrary_page() {
        let index = Index::build((0..50).map(|n| {
            Item::new("b", format!("{n:02}"), "sticky", if n == 40 { "fan" } else { "fanbelt" })
        }));
        let hits = index.search(&Query::parse("fan").with_limit(3));
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].item, "40", "the exact match, despite being the 41st swept");
        assert_eq!(ids(&hits), ["b/40", "b/00", "b/01"]);
    }

    /// The guarantee in `crate::rank` §5, checked the only way that means anything:
    /// build the same corpus in two different orders and compare the sequences.
    #[test]
    fn results_do_not_reshuffle_between_identical_queries_or_build_orders() {
        let mut items: Vec<Item> = (0..40)
            .map(|n| Item::new(format!("b{}", n % 3), format!("i{n:02}"), "sticky", "same text"))
            .collect();

        let forwards = Index::build(items.clone());
        items.reverse();
        let backwards = Index::build(items.clone());
        items.rotate_left(7);
        let rotated = Index::build(items);

        for query in ["same", "text", "kind:sticky", "same kind:sticky"] {
            let query = Query::parse(query);
            let expected = forwards.search(&query);
            assert_eq!(expected, forwards.search(&query), "twice on one index");
            assert_eq!(expected, backwards.search(&query), "built in reverse");
            assert_eq!(expected, rotated.search(&query), "built rotated");
            assert!(!expected.is_empty());
        }
    }

    #[test]
    fn determinism_survives_deletions_and_reinsertions() {
        let items: Vec<Item> = (0..20)
            .map(|n| Item::new("b", format!("i{n:02}"), "sticky", "cooling fan"))
            .collect();
        let pristine = Index::build(items.clone());

        let mut churned = Index::build(items.clone());
        for item in items.iter().take(10) {
            churned.remove(&item.board, &item.item);
        }
        for item in items.iter().take(10) {
            churned.upsert(item);
        }

        let query = Query::parse("cooling");
        assert_eq!(pristine.search(&query), churned.search(&query));
    }

    #[test]
    fn an_edited_item_is_searchable_by_its_new_text_and_not_its_old() {
        let mut index = corpus();
        index.upsert(&Item::new("garage", "s2", "sticky", "Thermostat housing"));
        assert!(index.search(&Query::parse("water")).is_empty());
        assert_eq!(find(&index, "thermostat"), ["garage/s2"]);
    }

    #[test]
    fn a_broad_prefix_is_bounded_but_still_returns_its_best_matches() {
        let index = Index::build(
            (0..500).map(|n| Item::new("b", format!("{n:03}"), "sticky", format!("term{n:03}"))),
        );
        let hits = index.search(&Query::parse("t").with_limit(5));
        assert_eq!(hits.len(), 5);
        assert!(hits.iter().all(|hit| hit.score.tier == MatchTier::Prefix));
    }

    #[test]
    fn highlight_spans_agree_with_the_tier_that_produced_the_hit() {
        let index = Index::build([Item::new("b", "1", "sticky", "waterpump")]);
        let hits = index.search(&Query::parse("pump"));
        assert_eq!(hits[0].snippet.render("[", "]"), "[waterpump]", "the whole token");
    }
}
