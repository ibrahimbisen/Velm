//! What the user typed, turned into something the index can execute.
//!
//! # Parsing cannot fail
//!
//! A search box is fed a partial query on every keystroke: `"cool` has an
//! unbalanced quote, `kind:` has no value, `::` is nothing at all. Each of those is
//! a state the user passes *through* on the way to what they meant, so none of them
//! is an error. An unterminated quote runs to the end of the input, an empty filter
//! value is dropped, and anything that is not recognised is treated as words. The
//! result is always a runnable query, sometimes an empty one.
//!
//! # The syntax
//!
//! | Typed | Means |
//! |---|---|
//! | `cooling fan` | both terms, in any order and anywhere in the item — AND, not OR |
//! | `"cooling fan"` | the two words adjacent and in that order |
//! | `"fan"` | exactly `fan` — a one-word phrase is how you switch prefix and fuzzy matching off for a single term |
//! | `kind:sticky` | only stickies. Also `type:` |
//! | `colour:yellow` `colour:#fff79e` | by name or by hex. Also `color:` |
//! | `board:site-plan` | only that board |
//! | `tag:review` | only items carrying that tag |
//! | `kind:"link preview"` | a filter value with a space in it |
//!
//! Filters of the same kind are OR-ed (`kind:sticky kind:frame` is either);
//! different kinds are AND-ed (`kind:sticky colour:yellow` is both). That is the
//! only combination anybody means by it.
//!
//! # Bare words that name a kind or a colour
//!
//! `every frame` and `all yellow stickies` are how people search a *visual* board,
//! and neither contains a filter. So a bare word that names a kind present in the
//! index, or a colour, matches items by that attribute **as well as** by text — see
//! [`Matching::facets`]. `frame` returns frames and items whose text says "frame",
//! with the text matches ranked first (`crate::rank` §2).
//!
//! Two supporting pieces make that read naturally rather than literally:
//! [`crate::source::resolve_kind`] accepts plurals and everyday synonyms, so
//! `stickies`, `notes` and `sticker` all reach `sticky`; and the determiners in
//! [`STOPWORDS`] are dropped, so `every` and `all` do not have to match anything.
//! A stopword is only dropped when a real term remains — `the` on its own is a
//! search for the word "the" — and quoting one (`"all"`) always searches for it.

use crate::colour::Colour;
use crate::token;

/// Words dropped from a query when at least one other term survives.
///
/// Strictly determiners, quantifiers and the verbs people put in front of a search
/// box. Not a general English stopword list: `and`, `not`, `in` and `for` are all
/// words that appear on real boards as content, and dropping them would make a
/// search for them silently impossible. Quote a stopword to search for it.
pub const STOPWORDS: &[&str] =
    &["a", "all", "an", "any", "every", "find", "me", "my", "search", "show", "the"];

/// One term of a query. All terms must match — the join is AND.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// A single word, matched at whatever tiers [`Matching`] allows.
    Word(String),
    /// Words that must appear adjacent and in order. Always matched exactly: a
    /// phrase is a statement about literal text, and expanding its words by prefix
    /// or edit distance would make `"cooling fan"` match `cooled fans`, which is
    /// not what quotes mean anywhere else.
    Phrase(Vec<String>),
}

/// Which kinds of match a query is willing to accept.
///
/// The default is what an interactive search box wants: prefix on, because a
/// half-typed word must find things; substring on, because Miro text is full of
/// compounds; fuzzy off, because typo tolerance changes results *while the user is
/// still typing the word* and reads as the search guessing wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Matching {
    /// Match items whose term starts with the query term.
    pub prefix: bool,
    /// Match items whose term contains the query term anywhere.
    pub substring: bool,
    /// Maximum edit distance, capped at 2 and scaled down for short terms by
    /// [`Matching::budget_for`]. `0` disables fuzzy matching.
    pub fuzzy: u8,
    /// Let bare words that name a kind or colour match by that attribute.
    pub facets: bool,
}

