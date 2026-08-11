//! HTML character-reference decoding.
//!
//! `tl` is a *tokenising* parser: it hands back the raw source bytes of a text node
//! or an attribute value, entities and all. Nothing else in the pipeline decodes
//! them, so a sticky reading `AT&amp;T` would otherwise import — and be searched,
//! measured and rendered — as the literal seven characters `AT&amp;T`.
//!
//! Two rules are taken from the HTML spec because they are what makes malformed
//! input safe rather than lossy:
//!
//! - An **unrecognised** reference is left verbatim. Browsers render `&foo;` as
//!   `&foo;`, and a sticky containing `R&D; notes` must not lose text to a
//!   speculative decode.
//! - A reference with **no terminating `;` nearby** is left verbatim. Bounding the
//!   search also stops a lone `&` in a long paragraph from costing a scan to the
//!   end of the string for every ampersand.

use std::borrow::Cow;

/// Longest name we will look ahead for a `;`. The longest entity below is
/// `&hellip;` at 6; numeric references reach ~9 (`&#x1F600;`). 16 leaves room
/// without letting a stray `&` scan an entire paragraph.
const MAX_REFERENCE_LEN: usize = 16;

/// The named references that actually occur in Miro's rich text and in HTML pasted
/// into it. The full HTML5 table is ~2200 entries; carrying it would be dead weight
/// for a canvas app, and anything missing degrades to verbatim text rather than to
/// a wrong character.
const NAMED: &[(&str, char)] = &[
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    // U+00A0. Deliberately *not* collapsible whitespace — a contenteditable emits
    // it precisely to defeat whitespace collapsing, which is why the collapser in
    // `html` only folds ASCII whitespace.
    ("nbsp", '\u{00A0}'),
    ("ensp", '\u{2002}'),
    ("emsp", '\u{2003}'),
    ("thinsp", '\u{2009}'),
    ("shy", '\u{00AD}'),
    ("ndash", '–'),
    ("mdash", '—'),
    ("hellip", '…'),
    ("lsquo", '‘'),
    ("rsquo", '’'),
    ("ldquo", '“'),
    ("rdquo", '”'),
    ("laquo", '«'),
    ("raquo", '»'),
    ("bull", '•'),
    ("middot", '·'),
    ("dagger", '†'),
    ("copy", '©'),
    ("reg", '®'),
    ("trade", '™'),
    ("deg", '°'),
    ("plusmn", '±'),
    ("times", '×'),
    ("divide", '÷'),
    ("frac12", '½'),
    ("micro", 'µ'),
    ("sect", '§'),
    ("para", '¶'),
    ("euro", '€'),
    ("pound", '£'),
    ("yen", '¥'),
    ("cent", '¢'),
    ("sup2", '²'),
    ("sup3", '³'),
    ("larr", '←'),
    ("rarr", '→'),
    ("harr", '↔'),
];

/// Decodes character references in `input`.
///
/// Borrows when there is nothing to decode, which is the overwhelmingly common case
/// — the reference board's 429 text strings contain almost no entities — so the
/// converter does not allocate a second copy of every text node.
pub fn decode(input: &str) -> Cow<'_, str> {
    if !input.contains('&') {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        // Entity names are ASCII, so a byte index from `find` is a char boundary.
        match after.find(';').filter(|end| *end <= MAX_REFERENCE_LEN) {
            Some(end) => match resolve(&after[..end]) {
                Some(c) => {
                    out.push(c);
                    rest = &after[end + 1..];
                }
                None => {
                    out.push('&');
                    rest = after;
                }
            },
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Resolves the text *between* `&` and `;`. `None` means "leave it alone".
fn resolve(name: &str) -> Option<char> {
    if let Some(digits) = name.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        // Surrogates and out-of-range values fail `from_u32`; NUL is rejected
        // explicitly because a stray one would truncate C-side consumers and is
        // never what a sticky note meant.
        return char::from_u32(code).filter(|c| *c != '\0');
    }
    NAMED.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_without_references_is_borrowed_not_copied() {
        assert!(matches!(decode("plain text"), Cow::Borrowed(_)));
        assert!(matches!(decode(""), Cow::Borrowed(_)));
    }

    #[test]
    fn decodes_the_five_xml_predefined_names() {
        assert_eq!(decode("&lt;p&gt; &amp; &quot;q&quot; &apos;a&apos;"), "<p> & \"q\" 'a'");
    }

    #[test]
    fn decodes_decimal_and_hexadecimal_references() {
        assert_eq!(decode("&#65;&#x42;&#X43;"), "ABC");
        assert_eq!(decode("&#128512;"), "😀");
        assert_eq!(decode("&#x1F600;"), "😀");
    }

    /// The whole point of `&nbsp;` is that it survives whitespace collapsing, so it
    /// must decode to U+00A0 and not to a plain space.
    #[test]
    fn nbsp_decodes_to_a_non_breaking_space() {
        assert_eq!(decode("a&nbsp;b"), "a\u{00A0}b");
    }

    /// A speculative decode would silently rewrite text the user typed.
    #[test]
    fn unknown_and_unterminated_references_are_left_verbatim() {
        assert_eq!(decode("&foo;"), "&foo;");
        assert_eq!(decode("R&D notes"), "R&D notes");
        assert_eq!(decode("100% & rising"), "100% & rising");
        assert_eq!(decode("&"), "&");
        assert_eq!(decode("&;"), "&;");
    }

    /// A `;` far away is punctuation, not the end of an entity.
    #[test]
    fn the_lookahead_for_a_terminator_is_bounded() {
        let long = format!("&{};", "x".repeat(MAX_REFERENCE_LEN + 1));
        assert_eq!(decode(&long), long);
    }

    #[test]
    fn invalid_code_points_are_left_verbatim() {
        assert_eq!(decode("&#xD800;"), "&#xD800;", "lone surrogate");
        assert_eq!(decode("&#x110000;"), "&#x110000;", "beyond Unicode");
        assert_eq!(decode("&#0;"), "&#0;");
        assert_eq!(decode("&#zz;"), "&#zz;");
    }

    #[test]
    fn consecutive_and_adjacent_references_decode() {
        assert_eq!(decode("&amp;&amp;"), "&&");
        assert_eq!(decode("&amp;amp;"), "&amp;", "one level of double-encoding");
    }

    /// Real link text from the reference board's `preview` widgets.
    #[test]
    fn decodes_a_query_string_the_way_a_link_href_arrives() {
        assert_eq!(
            decode("https://example.com/x?a=1&amp;b=2"),
            "https://example.com/x?a=1&b=2"
        );
    }
}
