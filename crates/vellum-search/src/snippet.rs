//! The bit of text a result actually shows, with the match marked.
//!
//! A hit is only useful if the user can tell, without opening anything, whether it
//! is the one they meant. That needs two things this module provides: a *window*
//! narrow enough to read at a glance, and the matched span marked inside it so the
//! eye lands on the reason the result is there.
//!
//! # Spans, not markup
//!
//! [`Snippet`] carries byte ranges, not a string with `<mark>` in it. The renderer
//! is a GPU text pipeline drawing styled spans (`docs/01-architecture.md` §4, §6),
//! not an HTML view: it needs offsets to apply a highlight run to, and handing it
//! markup would mean it had to parse the markup back into offsets. Callers that do
//! want a string get [`Snippet::render`], which is where escaping belongs.
//!
//! Offsets are into [`Snippet::text`] — the window — not into the item's full text.
//! The window is what gets drawn, so those are the offsets a renderer can use
//! without arithmetic.
//!
//! # The window
//!
//! Centred on the first match, [`SNIPPET_BUDGET`] characters wide, and snapped
//! outward to word boundaries so it never begins or ends mid-word. Every match that
//! falls inside is marked, not only the first. If the window does not start at the
//! beginning of the text, or does not reach its end, the corresponding ellipsis
//! flag is set — as a flag rather than a baked-in `…` so the caller can style it,
//! and so the marked offsets are not shifted by a character the index never saw.

/// How many characters of context a snippet shows.
///
/// Sized for a result row in a command palette rather than for a page of prose:
/// two lines at the 13px body size in `docs/05-design-language.md` §5, in a panel
/// that is not the whole window.
pub const SNIPPET_BUDGET: usize = 140;

/// How much of the budget is spent *before* the first match.
///
/// Deliberately less than half. The words after a match are usually what
/// disambiguates it — "cooling fan relay" versus "cooling fan fuse" — while the
/// words before it are context the user often already has.
const LEAD_IN: usize = 40;

/// A byte range within [`Snippet::text`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn range(self) -> std::ops::Range<usize> {
        self.start..self.end
    }

    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// A window of an item's text with the matched spans marked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snippet {
    /// The window itself, taken verbatim from the item's text — original case,
    /// original punctuation.
    pub text: String,
    /// Matched spans within `text`, ascending and non-overlapping.
    pub spans: Vec<Span>,
    /// The window does not start at the beginning of the item's text.
    pub elided_before: bool,
    /// The window does not reach the end of the item's text.
    pub elided_after: bool,
}

impl Snippet {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The snippet as one string, with each span wrapped in `open`/`close`.
    ///
    /// For tests, logs and any caller that really does want markup. Nothing is
    /// escaped: this crate does not know what `open` and `close` mean, so it cannot
    /// know what would need escaping between them.
    pub fn render(&self, open: &str, close: &str) -> String {
        let mut out = String::with_capacity(self.text.len() + self.spans.len() * 8);
        if self.elided_before {
            out.push('…');
        }
        let mut cursor = 0;
        for span in &self.spans {
            out.push_str(&self.text[cursor..span.start]);
            out.push_str(open);
            out.push_str(&self.text[span.range()]);
            out.push_str(close);
            cursor = span.end;
        }
        out.push_str(&self.text[cursor..]);
        if self.elided_after {
            out.push('…');
        }
        out
    }

    /// Builds a snippet of `text` around `matches`, which are byte ranges into
    /// `text` and need be neither sorted nor disjoint.
    ///
    /// Overlapping matches are merged. That is not a tidiness measure: two query
    /// terms can match the same word — `cool` as a prefix and `cooling` exactly —
    /// and emitting two overlapping highlight runs would make [`Snippet::render`]
    /// produce interleaved tags and a GPU span renderer double-shade the overlap.
    pub fn build(text: &str, matches: &[std::ops::Range<usize>]) -> Self {
        if text.is_empty() {
            return Self::default();
        }

        let mut merged = normalise(text, matches);
        let (start, end) = window(text, merged.first().map(|m| m.start));

        merged.retain(|span| span.end > start && span.start < end);
        let spans = merged
            .into_iter()
            .map(|span| Span::new(span.start.max(start) - start, span.end.min(end) - start))
            .filter(|span| !span.is_empty())
            .collect();

        Self {
            text: text[start..end].to_owned(),
            spans,
            elided_before: start > 0,
            elided_after: end < text.len(),
        }
    }
}