impl Default for Matching {
    fn default() -> Self {
        Self::interactive()
    }
}

impl Matching {
    /// Whole terms only. What a phrase uses, and what "find exactly this" means.
    pub const EXACT: Self =
        Self { prefix: false, substring: false, fuzzy: 0, facets: false };

    /// Prefix and substring, no typo tolerance. The default.
    pub const fn interactive() -> Self {
        Self { prefix: true, substring: true, fuzzy: 0, facets: true }
    }

    /// Interactive, plus up to two edits. For a query that returned nothing, or a
    /// deliberate "did you mean" pass.
    pub const fn forgiving() -> Self {
        Self { prefix: true, substring: true, fuzzy: 2, facets: true }
    }

    pub const fn with_fuzzy(mut self, budget: u8) -> Self {
        self.fuzzy = if budget > 2 { 2 } else { budget };
        self
    }

    /// The edit budget actually allowed for `term`.
    ///
    /// Short terms get less, and terms of three characters or fewer get none. One
    /// edit on a three-letter word reaches an enormous slice of any dictionary —
    /// `ecu` is one edit from `eau`, `ec`, `ecus`, `cu`, `emu` and hundreds more —
    /// so the results stop being about what was typed. The scale is the one every
    /// mature search engine converges on for the same reason.
    pub fn budget_for(&self, term: &str) -> u32 {
        if self.fuzzy == 0 {
            return 0;
        }
        match term.chars().count() {
            0..=3 => 0,
            4..=5 => 1,
            _ => u32::from(self.fuzzy).min(2),
        }
    }
}

/// A colour filter: an exact value, or a name from [`crate::colour::COLOUR_NAMES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColourFilter {
    Exact(Colour),
    Named(String),
}

impl ColourFilter {
    /// Parses a filter value: hex first, then a name. `#fff79e` is exact,
    /// `yellow` is a band.
    pub fn parse(value: &str) -> Self {
        match Colour::parse(value) {
            Some(colour) => Self::Exact(colour),
            None => Self::Named(value.to_ascii_lowercase()),
        }
    }

    /// Whether `colour` satisfies this filter. An item with no colour satisfies no
    /// colour filter — including `colour:white`, because "unstyled" is not white.
    pub fn matches(&self, colour: Option<Colour>) -> bool {
        match (self, colour) {
            (_, None) => false,
            (Self::Exact(wanted), Some(actual)) => *wanted == actual,
            (Self::Named(name), Some(actual)) => actual.is_named(name),
        }
    }
}

/// Attribute constraints. Empty lists constrain nothing.
///
/// OR within a list, AND across lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    pub boards: Vec<String>,
    pub kinds: Vec<String>,
    pub colours: Vec<ColourFilter>,
    pub tags: Vec<String>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.boards.is_empty()
            && self.kinds.is_empty()
            && self.colours.is_empty()
            && self.tags.is_empty()
    }

    /// Whether an item's attributes satisfy every non-empty list.
    pub fn accepts(
        &self,
        board: &str,
        kind: &str,
        colour: Option<Colour>,
        tags: &[Box<str>],
    ) -> bool {
        (self.boards.is_empty() || self.boards.iter().any(|b| b.eq_ignore_ascii_case(board)))
            && (self.kinds.is_empty() || self.kinds.iter().any(|k| k.eq_ignore_ascii_case(kind)))
            && (self.colours.is_empty() || self.colours.iter().any(|c| c.matches(colour)))
            && (self.tags.is_empty()
                || self.tags.iter().any(|t| tags.iter().any(|has| t.eq_ignore_ascii_case(has))))
    }
}

/// How many hits a query returns unless told otherwise.
///
/// Enough to scroll a result panel, few enough that ranking 50,000 matched items
/// costs a bounded heap rather than a full sort. Pass `usize::MAX` to
/// [`Query::with_limit`] for "every match", which is what a *select* by colour or
/// kind wants — `docs/features/README.md` §3, "Select all / by type / by colour".
pub const DEFAULT_LIMIT: usize = 200;

