//! The term dictionary: every distinct word in the corpus, and how to find the
//! ones a query nearly names.
//!
//! # Term ids are stable, which forces the layout
//!
//! Posting lists are keyed by term id, so an id can never change once handed out —
//! renumbering the dictionary would mean rewriting every posting list in the index.
//! That rules out the obvious representation, a sorted `Vec<String>` with the index
//! as the id, because inserting `cool` shifts `cooling` and everything after it.
//!
//! So there are three structures, each doing one job:
//!
//! - `terms` — append-only, indexed by [`TermId`]. Ids are dense and permanent.
//!   Freed slots are tombstoned and reused, never compacted.
//! - `by_text` — the exact-match hash lookup, which is the only one a keystroke in
//!   a well-typed query touches.
//! - `sorted` — term ids ordered by their text. This is what makes prefix matching
//!   `O(log n + k)` instead of a scan, and it is only ever memmoved, never rebuilt.
//!
//! Substring and fuzzy matching scan `terms` linearly. That is a real cost and it
//! is accepted knowingly: at the corpus this crate is built for — roughly 40k
//! distinct terms across 100 boards — a linear pass with the pruning below is
//! tens of microseconds, where a suffix automaton or a trigram index would be
//! hundreds of lines and a second thing to keep in sync on every edit. The
//! benchmark reports the real number; if a corpus ever makes it the bottleneck,
//! this is the module to change and nothing outside it needs to know.
//!
//! # Fuzzy matching is pruned before any arithmetic happens
//!
//! A bounded Levenshtein distance over 40k terms is 40k dynamic-programming
//! matrices, which is far too slow to do per keystroke. Two filters run first, and
//! together they reject well over 99% of the dictionary in a few nanoseconds each:
//!
//! 1. **Length.** Editing changes length by at most one per operation, so terms
//!    whose length differs from the query's by more than the budget cannot match.
//! 2. **Character classes.** Each term carries a 64-bit mask of the character
//!    classes it contains. A class present in the term but absent from the query
//!    needs at least one edit, so if more than `budget` classes are exclusive to
//!    either side, the distance exceeds the budget. Hash collisions can only make
//!    two masks *overlap more*, which can only make the filter reject less — it is
//!    never wrong, only sometimes lenient.
//!
//! Whatever survives goes through a banded DP that abandons a row as soon as its
//! minimum exceeds the budget.

use std::collections::HashMap;

/// A term's permanent identity. Dense, so posting lists can be a `Vec` indexed by
/// it rather than a map.
pub type TermId = u32;

#[derive(Debug, Clone)]
struct Entry {
    text: Box<str>,
    /// Character-class mask, for the fuzzy pre-filter. See the module docs.
    mask: u64,
    /// Length in *characters*, not bytes: edit distance counts characters, and a
    /// byte length would reject `motoröl` against `motorol` for the wrong reason.
    chars: u16,
}

#[derive(Debug, Default)]
pub struct Dictionary {
    terms: Vec<Option<Entry>>,
    by_text: HashMap<Box<str>, TermId>,
    /// Term ids ordered by `terms[id].text`. Never contains a tombstoned id.
    sorted: Vec<TermId>,
    free: Vec<TermId>,
}

impl Dictionary {
    /// Number of live terms.
    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// The id space's high-water mark, including tombstones. Posting storage is
    /// indexed by term id, so this is the length that storage must have.
    pub fn capacity(&self) -> usize {
        self.terms.len()
    }

    pub fn text(&self, id: TermId) -> Option<&str> {
        self.terms.get(id as usize)?.as_ref().map(|entry| &*entry.text)
    }

    /// The id of `term`, if it is in the dictionary.
    pub fn lookup(&self, term: &str) -> Option<TermId> {
        self.by_text.get(term).copied()
    }

    /// The id of `term`, adding it if new.
    pub fn intern(&mut self, term: &str) -> TermId {
        if let Some(id) = self.by_text.get(term) {
            return *id;
        }
        let entry = Entry {
            text: term.into(),
            mask: char_mask(term),
            chars: term.chars().count().min(u16::MAX as usize) as u16,
        };
        let id = match self.free.pop() {
            Some(id) => {
                self.terms[id as usize] = Some(entry);
                id
            }
            None => {
                self.terms.push(Some(entry));
                (self.terms.len() - 1) as TermId
            }
        };
        self.by_text.insert(term.into(), id);
        let at = self.sorted.partition_point(|&other| self.text_of(other) < term);
        self.sorted.insert(at, id);
        id
    }

