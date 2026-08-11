//! Miro rich-text HTML → [`StyledText`].
//!
//! Miro stores the body of a `sticker` and a `text` widget as an HTML fragment. Two
//! forms captured from the reference board, byte for byte:
//!
//! ```text
//! <p>fan</p><p><br /></p>
//! <a href="https://wiki.example.net/x">https://wiki.example.net/x</a>
//! ```
//!
//! so this converter is on the critical path for every one of the board's 44
//! stickies and 46 text widgets. It never fails: a sticky that fails to parse must
//! still import as its own text, because a *paste* is user-triggered and a blank
//! widget is a worse outcome than an unstyled one.
//!
//! # The rules that are not obvious
//!
//! **A trailing `<br>` inside a block is a placeholder, not a line break.** This is
//! the single most important rule here, and it is what `<p>fan</p><p><br /></p>`
//! turns on. A `contenteditable` cannot place a caret in a genuinely empty
//! paragraph, so every browser inserts a `<br>` to hold it open — and every browser
//! then declines to render that `<br>` as a line. Emitting one would give every
//! Miro sticky an extra blank line. The converter therefore *defers* each `<br>`
//! and materialises it only when more content follows.
//!
//! **Blocks separate, they do not terminate.** A newline is emitted *before* a
//! block's content when the output is not already at the start of a line, never
//! after it. `<p>a</p><p>b</p>` is two lines, not three.
//!
//! **Whitespace collapses the way HTML says it does.** Runs of ASCII whitespace fold
//! to one space, a space at the start of a line is dropped, and trailing spaces are
//! trimmed at a line break. `&nbsp;` is not ASCII whitespace and survives, which is
//! exactly the distinction a contenteditable relies on to store a double space.
//!
//! # Robustness
//!
//! `tl` is lenient by construction — it has no HTML5 tree-construction algorithm, so
//! unclosed tags simply nest and stray closing tags are ignored. The walk here is an
//! explicit work stack rather than recursion, so pathologically nested input costs
//! heap instead of blowing the call stack, which would abort the process rather than
//! merely panic.

use crate::css;
use crate::entity;
use crate::span::{SpanStyle, StyledText, TextSpan};
use std::borrow::Cow;
use tl::{HTMLTag, Node, NodeHandle, ParserOptions};

/// Elements that begin a new visual line.
///
/// Miro only ever emits `<p>`, but a paste carries whatever the source document
/// used, and a list or heading collapsing into one run of text is an obvious import
/// bug where a missing `<figcaption>` break is not.
const BLOCK_ELEMENTS: &[&[u8]] = &[
    b"p", b"div", b"h1", b"h2", b"h3", b"h4", b"h5", b"h6", b"li", b"ul", b"ol", b"dl", b"dt",
    b"dd", b"blockquote", b"pre", b"section", b"article", b"header", b"footer", b"figure",
    b"figcaption", b"table", b"tr", b"td", b"th", b"hr", b"address", b"main", b"nav", b"aside",
    b"form",
];

/// Elements whose subtree is not user-visible text. Without this a pasted fragment
/// that carries its own `<style>` block would import the CSS as sticky content.
const SKIPPED_ELEMENTS: &[&[u8]] =
    &[b"script", b"style", b"head", b"title", b"noscript", b"template", b"svg", b"iframe"];

