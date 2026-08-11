//! Ranking, and the guarantee that it never reshuffles.
//!
//! # The ordering, in full
//!
//! A hit's rank is a lexicographic key. Every component is an integer or a string
//! that is already in the index — there is no tuned float anywhere, and no term
//! statistics, which means two runs of the same query on the same index produce not
//! merely the same *set* but the same *sequence*, byte for byte.
//!
//! 1. **[`MatchTier`] — how the query reached the item.** Exact beats prefix beats
//!    substring beats fuzzy, and one typo beats two. In a multi-term query the
//!    document takes the *worst*
//!    tier of its terms: a document that matched one word exactly and guessed at
//!    the other is a guess, and ranking it above a document that matched both
//!    exactly would be a lie.
//! 2. **[`Field`] — text before facet.** An item whose text says "frame" is a
//!    better answer to `frame` than an item that merely *is* a frame. Both are
//!    returned; the one the user wrote wins.
//! 3. **Position — earlier is better.** The ordinal of the earliest matching term
//!    in the item's text. A sticky titled "Cooling" outranks one that mentions
//!    cooling in its ninth word, which is the same instinct that puts a title match
//!    above a body match without needing separate fields.
//! 4. **Occurrences — more is better.** The only frequency signal, and it is a
//!    tie-break rather than a score: it decides between two items that matched at
//!    the same tier in the same place, and nothing else.
//! 5. **`(board, item)` — lexicographic.** Not a relevance signal at all. It exists
//!    because the first four can tie and *something* has to be last, and because
//!    `(board, item)` is the index's primary key it is a **total** order. That is
//!    the whole guarantee: no two hits can compare equal, so the sort has no
//!    freedom left, and results cannot move between identical queries.
//!
//! # Why not BM25
//!
//! BM25 is the right answer for a corpus of documents. This is a corpus of stickies
//! — the reference board's 429 text strings average a few words each — where the
//! term-frequency and length-normalisation terms are computed over documents too
//! short for either to mean anything, and where `idf` shifts every time an item is
//! edited, so the same query would reorder its own results while the user typed.
//! The ordering above is worse at ranking a book and better at ranking a wall of
//! notes, which is what a board is.

use crate::snippet::Snippet;
use crate::colour::Colour;

/// How a query term reached an item. Lower ranks better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchTier {
    /// The term is the item's term, character for character after case folding.
    Exact,
    /// The item's term begins with the query term. What as-you-type search lives
    /// on: `cool` finds `cooling` before the user has finished the word.
    Prefix,
    /// The query term appears inside the item's term, but not at its start —
    /// `pump` in `waterpump`. A weaker signal than a prefix, and a much stronger
    /// one than a typo.
    Substring,
    /// The item's term is within the query's edit budget. Ranked last because it is
    /// the only tier that matches something the user did not type.
    ///
    /// The distance is carried rather than discarded, and the derived `Ord` puts
    /// `Fuzzy { edits: 1 }` above `Fuzzy { edits: 2 }`: `coolent` is one edit from
    /// `coolant` and two from `cooling`, and showing the second first would be
    /// visibly the wrong guess.
    Fuzzy { edits: u8 },
}

impl MatchTier {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
            Self::Substring => "substring",
            Self::Fuzzy { .. } => "fuzzy",
        }
    }

    /// Whether the query reached the item only by tolerating a typo. A UI may want
    /// to mark these, since they are the only hits that match something the user
    /// did not type.
    pub const fn is_fuzzy(self) -> bool {
        matches!(self, Self::Fuzzy { .. })
    }
}

/// Where in the item the match landed. Lower ranks better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    /// The item's own text.
    Text,
    /// One of its attributes — kind, colour, tag, board. Matched when a bare query
    /// word names one, so that `every frame` and `all yellow stickies` work at all.
    Facet,
}

impl Field {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Facet => "facet",
        }
    }
}

/// Why a hit ranked where it did.
///
/// Public, and carried on every [`Hit`], because a search that cannot explain
/// itself cannot be debugged — and because a UI may legitimately want to show the
/// tier, greying a fuzzy result or labelling a facet match "yellow sticky".
///
/// `Ord` is implemented rather than derived: `occurrences` sorts *descending* while
/// everything before it sorts ascending, which no derive can express. **`Less`
/// means "ranks higher"**, so `Vec<Hit>` sorted ascending is best-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Score {
    /// The worst tier among the query's terms.
    pub tier: MatchTier,
    /// The worst field among the query's terms.
    pub field: Field,
    /// Token ordinal of the earliest match in the item's text. `0` for facet
    /// matches, which have no position of their own.
    pub position: u32,
    /// How many matching tokens the item holds, summed over the query's terms.
    pub occurrences: u32,
}