/// A parsed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    terms: Vec<Term>,
    filters: Filters,
    matching: Matching,
    limit: usize,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            terms: Vec::new(),
            filters: Filters::default(),
            matching: Matching::default(),
            limit: DEFAULT_LIMIT,
        }
    }
}

impl Query {
    /// An empty query, which matches nothing. Build one up with the methods below,
    /// or use [`Query::parse`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses the syntax in the module docs. Never fails; see the module docs.
    pub fn parse(input: &str) -> Self {
        let mut query = Self::new();
        let mut words: Vec<String> = Vec::new();

        for chunk in scan(input) {
            match chunk.key.as_deref().and_then(FilterKey::of) {
                Some(key) if !chunk.value.trim().is_empty() => {
                    query.filters.push(key, chunk.value.trim());
                }
                Some(_) => {}
                None => {
                    // Either a real term, or a `key:value` whose key names no
                    // filter — `http://x` and `10:30` both land here, and both are
                    // more useful as words than as a dropped filter.
                    let raw = match &chunk.key {
                        Some(key) => format!("{key}:{}", chunk.value),
                        None => chunk.value.clone(),
                    };
                    if chunk.quoted {
                        let phrase: Vec<String> = token::terms(&raw).collect();
                        match phrase.len() {
                            0 => {}
                            // A one-word phrase is a term that refuses expansion,
                            // which is exactly a one-word `Phrase`.
                            _ => query.terms.push(Term::Phrase(phrase)),
                        }
                    } else {
                        words.extend(token::terms(&raw));
                    }
                }
            }
        }

        let kept: Vec<String> = words.iter().filter(|w| !is_stopword(w)).cloned().collect();
        let words = if kept.is_empty() { words } else { kept };
        query.terms.extend(words.into_iter().map(Term::Word));
        query
    }

    pub fn terms(&self) -> &[Term] {
        &self.terms
    }

    pub fn filters(&self) -> &Filters {
        &self.filters
    }

    pub fn matching(&self) -> Matching {
        self.matching
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// A query with neither terms nor filters matches nothing, and
    /// [`Index::search`](crate::Index::search) short-circuits on it. An empty search
    /// box shows an empty result list, not the whole corpus.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.filters.is_empty()
    }