/// Sorts, clamps to char boundaries, and merges overlapping or touching matches.
fn normalise(text: &str, matches: &[std::ops::Range<usize>]) -> Vec<Span> {
    let mut spans: Vec<Span> = matches
        .iter()
        .filter(|range| range.start < range.end && range.end <= text.len())
        .map(|range| {
            Span::new(floor_boundary(text, range.start), ceil_boundary(text, range.end))
        })
        .collect();
    spans.sort_unstable();

    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// The byte range of the window, snapped to word and character boundaries.
fn window(text: &str, first_match: Option<usize>) -> (usize, usize) {
    let Some(first_match) = first_match else {
        // No match to centre on — a facet-only hit, or a filter-only query. Show
        // the opening of the text, which is what the item is "called".
        return (0, take_chars(text, 0, SNIPPET_BUDGET));
    };

    let start = snap_forward_to_word(text, back_chars(text, first_match, LEAD_IN));
    let end = take_chars(text, start, SNIPPET_BUDGET);
    let end = snap_back_to_word(text, end, first_match);
    (start, end)
}

/// The byte offset `count` characters before `from`, or the start of the text.
fn back_chars(text: &str, from: usize, count: usize) -> usize {
    if count == 0 {
        return from;
    }
    text[..from].char_indices().rev().nth(count - 1).map_or(0, |(i, _)| i)
}

/// The byte offset `count` characters after `from`, or the end of the text.
fn take_chars(text: &str, from: usize, count: usize) -> usize {
    text[from..].char_indices().nth(count).map_or(text.len(), |(i, _)| from + i)
}

/// Moves a window start forward to just after the nearest preceding separator, so
/// the snippet does not begin in the middle of a word. Gives up if that would eat
/// more than a word's worth of text.
fn snap_forward_to_word(text: &str, start: usize) -> usize {
    if start == 0 {
        return 0;
    }
    let limit = take_chars(text, start, 24);
    text[start..limit]
        .char_indices()
        .find(|(_, ch)| !ch.is_alphanumeric())
        .map_or(start, |(offset, ch)| start + offset + ch.len_utf8())
}

/// Moves a window end back to the nearest separator, without ever cutting into the
/// first match — a highlight that ran off the end of the snippet would be worse
/// than a word split in half.
fn snap_back_to_word(text: &str, end: usize, protect_from: usize) -> usize {
    if end >= text.len() {
        return text.len();
    }
    let floor = protect_from.max(back_chars(text, end, 24));
    if floor >= end {
        return end;
    }
    text[floor..end]
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_alphanumeric())
        .map_or(end, |(offset, _)| floor + offset)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at.min(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str, needle: &str) -> std::ops::Range<usize> {
        let start = text.find(needle).expect("needle is in the haystack");
        start..start + needle.len()
    }

    /// One match, as a slice. Built through a function rather than written as
    /// `&[a..b]`, which `clippy::single_range_in_vec_init` reads as a mistyped
    /// array length.
    fn one(start: usize, end: usize) -> Vec<std::ops::Range<usize>> {
        vec![std::ops::Range { start, end }]
    }

    #[test]
    fn a_short_text_is_shown_whole_with_the_match_marked() {
        let text = "Cooling fan relay";
        let snippet = Snippet::build(text, &[find(text, "Cooling")]);
        assert_eq!(snippet.text, text);
        assert!(!snippet.elided_before && !snippet.elided_after);
        assert_eq!(snippet.render("[", "]"), "[Cooling] fan relay");
    }

    #[test]
    fn every_match_inside_the_window_is_marked_not_only_the_first() {
        let text = "fan relay and fan fuse";
        let matches = [find(text, "fan"), 14..17];
        let snippet = Snippet::build(text, &matches);
        assert_eq!(snippet.spans.len(), 2);
        assert_eq!(snippet.render("<", ">"), "<fan> relay and <fan> fuse");
    }

    #[test]
    fn overlapping_matches_merge_into_one_span() {
        let text = "cooling";
        let snippet = Snippet::build(text, &[0..4, 0..7, 3..7]);
        assert_eq!(snippet.spans, [Span::new(0, 7)]);
        assert_eq!(snippet.render("[", "]"), "[cooling]");
    }

    #[test]
    fn a_match_late_in_a_long_text_is_windowed_with_both_ellipses() {
        let text = format!("{} cooling {}", "alpha ".repeat(60), "omega ".repeat(60));
        let snippet = Snippet::build(&text, &[find(&text, "cooling")]);
        assert!(snippet.text.chars().count() <= SNIPPET_BUDGET);
        assert!(snippet.elided_before, "there is text before the window");
        assert!(snippet.elided_after, "and after it");
        assert_eq!(snippet.spans.len(), 1);
        assert_eq!(&snippet.text[snippet.spans[0].range()], "cooling");
        assert!(snippet.render("[", "]").starts_with('…'));
    }

    #[test]
    fn the_window_does_not_begin_or_end_in_the_middle_of_a_word() {
        let text = format!("{}cooling fan{}", "wordy ".repeat(40), " trailing".repeat(40));
        let snippet = Snippet::build(&text, &[find(&text, "cooling")]);
        assert!(snippet.elided_before && snippet.elided_after, "the text is far longer");
        assert!(snippet.text.starts_with("wordy"), "not mid-word: {:?}", &snippet.text[..12]);
        assert!(snippet.text.ends_with(|c: char| c.is_alphanumeric()));
    }

    #[test]
    fn a_hit_with_no_match_shows_the_opening_of_the_text() {
        let text = "a".repeat(400);
        let snippet = Snippet::build(&text, &[]);
        assert_eq!(snippet.text.chars().count(), SNIPPET_BUDGET);
        assert!(snippet.spans.is_empty());
        assert!(!snippet.elided_before);
        assert!(snippet.elided_after);
    }

    #[test]
    fn an_item_with_no_text_produces_an_empty_snippet_rather_than_a_panic() {
        let snippet = Snippet::build("", &one(0, 5));
        assert!(snippet.is_empty());
        assert_eq!(snippet.render("[", "]"), "");
    }

    /// Windowing walks characters, never bytes, so a snippet of text that is mostly
    /// multi-byte must still be valid UTF-8 and must not split a character.
    #[test]
    fn multi_byte_text_is_never_split_mid_character() {
        let text = format!("{} kühlung {}", "größe ".repeat(50), "öl ".repeat(50));
        let snippet = Snippet::build(&text, &[find(&text, "kühlung")]);
        assert!(text.contains(&snippet.text));
        assert_eq!(&snippet.text[snippet.spans[0].range()], "kühlung");
    }

    #[test]
    fn a_match_range_that_lands_off_a_character_boundary_is_widened_not_dropped() {
        let text = "kühlung";
        // 2..3 cuts the two-byte `ü` in half.
        let snippet = Snippet::build(text, &one(2, 3));
        assert_eq!(&snippet.text[snippet.spans[0].range()], "ü");
    }

    #[test]
    fn out_of_range_matches_are_ignored() {
        let text = "fan";
        let inverted = std::ops::Range { start: 5, end: 2 };
        let snippet = Snippet::build(text, &[0..99, inverted, 0..3]);
        assert_eq!(snippet.spans, [Span::new(0, 3)]);
    }
}
