//! Tokenisation — the one rule the index and every query must agree on.
//!
//! This module is small and load-bearing out of all proportion to its size. If the
//! indexer and the query parser disagree about where a word ends, the index is not
//! *slightly* wrong, it is unusable: the term that was stored is not the term that
//! is looked up, and the search returns nothing while looking like it works. So
//! there is exactly one tokeniser, and both sides call it.
//!
//! # The rule
//!
//! A term is a maximal run of [`char::is_alphanumeric`], case-folded. Everything
//! else — spaces, punctuation, emoji, the `<p>` tags Miro's rich text arrives
//! wrapped in — separates terms and is never indexed.
//!
//! Two consequences worth stating, because both look like defects until the reason
//! is written down:
//!
//! - **`4k` is one term.** Alphanumeric runs are not split at the letter/digit
//!   boundary. Boards are full of model numbers and measurements — `4k`, `12mm`,
//!   `v3` — and splitting `4k` into `4` and `k` would mean a search for `4k` had to
//!   be reassembled as a phrase query, while a single stray `k` would be in the
//!   dictionary matching everything.
//! - **`don't` is two terms, `don` and `t`.** An apostrophe separates. This is not
//!   ideal English, but it is *symmetric*: a query for `don't` tokenises the same
//!   way, so the phrase still matches. Making the apostrophe a word character
//!   instead would mean `dont` no longer finds `don't`, which is the commoner
//!   mistake by far.
//!
//! # Positions are recorded twice, on purpose
//!
//! Each token carries both its **ordinal** (`index`) and its **byte span** in the
//! source string, because they answer different questions and neither substitutes
//! for the other:
//!
//! - The ordinal is what phrase matching compares (`"cooling fan"` is two terms at
//!   consecutive ordinals) and what ranking calls "match position". It is what the
//!   posting lists store, at 4 bytes rather than 8.
//! - The byte span is what a snippet highlights. It indexes the *original* text,
//!   not the folded term, so `Cooling` is highlighted with its capital intact.

use std::borrow::Cow;

/// The longest alphanumeric run kept as a single term, in bytes.
///
/// Runs longer than this are split at the limit rather than truncated or dropped.
/// The case this exists for is real: Miro embeds carry base64 payloads and data
/// URIs, and one of those becoming a single 40KB dictionary term would be scanned
/// character-by-character by every substring and fuzzy query for the life of the
/// index, for a "word" nobody will ever search for. Splitting keeps the term
/// dictionary bounded; the cost is that a substring straddling a split boundary is
/// not found, inside a base64 blob, which is not a loss anyone can observe.
pub const MAX_TERM_BYTES: usize = 96;

/// One term, with everything needed to rank it and to highlight it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'a> {
    /// The case-folded term, as stored in the dictionary. Borrowed when the source
    /// run was already lowercase, which on real board text it usually is.
    pub text: Cow<'a, str>,
    /// Byte offset of the run in the *source* string.
    pub start: usize,
    /// Byte offset one past the run, in the source string.
    pub end: usize,
    /// The token's ordinal within the source string, counting from zero.
    pub index: u32,
}

impl Token<'_> {
    /// The token's span in the source, for snippet highlighting.
    pub fn span(&self) -> std::ops::Range<usize> {
        self.start..self.end
    }
}

/// Splits `text` into terms. See the module docs for the rule.
pub fn tokenise(text: &str) -> Tokens<'_> {
    Tokens { text, cursor: 0, index: 0 }
}

/// Iterator over [`tokenise`].
#[derive(Debug, Clone)]
pub struct Tokens<'a> {
    text: &'a str,
    cursor: usize,
    index: u32,
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let mut start = None;
        let mut end = self.text.len();
        let mut taken = 0usize;

        for (offset, ch) in self.text[self.cursor..].char_indices() {
            let at = self.cursor + offset;
            let width = ch.len_utf8();
            match start {
                None => {
                    if ch.is_alphanumeric() {
                        start = Some(at);
                        taken = width;
                    }
                }
                Some(_) => {
                    if !ch.is_alphanumeric() || taken + width > MAX_TERM_BYTES {
                        end = at;
                        break;
                    }
                    taken += width;
                }
            }
        }

        let start = start?;
        let token = Token {
            text: fold(&self.text[start..end]),
            start,
            end,
            index: self.index,
        };
        self.cursor = end;
        self.index += 1;
        Some(token)
    }
}