impl Score {
    pub(crate) const BEST: Self =
        Self { tier: MatchTier::Exact, field: Field::Text, position: u32::MAX, occurrences: 0 };
}

impl Ord for Score {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.tier
            .cmp(&other.tier)
            .then(self.field.cmp(&other.field))
            .then(self.position.cmp(&other.position))
            .then(other.occurrences.cmp(&self.occurrences))
    }
}

impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// One result, carrying everything needed to jump straight to it.
///
/// `board` and `item` are the caller's own ids, verbatim — the pair is enough to
/// open the board and select the item, with no lookup back through this crate. The
/// snippet is built from text the index stored at write time, so producing a full
/// result page touches no board file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The board's id, as given to [`SearchItem::board`](crate::SearchItem::board).
    pub board: String,
    /// The item's id within that board.
    pub item: String,
    /// The item's kind tag, for an icon or a type label beside the result.
    pub kind: String,
    /// The item's colour, for a swatch beside the result.
    pub colour: Option<Colour>,
    /// The item's tags.
    pub tags: Vec<String>,
    /// Why it ranked here. See [`Score`].
    pub score: Score,
    /// A window of the item's text with the matched spans marked.
    pub snippet: Snippet,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(tier: MatchTier, field: Field, position: u32, occurrences: u32) -> Score {
        Score { tier, field, position, occurrences }
    }

    #[test]
    fn tier_dominates_everything_below_it() {
        let exact_late = score(MatchTier::Exact, Field::Text, 900, 1);
        let prefix_first = score(MatchTier::Prefix, Field::Text, 0, 99);
        assert!(exact_late < prefix_first, "a late exact match beats an early prefix one");

        let one_typo = score(MatchTier::Fuzzy { edits: 1 }, Field::Text, 900, 1);
        let two_typos = score(MatchTier::Fuzzy { edits: 2 }, Field::Text, 0, 99);
        assert!(one_typo < two_typos);
    }

    #[test]
    fn the_tiers_are_ordered_exact_prefix_substring_then_fuzzy_by_distance() {
        let mut tiers = [
            MatchTier::Fuzzy { edits: 2 },
            MatchTier::Exact,
            MatchTier::Fuzzy { edits: 1 },
            MatchTier::Substring,
            MatchTier::Prefix,
        ];
        tiers.sort();
        assert_eq!(
            tiers,
            [
                MatchTier::Exact,
                MatchTier::Prefix,
                MatchTier::Substring,
                MatchTier::Fuzzy { edits: 1 },
                MatchTier::Fuzzy { edits: 2 },
            ]
        );
        assert!(MatchTier::Fuzzy { edits: 2 }.is_fuzzy());
        assert!(!MatchTier::Prefix.is_fuzzy());
    }

    #[test]
    fn text_outranks_a_facet_match_at_the_same_tier() {
        let text = score(MatchTier::Exact, Field::Text, 40, 1);
        let facet = score(MatchTier::Exact, Field::Facet, 0, 1);
        assert!(text < facet);
    }

    #[test]
    fn an_earlier_position_wins_and_then_more_occurrences_wins() {
        let early = score(MatchTier::Exact, Field::Text, 0, 1);
        let late = score(MatchTier::Exact, Field::Text, 7, 50);
        assert!(early < late);

        let many = score(MatchTier::Exact, Field::Text, 3, 9);
        let few = score(MatchTier::Exact, Field::Text, 3, 1);
        assert!(many < few, "occurrences is the one component that sorts descending");
    }

    #[test]
    fn the_best_sentinel_is_the_identity_for_worsening() {
        let real = score(MatchTier::Prefix, Field::Facet, 12, 3);
        assert!(Score::BEST.tier <= real.tier, "tier worsens by taking the maximum");
        assert!(Score::BEST.field <= real.field, "and so does field");
        assert_eq!(
            Score::BEST.position,
            u32::MAX,
            "position worsens by taking the minimum, so it starts at the other end",
        );
    }
}