    /// Drops a term. Called when its posting list empties.
    ///
    /// Leaving dead terms in place would be cheaper, but they are not free: every
    /// substring and fuzzy query scans them, and every one of them is a candidate
    /// that resolves to no documents — so a search would spend its budget proving
    /// that words nobody has written any more still match nothing.
    pub fn remove(&mut self, id: TermId) {
        let Some(entry) = self.terms.get(id as usize).and_then(Option::as_ref) else {
            return;
        };
        // The sorted view is searched *before* the slot is tombstoned: the search
        // compares against `terms[..].text`, and a hole where the target used to be
        // would make it walk straight past its own element.
        let text = entry.text.clone();
        let at = self.sorted.binary_search_by(|&other| self.text_of(other).cmp(&text));
        if let Ok(at) = at {
            self.sorted.remove(at);
        }
        self.by_text.remove(&text);
        self.terms[id as usize] = None;
        self.free.push(id);
    }

    /// Live term ids in ascending text order. The order every persisted index is
    /// written in, and the order prefix results are produced in.
    pub fn sorted_ids(&self) -> &[TermId] {
        &self.sorted
    }

    /// Every term id whose text begins with `prefix`, in text order.
    ///
    /// `O(log n + k)`: the sorted view is binary-searched for the first term at or
    /// after the prefix, then walked while the prefix still matches. Because the
    /// order is bytewise and a prefix is a bytewise-minimal element of its own
    /// range, the walk stops at exactly the right place.
    pub fn with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = TermId> + 'a {
        let start = self.sorted.partition_point(|&id| self.text_of(id) < prefix);
        self.sorted[start..]
            .iter()
            .copied()
            .take_while(move |&id| self.text_of(id).starts_with(prefix))
    }

    /// Every term id whose text contains `needle`, in term-id order.
    ///
    /// The same two pre-filters the fuzzy scan uses, for the same reason and with
    /// the same safety argument: a term containing `needle` must be at least as
    /// long as it and must contain every one of its character classes, so a term
    /// failing either cannot match. Mask collisions can only make two masks look
    /// *more* alike, which can only let a term through — never exclude a real
    /// match. On the benchmark corpus this rejects the overwhelming majority of the
    /// dictionary before `str::contains` is called once, and it is the difference
    /// between a substring query costing hundreds of microseconds and tens.
    pub fn containing<'a>(&'a self, needle: &'a str) -> impl Iterator<Item = TermId> + 'a {
        let mask = char_mask(needle);
        let length = needle.len();
        self.live()
            .filter(move |(_, entry)| {
                entry.text.len() >= length
                    && (mask & !entry.mask) == 0
                    && entry.text.contains(needle)
            })
            .map(|(id, _)| id)
    }

    /// Every term within `budget` edits of `query`, with its distance.
    ///
    /// Returned in term-id order, which is stable for a given index but not
    /// meaningful; the caller ranks by distance and then by the deterministic key
    /// in [`crate::rank`].
    pub fn within_edits<'a>(
        &'a self,
        query: &'a str,
        budget: u32,
    ) -> impl Iterator<Item = (TermId, u32)> + 'a {
        let chars: Vec<char> = query.chars().collect();
        let mask = char_mask(query);
        let len = chars.len() as i64;
        self.live().filter_map(move |(id, entry)| {
            if (entry.chars as i64 - len).abs() > budget as i64 {
                return None;
            }
            if (entry.mask & !mask).count_ones() > budget
                || (mask & !entry.mask).count_ones() > budget
            {
                return None;
            }
            bounded_levenshtein(&chars, &entry.text, budget).map(|distance| (id, distance))
        })
    }

    fn live(&self) -> impl Iterator<Item = (TermId, &Entry)> {
        self.terms
            .iter()
            .enumerate()
            .filter_map(|(id, slot)| slot.as_ref().map(|entry| (id as TermId, entry)))
    }

    /// Panics on a tombstoned id. Only ever called with ids drawn from `sorted`,
    /// which never holds one.
    fn text_of(&self, id: TermId) -> &str {
        &self.terms[id as usize].as_ref().expect("sorted holds only live ids").text
    }

    /// Rebuilds a dictionary from terms already in ascending text order.
    ///
    /// Used only by [`crate::persist`]. Assigning ids in the file's order makes
    /// `sorted` the identity permutation, so loading does no sorting and no
    /// comparisons at all — the whole point of writing the file sorted.
    pub(crate) fn from_sorted(terms: Vec<Box<str>>) -> Self {
        let mut by_text = HashMap::with_capacity(terms.len());
        let mut entries = Vec::with_capacity(terms.len());
        let mut sorted = Vec::with_capacity(terms.len());
        for (id, text) in terms.into_iter().enumerate() {
            by_text.insert(text.clone(), id as TermId);
            sorted.push(id as TermId);
            entries.push(Some(Entry {
                mask: char_mask(&text),
                chars: text.chars().count().min(u16::MAX as usize) as u16,
                text,
            }));
        }
        Self { terms: entries, by_text, sorted, free: Vec::new() }
    }
}