/// Converts one Miro rich-text value into styled spans.
///
/// Total: malformed markup degrades to its text content, and input containing no
/// markup at all round-trips as plain text.
pub fn from_miro_html(html: &str) -> StyledText {
    let Ok(dom) = tl::parse(html, ParserOptions::default()) else {
        // `tl` only rejects input whose length overflows a `u32`, which a widget
        // body never reaches. Degrading rather than propagating keeps the whole
        // module infallible for callers.
        return strip_markup(html);
    };
    let parser = dom.parser();

    let mut builder = Builder::default();
    let mut styles = vec![SpanStyle::default()];
    let mut work: Vec<Step> = dom.children().iter().rev().map(|h| Step::Visit(*h)).collect();

    while let Some(step) = work.pop() {
        match step {
            Step::PopStyle => {
                styles.pop();
            }
            Step::EndBlock => builder.end_block(),
            Step::Visit(handle) => {
                let Some(node) = handle.get(parser) else { continue };
                match node {
                    Node::Comment(_) => {}
                    Node::Raw(bytes) => {
                        let style = styles.last().cloned().unwrap_or_default();
                        builder.push_text(&entity::decode(&bytes.as_utf8_str()), &style);
                    }
                    Node::Tag(tag) => {
                        let name = tag.name().as_bytes();
                        if matches_any(name, SKIPPED_ELEMENTS) {
                            continue;
                        }
                        if name.eq_ignore_ascii_case(b"br") {
                            builder.push_break();
                            continue;
                        }

                        let is_block = matches_any(name, BLOCK_ELEMENTS);
                        if is_block {
                            builder.start_block();
                        }

                        // Exit markers are pushed first so they pop *after* every
                        // child has been visited.
                        if is_block {
                            work.push(Step::EndBlock);
                        }
                        work.push(Step::PopStyle);
                        styles.push(inherit(styles.last().cloned().unwrap_or_default(), tag));
                        // `as_slice`, not `iter`: `tl`'s inline vector iterator is
                        // forward-only, and children must be pushed in reverse so
                        // the stack pops them in document order.
                        let children = tag.children();
                        work.extend(
                            children.top().as_slice().iter().rev().map(|h| Step::Visit(*h)),
                        );
                    }
                }
            }
        }
    }

    // The document root is itself a block, so whitespace stranded at its end is not
    // rendered either.
    builder.trim_trailing_spaces();
    StyledText::from_spans(builder.spans)
}

/// One entry in the explicit traversal stack. Recursion would be shorter, but a
/// widget body is attacker-controlled in the sense that it comes off the clipboard,
/// and a stack overflow is an abort rather than a recoverable error.
enum Step {
    Visit(NodeHandle),
    PopStyle,
    EndBlock,
}

/// Resolves an element's own formatting on top of its parent's.
fn inherit(mut style: SpanStyle, tag: &HTMLTag<'_>) -> SpanStyle {
    let name = tag.name().as_bytes();
    if matches_any(name, &[b"strong", b"b"]) {
        style.bold = true;
    }
    if matches_any(name, &[b"em", b"i", b"cite", b"var"]) {
        style.italic = true;
    }
    if matches_any(name, &[b"u", b"ins"]) {
        style.underline = true;
    }
    if matches_any(name, &[b"s", b"strike", b"del"]) {
        style.strikethrough = true;
    }
    if name.eq_ignore_ascii_case(b"a") {
        // An anchor with no target is a bookmark, not a link; styling it as one
        // would render underlined blue text that goes nowhere.
        if let Some(href) = attribute(tag, "href") {
            let href = entity::decode(&href);
            let href = href.trim();
            if !href.is_empty() {
                style.link = Some(href.to_owned());
            }
        }
    }
    if let Some(declarations) = attribute(tag, "style") {
        css::apply_declarations(&mut style, &entity::decode(&declarations));
    }
    style
}

/// Reads an attribute by name, case-insensitively.
///
/// `tl`'s own lookup compares raw bytes, so it would miss the `STYLE=` that pasted
/// legacy markup still contains.
fn attribute(tag: &HTMLTag<'_>, key: &str) -> Option<String> {
    tag.attributes()
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .and_then(|(_, value)| value)
        .map(Cow::into_owned)
}

fn matches_any(name: &[u8], set: &[&[u8]]) -> bool {
    set.iter().any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// A line break that has been decided on but not yet written.
///
/// Deferring is what distinguishes a real `<br>` from the placeholder one a
/// contenteditable leaves in an empty paragraph: a pending break is discarded at end
/// of input instead of adding a phantom trailing line.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Pending {
    #[default]
    None,
    /// A block boundary — writes a newline only if the line is not already broken.
    Block,
    /// An explicit `<br>` — always writes a newline, once something follows it.
    Break,
}

#[derive(Default)]
struct Builder {
    spans: Vec<TextSpan>,
    pending: Pending,
}

impl Builder {
    fn last_char(&self) -> Option<char> {
        self.spans.last().and_then(|span| span.text.chars().next_back())
    }

    /// True when the next character written would begin a line.
    fn at_line_start(&self) -> bool {
        matches!(self.last_char(), None | Some('\n'))
    }