/// Case-folds one already-extracted run.
///
/// The ASCII path is separated out because it is the overwhelmingly common one on
/// these boards and because it can answer "no change needed" without allocating.
/// Non-ASCII always goes through `to_lowercase`, which is the only correct answer
/// for the cases that matter (`İ` folds to two code points, `Σ` folds contextually)
/// and is not worth trying to predict.
fn fold(run: &str) -> Cow<'_, str> {
    if run.is_ascii() {
        if run.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(run.to_ascii_lowercase())
        } else {
            Cow::Borrowed(run)
        }
    } else {
        Cow::Owned(run.to_lowercase())
    }
}

/// Case-folds a whole string as if it were a single term.
///
/// Used for query words and for facet values, which have already been split by the
/// query parser and must fold by exactly the same rule the indexer used.
pub fn fold_term(term: &str) -> String {
    fold(term).into_owned()
}

/// The terms of `text`, folded, discarding positions.
///
/// A convenience for callers that only need the word list — the query parser, and
/// the facet-value normaliser.
pub fn terms(text: &str) -> impl Iterator<Item = String> + '_ {
    tokenise(text).map(|token| token.text.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        terms(text).collect()
    }

    #[test]
    fn splits_on_everything_that_is_not_alphanumeric() {
        assert_eq!(words("cooling-fan, ECU!"), ["cooling", "fan", "ecu"]);
        assert_eq!(words("<p>fan</p>"), ["p", "fan", "p"]);
        assert_eq!(words("   "), Vec::<String>::new());
        assert_eq!(words(""), Vec::<String>::new());
    }

    /// A board title full of model numbers. Splitting `4k` would be the wrong answer.
    #[test]
    fn a_letter_digit_run_stays_one_term() {
        assert_eq!(words("Panel 2020 4k v3"), ["panel", "2020", "4k", "v3"]);
    }

    #[test]
    fn positions_index_the_source_not_the_folded_text() {
        let text = "Cooling Fan";
        let tokens: Vec<_> = tokenise(text).collect();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].text, "cooling");
        assert_eq!(&text[tokens[0].span()], "Cooling", "the span keeps the capital");
        assert_eq!(tokens[0].index, 0);
        assert_eq!(&text[tokens[1].span()], "Fan");
        assert_eq!(tokens[1].index, 1);
    }

    #[test]
    fn ordinals_are_consecutive_regardless_of_how_much_punctuation_separates_terms() {
        let tokens: Vec<_> = tokenise("a ... b !!!!! c").collect();
        assert_eq!(tokens.iter().map(|t| t.index).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn already_lowercase_ascii_is_borrowed_rather_than_allocated() {
        let tokens: Vec<_> = tokenise("fan ECU").collect();
        assert!(matches!(tokens[0].text, Cow::Borrowed(_)));
        assert!(matches!(tokens[1].text, Cow::Owned(_)));
    }

    #[test]
    fn non_ascii_folds_and_keeps_its_source_span() {
        let text = "MOTORÖL Café";
        let tokens: Vec<_> = tokenise(text).collect();
        assert_eq!(tokens[0].text, "motoröl");
        assert_eq!(&text[tokens[0].span()], "MOTORÖL");
        assert_eq!(tokens[1].text, "café", "already lowercase, still valid");
    }

    #[test]
    fn a_run_longer_than_the_cap_is_split_rather_than_truncated() {
        let long = "a".repeat(MAX_TERM_BYTES * 2 + 5);
        let tokens: Vec<_> = tokenise(&long).collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].text.len(), MAX_TERM_BYTES);
        assert_eq!(tokens[1].text.len(), MAX_TERM_BYTES);
        assert_eq!(tokens[2].text.len(), 5);
        let rejoined: String = tokens.iter().map(|t| t.text.as_ref()).collect();
        assert_eq!(rejoined, long, "splitting loses no characters");
    }

    /// Splitting must not cut a multi-byte character in half, which would panic on
    /// the slice. The cap lands mid-character for any run of 3-byte characters.
    #[test]
    fn the_cap_never_splits_a_character() {
        let text = "€".repeat(MAX_TERM_BYTES);
        let tokens: Vec<_> = tokenise(&text).collect();
        // `€` is not alphanumeric, so this is the degenerate case: no terms at all.
        assert!(tokens.is_empty());

        // A currency sign is not a letter; use one that is.
        let text = "ω".repeat(MAX_TERM_BYTES);
        let tokens: Vec<_> = tokenise(&text).collect();
        assert!(tokens.iter().all(|t| t.text.len() <= MAX_TERM_BYTES));
        let rejoined: String = tokens.iter().map(|t| t.text.as_ref()).collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn fold_term_matches_what_the_indexer_stores() {
        let indexed = tokenise("Cooling").next().unwrap().text.into_owned();
        assert_eq!(fold_term("COOLING"), indexed);
        assert_eq!(fold_term("cooling"), indexed);
    }
}