/// A 64-bit summary of which characters a string contains.
///
/// The hash is a multiply-shift rather than `c as u32 % 64`, because the latter
/// puts `a`–`z` into only 26 of the 64 buckets and leaves the mask nearly saturated
/// for any ordinary English word, which would prune nothing.
fn char_mask(text: &str) -> u64 {
    text.chars().fold(0u64, |mask, ch| {
        mask | 1u64 << ((ch as u32).wrapping_mul(0x9E37_79B1) >> 26)
    })
}

/// Levenshtein distance between `a` and `b`, or `None` if it exceeds `budget`.
///
/// A banded two-row DP: only the diagonal band of width `2 * budget + 1` can hold
/// values within the budget, so the rest of each row is never computed. A row whose
/// minimum already exceeds the budget can only grow, so the whole comparison is
/// abandoned there — which is what makes the common case (a term that is nothing
/// like the query and survived the pre-filters by luck) cost a handful of cells
/// rather than a full matrix.
pub fn bounded_levenshtein(a: &[char], b: &str, budget: u32) -> Option<u32> {
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > budget as usize {
        return None;
    }
    if budget == 0 {
        return (a == b.as_slice()).then_some(0);
    }

    let budget = budget as usize;
    let infinite = budget as u32 + 1;
    let mut previous: Vec<u32> = (0..=m).map(|j| j.min(infinite as usize) as u32).collect();
    let mut current = vec![0u32; m + 1];

    for (i, &ca) in a.iter().enumerate() {
        let i = i + 1;
        let lo = i.saturating_sub(budget);
        let hi = (i + budget).min(m);
        current[0] = i.min(infinite as usize) as u32;
        let mut row_min = current[0];
        for j in 1..=m {
            if j < lo || j > hi {
                current[j] = infinite;
                continue;
            }
            let substitute = previous[j - 1] + u32::from(ca != b[j - 1]);
            let delete = previous[j].saturating_add(1);
            let insert = current[j - 1].saturating_add(1);
            let best = substitute.min(delete).min(insert).min(infinite);
            current[j] = best;
            row_min = row_min.min(best);
        }
        if row_min > budget as u32 {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }

    let distance = previous[m];
    (distance <= budget as u32).then_some(distance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dictionary(terms: &[&str]) -> Dictionary {
        let mut dictionary = Dictionary::default();
        for term in terms {
            dictionary.intern(term);
        }
        dictionary
    }

    #[test]
    fn interning_is_idempotent_and_ids_are_stable_across_later_inserts() {
        let mut dictionary = Dictionary::default();
        let cooling = dictionary.intern("cooling");
        assert_eq!(dictionary.intern("cooling"), cooling);

        // `air` sorts before `cooling`; the id must not move.
        dictionary.intern("air");
        assert_eq!(dictionary.lookup("cooling"), Some(cooling));
        assert_eq!(dictionary.text(cooling), Some("cooling"));
    }

    #[test]
    fn prefix_search_returns_the_whole_range_and_nothing_after_it() {
        let dictionary = dictionary(&["coo", "cool", "coolant", "cooling", "cop", "zebra", "a"]);
        let found: Vec<_> =
            dictionary.with_prefix("cool").map(|id| dictionary.text(id).unwrap()).collect();
        assert_eq!(found, ["cool", "coolant", "cooling"], "in text order");

        let none: Vec<_> = dictionary.with_prefix("cooler").collect();
        assert!(none.is_empty());

        let all: Vec<_> = dictionary.with_prefix("").collect();
        assert_eq!(all.len(), 7, "the empty prefix is every term");
    }

    #[test]
    fn substring_search_finds_matches_that_are_not_prefixes() {
        let dictionary = dictionary(&["cooling", "precool", "heater"]);
        let found: Vec<_> =
            dictionary.containing("cool").map(|id| dictionary.text(id).unwrap()).collect();
        assert_eq!(found, ["cooling", "precool"]);
    }

    #[test]
    fn removing_a_term_takes_it_out_of_every_view_and_frees_its_id() {
        let mut dictionary = dictionary(&["cool", "coolant", "cooling"]);
        let coolant = dictionary.lookup("coolant").unwrap();
        dictionary.remove(coolant);

        assert_eq!(dictionary.lookup("coolant"), None);
        assert_eq!(dictionary.text(coolant), None);
        assert_eq!(dictionary.len(), 2);
        let found: Vec<_> =
            dictionary.with_prefix("cool").map(|id| dictionary.text(id).unwrap()).collect();
        assert_eq!(found, ["cool", "cooling"]);

        // The freed slot is reused rather than the id space growing forever.
        let capacity = dictionary.capacity();
        let fresh = dictionary.intern("condenser");
        assert_eq!(fresh, coolant);
        assert_eq!(dictionary.capacity(), capacity);
    }

    #[test]
    fn removing_every_term_leaves_an_empty_dictionary() {
        let mut dictionary = dictionary(&["a", "b", "c"]);
        for id in dictionary.sorted_ids().to_vec() {
            dictionary.remove(id);
        }
        assert!(dictionary.is_empty());
        assert_eq!(dictionary.with_prefix("").count(), 0);
        assert_eq!(dictionary.within_edits("a", 1).count(), 0);
    }

    #[test]
    fn edit_distance_matches_hand_computed_values() {
        let query: Vec<char> = "cooling".chars().collect();
        assert_eq!(bounded_levenshtein(&query, "cooling", 2), Some(0));
        assert_eq!(bounded_levenshtein(&query, "coolign", 2), Some(2), "transposition is two");
        assert_eq!(bounded_levenshtein(&query, "colling", 2), Some(1), "substitution");
        assert_eq!(bounded_levenshtein(&query, "coling", 2), Some(1), "deletion");
        assert_eq!(bounded_levenshtein(&query, "coooling", 2), Some(1), "insertion");
        assert_eq!(bounded_levenshtein(&query, "heater", 2), None);
        assert_eq!(bounded_levenshtein(&query, "", 2), None);
    }

    #[test]
    fn edit_distance_counts_characters_not_bytes() {
        let query: Vec<char> = "motoröl".chars().collect();
        assert_eq!(bounded_levenshtein(&query, "motorol", 1), Some(1));
        assert_eq!(bounded_levenshtein(&query, "motoröl", 0), Some(0));
    }

    /// The pre-filters are an optimisation, so their only correctness obligation is
    /// that they agree exactly with an unfiltered scan. Asserted rather than
    /// assumed, because a mask that over-prunes loses results silently.
    #[test]
    fn the_substring_prefilter_never_rejects_a_real_match() {
        let words = [
            "cooling", "coolinghose", "precooling", "hose", "os", "o", "", "oo", "ling", "ing",
            "motoröl", "öl", "11517586925", "586", "sensor4821", "482", "aaa", "aa",
        ];
        let dictionary = dictionary(&words);
        for needle in words {
            let mut expected: Vec<&str> =
                words.iter().copied().filter(|w| w.contains(needle)).collect();
            let mut found: Vec<&str> =
                dictionary.containing(needle).map(|id| dictionary.text(id).unwrap()).collect();
            expected.sort_unstable();
            expected.dedup();
            found.sort_unstable();
            assert_eq!(found, expected, "needle {needle:?}");
        }
    }

    #[test]
    fn the_fuzzy_prefilters_never_reject_a_real_match() {
        let words = [
            "cooling", "coolant", "colling", "cooler", "heater", "fan", "fane", "ecu", "ec",
            "ecus", "wiring", "wiring2", "motoröl", "motorol", "a", "ab", "abc",
        ];
        let dictionary = dictionary(&words);
        for query in words {
            for budget in 0..=2u32 {
                let chars: Vec<char> = query.chars().collect();
                let mut expected: Vec<_> = words
                    .iter()
                    .filter_map(|w| bounded_levenshtein(&chars, w, budget).map(|d| ((*w).to_owned(), d)))
                    .collect();
                let mut found: Vec<_> = dictionary
                    .within_edits(query, budget)
                    .map(|(id, d)| (dictionary.text(id).unwrap().to_owned(), d))
                    .collect();
                expected.sort();
                found.sort();
                assert_eq!(found, expected, "query {query:?} budget {budget}");
            }
        }
    }

    #[test]
    fn a_dictionary_rebuilt_from_a_sorted_list_behaves_identically() {
        let terms = ["air", "cool", "coolant", "cooling", "zebra"];
        let rebuilt = Dictionary::from_sorted(terms.iter().map(|t| (*t).into()).collect());
        assert_eq!(rebuilt.len(), terms.len());
        assert_eq!(rebuilt.sorted_ids(), &[0, 1, 2, 3, 4]);
        let found: Vec<_> =
            rebuilt.with_prefix("cool").map(|id| rebuilt.text(id).unwrap()).collect();
        assert_eq!(found, ["cool", "coolant", "cooling"]);
        assert_eq!(rebuilt.lookup("zebra"), Some(4));
    }
}