    pub fn with_matching(mut self, matching: Matching) -> Self {
        self.matching = matching;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_term(mut self, word: &str) -> Self {
        self.terms.extend(token::terms(word).map(Term::Word));
        self
    }

    pub fn with_phrase(mut self, phrase: &str) -> Self {
        let words: Vec<String> = token::terms(phrase).collect();
        if !words.is_empty() {
            self.terms.push(Term::Phrase(words));
        }
        self
    }

    pub fn in_board(mut self, board: impl Into<String>) -> Self {
        self.filters.boards.push(board.into());
        self
    }

    pub fn of_kind(mut self, kind: impl Into<String>) -> Self {
        self.filters.kinds.push(kind.into());
        self
    }

    pub fn coloured(mut self, colour: ColourFilter) -> Self {
        self.filters.colours.push(colour);
        self
    }

    pub fn tagged(mut self, tag: impl Into<String>) -> Self {
        self.filters.tags.push(tag.into());
        self
    }
}

fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKey {
    Board,
    Kind,
    Colour,
    Tag,
}

impl FilterKey {
    fn of(key: &str) -> Option<Self> {
        // Both spellings of colour, and `type` for kind, because Miro's own UI and
        // this codebase disagree about all three and the user should not have to
        // remember which one won.
        match key.to_ascii_lowercase().as_str() {
            "board" => Some(Self::Board),
            "kind" | "type" => Some(Self::Kind),
            "colour" | "color" => Some(Self::Colour),
            "tag" | "label" => Some(Self::Tag),
            _ => None,
        }
    }
}

impl Filters {
    fn push(&mut self, key: FilterKey, value: &str) {
        match key {
            FilterKey::Board => self.boards.push(value.to_owned()),
            // `link preview` and `link-preview` both become the `link_preview` tag
            // that `vellum_doc::ItemKind::tag` produces.
            FilterKey::Kind => self
                .kinds
                .push(value.to_ascii_lowercase().replace([' ', '-'], "_")),
            FilterKey::Colour => self.colours.push(ColourFilter::parse(value)),
            FilterKey::Tag => self.tags.push(value.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Chunk {
    key: Option<String>,
    value: String,
    quoted: bool,
}

/// Splits raw input into whitespace-separated chunks, honouring quotes and a
/// single leading `key:`.
///
/// One pass, no backtracking, and no failure mode. An unterminated quote consumes
/// the rest of the input, which is what makes `"cool` behave as the prefix of the
/// phrase the user is in the middle of typing rather than as a syntax error.
fn scan(input: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut key: Option<String> = None;
    let mut value = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let mut started = false;

    for ch in input.chars() {
        if in_quotes {
            if ch == '"' {
                in_quotes = false;
            } else {
                value.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                in_quotes = true;
                quoted = true;
                started = true;
            }
            // Only the first colon of an unquoted chunk splits it, so `10:30:00`
            // keeps its tail and `board:a:b` filters on the board `a:b`.
            ':' if key.is_none() && !quoted && !value.is_empty() => {
                key = Some(std::mem::take(&mut value));
                started = true;
            }
            _ if ch.is_whitespace() => {
                if started || !value.is_empty() {
                    chunks.push(Chunk {
                        key: key.take(),
                        value: std::mem::take(&mut value),
                        quoted,
                    });
                    quoted = false;
                    started = false;
                }
            }
            _ => {
                value.push(ch);
                started = true;
            }
        }
    }
    if started || !value.is_empty() {
        chunks.push(Chunk { key, value, quoted });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(query: &Query) -> Vec<&str> {
        query
            .terms()
            .iter()
            .filter_map(|t| match t {
                Term::Word(w) => Some(w.as_str()),
                Term::Phrase(_) => None,
            })
            .collect()
    }

    #[test]
    fn bare_words_become_and_ed_terms_and_are_case_folded() {
        let query = Query::parse("Cooling FAN");
        assert_eq!(words(&query), ["cooling", "fan"]);
        assert!(query.filters().is_empty());
    }

    #[test]
    fn punctuation_inside_a_bare_chunk_splits_it_into_terms() {
        assert_eq!(words(&Query::parse("cooling-fan")), ["cooling", "fan"]);
    }

    #[test]
    fn quotes_make_a_phrase_and_a_single_quoted_word_refuses_expansion() {
        let query = Query::parse("\"cooling fan\"");
        assert_eq!(query.terms(), [Term::Phrase(vec!["cooling".into(), "fan".into()])]);

        let query = Query::parse("\"fan\"");
        assert_eq!(query.terms(), [Term::Phrase(vec!["fan".into()])]);
    }

    #[test]
    fn an_unterminated_quote_is_a_phrase_in_progress_not_an_error() {
        let query = Query::parse("relay \"cooling fa");
        assert_eq!(
            query.terms(),
            [
                Term::Phrase(vec!["cooling".into(), "fa".into()]),
                Term::Word("relay".into()),
            ]
        );
    }

    #[test]
    fn filters_are_recognised_under_every_spelling_the_codebase_uses() {
        let query = Query::parse("kind:sticky type:frame colour:yellow color:#fff79e tag:review board:garage");
        assert_eq!(query.filters().kinds, ["sticky", "frame"]);
        assert_eq!(
            query.filters().colours,
            [
                ColourFilter::Named("yellow".into()),
                ColourFilter::Exact(Colour::parse("#fff79e").unwrap())
            ]
        );
        assert_eq!(query.filters().tags, ["review"]);
        assert_eq!(query.filters().boards, ["garage"]);
        assert!(query.terms().is_empty(), "a filter is not also a term");
    }

    #[test]
    fn a_quoted_filter_value_keeps_its_space_and_normalises_to_a_kind_tag() {
        let query = Query::parse("kind:\"link preview\"");
        assert_eq!(query.filters().kinds, ["link_preview"]);
        assert_eq!(Query::parse("kind:link-preview").filters().kinds, ["link_preview"]);
    }

    #[test]
    fn an_unrecognised_key_stays_a_term_rather_than_being_swallowed() {
        let query = Query::parse("https://miro.com/app");
        assert!(query.filters().is_empty());
        assert_eq!(words(&query), ["https", "miro", "com", "app"]);
    }

    #[test]
    fn a_filter_with_no_value_is_dropped_rather_than_matching_nothing() {
        let query = Query::parse("kind: cooling");
        assert!(query.filters().is_empty());
        assert_eq!(words(&query), ["cooling"]);
    }

    #[test]
    fn stopwords_go_only_when_a_real_term_remains() {
        assert_eq!(words(&Query::parse("all yellow stickies")), ["yellow", "stickies"]);
        assert_eq!(words(&Query::parse("every frame")), ["frame"]);
        assert_eq!(words(&Query::parse("the")), ["the"], "on its own it is the query");
        assert_eq!(
            Query::parse("\"all\"").terms(),
            [Term::Phrase(vec!["all".into()])],
            "quoting always searches for the word"
        );
    }

    #[test]
    fn an_empty_or_punctuation_only_query_is_empty() {
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   ").is_empty());
        assert!(Query::parse("!!! ???").is_empty());
        assert!(Query::parse("::").is_empty());
        assert!(!Query::parse("kind:frame").is_empty());
    }

    #[test]
    fn the_fuzzy_budget_shrinks_with_the_term_and_never_exceeds_two() {
        let forgiving = Matching::forgiving();
        assert_eq!(forgiving.budget_for("ecu"), 0, "three letters is too short to guess at");
        assert_eq!(forgiving.budget_for("fan"), 0);
        assert_eq!(forgiving.budget_for("relay"), 1);
        assert_eq!(forgiving.budget_for("cooling"), 2);
        assert_eq!(Matching::interactive().budget_for("cooling"), 0, "off by default");
        assert_eq!(Matching::default().with_fuzzy(9).fuzzy, 2, "capped");
    }

    #[test]
    fn colour_filters_never_accept_an_item_with_no_colour() {
        let yellow = ColourFilter::parse("yellow");
        assert!(yellow.matches(Colour::parse("#fff79e")));
        assert!(!yellow.matches(None));
        assert!(!ColourFilter::parse("white").matches(None), "unstyled is not white");

        let exact = ColourFilter::parse("#FFF79E");
        assert!(exact.matches(Colour::parse("#fff79e")));
        assert!(!exact.matches(Colour::parse("#fff79f")));
    }

    #[test]
    fn filters_are_or_within_a_kind_and_and_across_kinds() {
        let filters = Query::parse("kind:sticky kind:frame colour:yellow").filters().clone();
        let yellow = Colour::parse("#fff79e");
        assert!(filters.accepts("b", "sticky", yellow, &[]));
        assert!(filters.accepts("b", "frame", yellow, &[]));
        assert!(!filters.accepts("b", "ink", yellow, &[]), "kind is not in the list");
        assert!(!filters.accepts("b", "sticky", None, &[]), "colour is required too");
    }

    #[test]
    fn tag_filters_are_case_insensitive() {
        let filters = Query::parse("tag:Review").filters().clone();
        assert!(filters.accepts("b", "sticky", None, &["review".into()]));
        assert!(!filters.accepts("b", "sticky", None, &["done".into()]));
    }

    #[test]
    fn the_builder_and_the_parser_agree() {
        let parsed = Query::parse("cooling kind:sticky");
        let built = Query::new().with_term("cooling").of_kind("sticky");
        assert_eq!(parsed.terms(), built.terms());
        assert_eq!(parsed.filters(), built.filters());
    }
}