    /// Opens a block. A block always begins a line, and it begins a *new* one when
    /// the previous block already closed — which is how `<p>a</p><p></p><p>b</p>`
    /// keeps the blank line the author pressed Enter for.
    fn start_block(&mut self) {
        if self.pending != Pending::None || !self.at_line_start() {
            self.write_newline();
        }
        self.pending = Pending::None;
    }

    fn end_block(&mut self) {
        // Whitespace before a block's closing tag is markup indentation and is not
        // rendered — `<p>\n  a\n</p>` is one word, not one word and a space.
        self.trim_trailing_spaces();
        // A `<br>` already inside this block outranks the block's own boundary,
        // because a break is unconditional where a boundary is not: downgrading it
        // would flatten `<p>a<br><br></p>tail` from the three lines a browser
        // renders to two.
        if self.pending == Pending::None {
            self.pending = Pending::Block;
        }
    }

    fn push_break(&mut self) {
        self.flush_pending();
        self.pending = Pending::Break;
    }

    fn push_text(&mut self, text: &str, style: &SpanStyle) {
        let drop_leading_space =
            self.pending != Pending::None || matches!(self.last_char(), None | Some('\n' | ' '));
        let collapsed = collapse_whitespace(text, drop_leading_space);
        // Whitespace between two block tags is markup indentation, not content, and
        // must not flush a pending break — otherwise `<p>a</p>\n` gains a line.
        if collapsed.is_empty() {
            return;
        }
        self.flush_pending();
        self.push_span(&collapsed, style);
    }

    fn flush_pending(&mut self) {
        match self.pending {
            Pending::None => {}
            Pending::Block => {
                if !self.at_line_start() {
                    self.write_newline();
                }
            }
            Pending::Break => self.write_newline(),
        }
        self.pending = Pending::None;
    }

    /// Writes a hard break, dropping the whitespace it would have stranded at the
    /// end of the line.
    ///
    /// The newline itself is unstyled on purpose: it belongs to no run's formatting,
    /// and keeping it plain means an underline or a link never visibly extends past
    /// the end of a line.
    fn write_newline(&mut self) {
        self.trim_trailing_spaces();
        self.push_span("\n", &SpanStyle::default());
        self.pending = Pending::None;
    }

    fn trim_trailing_spaces(&mut self) {
        while let Some(span) = self.spans.last_mut() {
            let trimmed = span.text.trim_end_matches(' ');
            if trimmed.len() == span.text.len() {
                return;
            }
            span.text.truncate(trimmed.len());
            if !span.text.is_empty() {
                return;
            }
            self.spans.pop();
        }
    }

    fn push_span(&mut self, text: &str, style: &SpanStyle) {
        match self.spans.last_mut() {
            Some(prev) if &prev.style == style => prev.text.push_str(text),
            _ => self.spans.push(TextSpan::new(text, style.clone())),
        }
    }
}

/// Folds runs of ASCII whitespace into single spaces, per `white-space: normal`.
///
/// Only ASCII whitespace collapses. U+00A0 and friends are what a contenteditable
/// writes precisely so they *don't*, and folding them would silently rewrite text.
fn collapse_whitespace(text: &str, drop_leading_space: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_whitespace = drop_leading_space;
    for c in text.chars() {
        if c.is_ascii_whitespace() {
            if !in_whitespace {
                out.push(' ');
                in_whitespace = true;
            }
        } else {
            out.push(c);
            in_whitespace = false;
        }
    }
    out
}

/// Last-resort conversion for input the parser rejected outright.
///
/// Deliberately naive — it drops everything between `<` and `>` and decodes what is
/// left. It exists so that [`from_miro_html`] has no failure path at all, not to
/// parse anything.
fn strip_markup(html: &str) -> StyledText {
    let mut text = String::with_capacity(html.len());
    let mut depth = 0usize;
    for c in html.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => text.push(c),
            _ => {}
        }
    }
    StyledText::plain(entity::decode(&text).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Rgb;

    fn plain(html: &str) -> String {
        from_miro_html(html).to_plain()
    }

    /// Captured verbatim from a `sticker` widget on the reference board.
    ///
    /// The empty second paragraph is a real, deliberate blank line the author left,
    /// and its `<br />` is the caret placeholder that must **not** add a third.
    #[test]
    fn converts_the_verified_sticky_html() {
        let text = from_miro_html("<p>fan</p><p><br /></p>");
        assert_eq!(text.to_plain(), "fan\n");
        assert_eq!(text.spans().len(), 1);
        assert!(text.spans()[0].style.is_plain());
    }

    /// Captured verbatim from a `text` widget on the reference board.
    #[test]
    fn converts_the_verified_link_html() {
        let url = "https://wiki.example.net/x";
        let text = from_miro_html(&format!("<a href=\"{url}\">{url}</a>"));
        assert_eq!(text.spans().len(), 1);
        assert_eq!(text.spans()[0].text, url);
        assert_eq!(text.spans()[0].style.link.as_deref(), Some(url));
    }

    #[test]
    fn paragraphs_separate_without_a_trailing_break() {
        assert_eq!(plain("<p>a</p><p>b</p>"), "a\nb");
        assert_eq!(plain("<p>a</p><p>b</p><p>c</p>"), "a\nb\nc");
        assert_eq!(plain("<div>a</div><div>b</div>"), "a\nb");
    }

    #[test]
    fn empty_paragraphs_become_empty_lines() {
        assert_eq!(plain("<p>a</p><p></p><p>b</p>"), "a\n\nb");
        assert_eq!(plain("<p><br></p><p>b</p>"), "\nb");
    }

    #[test]
    fn explicit_breaks_are_kept_only_where_a_browser_renders_them() {
        assert_eq!(plain("a<br>b"), "a\nb");
        assert_eq!(plain("a<br><br>b"), "a\n\nb");
        assert_eq!(plain("a<br>"), "a", "trailing placeholder");
        assert_eq!(plain("<p>a<br></p><p>b</p>"), "a\nb");
        assert_eq!(plain("<br>a"), "\na", "leading break is a real empty line");
        assert_eq!(plain("<p><br>x</p>"), "\nx");
        // A break outranks the block boundary that follows it, because a break is
        // unconditional where a boundary only breaks a line that is still open.
        assert_eq!(plain("<p>a<br><br></p>tail"), "a\n\ntail");
    }

    #[test]
    fn every_character_formatting_tag_is_recognised() {
        let text = from_miro_html(
            "<b>b</b><strong>s</strong><i>i</i><em>e</em><u>u</u><ins>n</ins>\
             <s>x</s><strike>y</strike><del>z</del>",
        );
        let styles: Vec<_> = text.spans().iter().map(|s| (s.text.as_str(), &s.style)).collect();
        assert_eq!(styles.len(), 4, "adjacent equal styles merge: {styles:?}");
        assert_eq!(styles[0].0, "bs");
        assert!(styles[0].1.bold);
        assert_eq!(styles[1].0, "ie");
        assert!(styles[1].1.italic);
        assert_eq!(styles[2].0, "un");
        assert!(styles[2].1.underline);
        assert_eq!(styles[3].0, "xyz");
        assert!(styles[3].1.strikethrough);
    }

    #[test]
    fn nested_tags_compose_into_one_style() {
        let text = from_miro_html("<strong><em><u>all three</u></em></strong>");
        assert_eq!(text.spans().len(), 1);
        let style = &text.spans()[0].style;
        assert!(style.bold && style.italic && style.underline);
        assert!(!style.strikethrough);
    }

    #[test]
    fn formatting_stops_at_the_closing_tag() {
        let text = from_miro_html("plain <strong>bold</strong> plain");
        assert_eq!(text.to_plain(), "plain bold plain");
        assert_eq!(text.spans().len(), 3);
        assert!(!text.spans()[0].style.bold);
        assert!(text.spans()[1].style.bold);
        assert!(!text.spans()[2].style.bold);
    }

    #[test]
    fn links_nest_with_character_formatting() {
        let text = from_miro_html("<a href=\"https://vellum.app\"><strong>docs</strong></a>");
        assert_eq!(text.spans().len(), 1);
        assert_eq!(text.spans()[0].style.link.as_deref(), Some("https://vellum.app"));
        assert!(text.spans()[0].style.bold);
    }

    #[test]
    fn an_inner_link_overrides_an_outer_one() {
        let text = from_miro_html("<a href=\"/outer\">a<a href=\"/inner\">b</a></a>");
        let links: Vec<_> =
            text.spans().iter().map(|s| s.style.link.as_deref().unwrap_or("")).collect();
        assert_eq!(links, ["/outer", "/inner"]);
    }

    #[test]
    fn an_anchor_without_a_usable_href_is_not_a_link() {
        assert!(from_miro_html("<a>text</a>").spans()[0].style.link.is_none());
        assert!(from_miro_html("<a href=\"\">text</a>").spans()[0].style.link.is_none());
        assert!(from_miro_html("<a href=\"   \">text</a>").spans()[0].style.link.is_none());
    }

    /// An href arrives entity-encoded whenever it carries more than one query
    /// parameter, which is most real links.
    #[test]
    fn href_entities_are_decoded() {
        let text = from_miro_html("<a href=\"https://x.test/?a=1&amp;b=2\">x</a>");
        assert_eq!(text.spans()[0].style.link.as_deref(), Some("https://x.test/?a=1&b=2"));
    }

    #[test]
    fn span_colour_becomes_a_span_style() {
        let text = from_miro_html("<span style=\"color: #1a1a1a\">dark</span> normal");
        assert_eq!(text.spans()[0].style.color, Some(Rgb::new(0x1A, 0x1A, 0x1A)));
        assert_eq!(text.spans()[1].style.color, None);
    }

    #[test]
    fn inline_css_is_read_from_any_element_and_any_case() {
        let text = from_miro_html("<div STYLE=\"font-weight:700;color:red\">x</div>");
        assert!(text.spans()[0].style.bold);
        assert_eq!(text.spans()[0].style.color, Some(Rgb::new(0xFF, 0, 0)));
    }

    #[test]
    fn entities_in_text_are_decoded() {
        assert_eq!(plain("<p>AT&amp;T &lt;3</p>"), "AT&T <3");
        assert_eq!(plain("<p>a&nbsp;&nbsp;b</p>"), "a\u{A0}\u{A0}b");
    }

    #[test]
    fn whitespace_collapses_the_way_html_specifies() {
        assert_eq!(plain("<p>a   b</p>"), "a b");
        assert_eq!(plain("<p>\n  a\n  b\n</p>"), "a b");
        assert_eq!(plain("<p>a</p>\n  \n<p>b</p>"), "a\nb", "indentation is not content");
        assert_eq!(plain("<p>  leading</p>"), "leading");
        assert_eq!(plain("<p>trailing  </p>"), "trailing");
        assert_eq!(plain("a <b> b</b>"), "a b", "a space either side of a tag is one space");
    }

    #[test]
    fn input_with_no_markup_is_plain_text() {
        assert_eq!(plain("fan"), "fan");
        assert_eq!(from_miro_html("fan"), StyledText::plain("fan"));
    }

    #[test]
    fn empty_input_is_empty_text() {
        assert!(from_miro_html("").is_empty());
        assert!(from_miro_html("<p></p>").is_empty());
        assert!(from_miro_html("<p><br /></p>").is_empty());
        assert!(from_miro_html("   ").is_empty());
    }

    /// A paste is user-triggered and must never take the app down, and must never
    /// silently discard text it could otherwise have structured.
    #[test]
    fn malformed_markup_degrades_to_its_text() {
        assert_eq!(plain("<p>unclosed"), "unclosed");
        assert_eq!(plain("<strong>bold forever"), "bold forever");
        assert_eq!(plain("stray </b> close"), "stray close");
        assert_eq!(plain("<p>a<p>b"), "a\nb", "an unclosed block still separates");
        assert_eq!(plain("<<>>"), ">");
        assert_eq!(plain("<p><p><p>"), "");
    }

    /// An **unescaped** `<` in text is the one lossy case, and it is `tl`'s: the
    /// tokeniser reads everything after it as a tag name and never finds a `>`, so
    /// the rest of the fragment is discarded. Recorded rather than worked around —
    /// every producer of this HTML escapes `<`, and pre-scanning for "ambiguous"
    /// `<` would mean writing the tokeniser we chose `tl` to avoid.
    #[test]
    fn an_unescaped_less_than_swallows_the_rest_of_the_fragment() {
        assert_eq!(plain("a < b"), "a");
        assert_eq!(plain("<p title=\"unterminated>text</p>"), "");
        // Escaped, which is how it actually arrives, it round-trips.
        assert_eq!(plain("a &lt; b"), "a < b");
        assert_eq!(plain("&lt;p&gt;not a tag&lt;/p&gt;"), "<p>not a tag</p>");
    }

    #[test]
    fn unclosed_formatting_still_styles_what_follows() {
        let text = from_miro_html("<strong>bold forever");
        assert!(text.spans()[0].style.bold);
    }

    #[test]
    fn comments_and_non_content_subtrees_are_dropped() {
        assert_eq!(plain("a<!-- hidden -->b"), "ab");
        assert_eq!(plain("<style>p{color:red}</style>visible"), "visible");
        assert_eq!(plain("<script>alert(1)</script>visible"), "visible");
    }

    #[test]
    fn list_items_and_table_cells_each_take_a_line() {
        assert_eq!(plain("<ul><li>one</li><li>two</li></ul>"), "one\ntwo");
        assert_eq!(plain("<table><tr><td>a</td><td>b</td></tr></table>"), "a\nb");
    }

    /// Deep nesting must cost heap, not call stack. 50k frames would overflow the
    /// default 8MB stack on a recursive walk, and an overflow aborts the process.
    #[test]
    fn pathological_nesting_does_not_overflow_the_stack() {
        let depth = 50_000;
        let html = format!("{}deep{}", "<span>".repeat(depth), "</span>".repeat(depth));
        assert_eq!(from_miro_html(&html).to_plain(), "deep");
    }

    #[test]
    fn multibyte_text_survives_intact() {
        let text = from_miro_html("<p>héllo wörld — 日本語</p>");
        assert_eq!(text.to_plain(), "héllo wörld — 日本語");
        assert_eq!(text.char_len(), 17);
        assert_eq!(text.to_plain().len(), 27, "and it is not a byte count");
    }

    /// One fragment exercising paragraphs, nesting, a link, a colour span and
    /// entities together — the shape a paste from a browser actually arrives in.
    #[test]
    fn a_realistic_mixed_fragment_converts_end_to_end() {
        let text = from_miro_html(
            "<p>Check the <strong>ECU <em>pinout</em></strong> &mdash; \
             <a href=\"https://x.test/?a=1&amp;b=2\">wiki</a></p>\
             <p><span style=\"color:#ff9e9e\">TODO</span>: <s>rewire</s></p>",
        );
        assert_eq!(text.to_plain(), "Check the ECU pinout — wiki\nTODO: rewire");

        let styles: Vec<(&str, &SpanStyle)> =
            text.spans().iter().map(|s| (s.text.as_str(), &s.style)).collect();
        assert_eq!(styles[0].0, "Check the ");
        assert!(styles[0].1.is_plain());
        assert_eq!(styles[1].0, "ECU ");
        assert!(styles[1].1.bold && !styles[1].1.italic);
        assert_eq!(styles[2].0, "pinout");
        assert!(styles[2].1.bold && styles[2].1.italic);
        assert_eq!(styles[3].0, " — ");
        assert_eq!(styles[4].0, "wiki");
        assert_eq!(styles[4].1.link.as_deref(), Some("https://x.test/?a=1&b=2"));
        assert_eq!(styles[5].0, "\n");
        assert!(styles[5].1.is_plain(), "a line break carries no formatting");
        assert_eq!(styles[6].0, "TODO");
        assert_eq!(styles[6].1.color, Some(Rgb::new(0xFF, 0x9E, 0x9E)));
        assert_eq!(styles[7].0, ": ");
        assert_eq!(styles[8].0, "rewire");
        assert!(styles[8].1.strikethrough);
        assert_eq!(styles.len(), 9);
    }

    /// The normalisation invariant `StyledText` promises must hold for converter
    /// output, or equality between two identical-looking imports is meaningless.
    #[test]
    fn output_is_normalised() {
        let split = from_miro_html("<b>a</b><b>b</b>");
        let whole = from_miro_html("<b>ab</b>");
        assert_eq!(split, whole);
        assert_eq!(split.spans().len(), 1);
    }

    #[test]
    fn the_markup_stripping_fallback_keeps_text_and_entities() {
        assert_eq!(strip_markup("<p>a &amp; b</p>").to_plain(), "a & b");
        assert_eq!(strip_markup("no markup").to_plain(), "no markup");
        assert!(strip_markup("").is_empty());
    }
}
