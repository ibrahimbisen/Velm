//! The sliver of inline CSS that carries character formatting.
//!
//! Miro's own rich text keeps most formatting in tags (`<strong>`, `<em>`) and uses
//! `style="color: …"` for colour only. But the same HTML flavour is what a *paste*
//! from a browser, Google Docs or Notion arrives as, and those emit
//! `style="font-weight: 700; text-decoration: underline"` with no tag at all. The
//! two paths share one parser so a pasted bold looks like a typed bold.
//!
//! This is not a CSS engine and must not become one: unknown properties, units and
//! functions are ignored, which loses formatting rather than corrupting text.

use crate::span::{Rgb, SpanStyle};

/// Applies the declarations in one `style` attribute on top of `style`.
///
/// Later declarations win over earlier ones, matching CSS. Inheritance is handled
/// by the caller passing the parent's resolved style in, so a nested
/// `text-decoration: none` can genuinely clear an ancestor's underline.
pub fn apply_declarations(style: &mut SpanStyle, css: &str) {
    for declaration in css.split(';') {
        let Some((property, value)) = declaration.split_once(':') else { continue };
        let property = property.trim();
        let value = value.trim();
        if property.eq_ignore_ascii_case("color") {
            // A colour we cannot parse must leave the inherited one alone rather
            // than reset it to "no override", which would repaint the run.
            if let Some(rgb) = parse_color(value) {
                style.color = Some(rgb);
            }
        } else if property.eq_ignore_ascii_case("font-weight") {
            style.bold = is_bold_weight(value);
        } else if property.eq_ignore_ascii_case("font-style") {
            style.italic =
                value.eq_ignore_ascii_case("italic") || value.eq_ignore_ascii_case("oblique");
        } else if property.eq_ignore_ascii_case("text-decoration")
            || property.eq_ignore_ascii_case("text-decoration-line")
        {
            // The shorthand also carries colour, style and thickness keywords; only
            // the line keywords matter to a span.
            let lowered = value.to_ascii_lowercase();
            style.underline = lowered.contains("underline");
            style.strikethrough = lowered.contains("line-through");
        }
    }
}

/// `font-weight` is bold from 600 up, which is where the CSS `bold` keyword sits
/// and where every font family that has a semibold starts looking bold.
fn is_bold_weight(value: &str) -> bool {
    if value.eq_ignore_ascii_case("bold") || value.eq_ignore_ascii_case("bolder") {
        return true;
    }
    value.parse::<u32>().is_ok_and(|w| w >= 600)
}

/// The CSS colour forms a contenteditable actually emits.
///
/// `hsl()`, `color()`, percentage `rgb()` components and the extended keyword list
/// are unhandled: they yield `None`, which leaves the run in the inherited colour.
/// That is a visible-but-correct fallback, whereas guessing is not.
pub fn parse_color(value: &str) -> Option<Rgb> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Some(args) = strip_call(value, "rgb").or_else(|| strip_call(value, "rgba")) {
        return parse_rgb_arguments(args);
    }
    NAMED_COLORS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(value))
        .map(|(_, rgb)| *rgb)
}

/// `#rgb`, `#rrggbb` and `#rrggbbaa`. Alpha is parsed to validate the length and
/// then dropped — see [`Rgb`].
fn parse_hex(hex: &str) -> Option<Rgb> {
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let bytes = hex.as_bytes();
    match bytes.len() {
        3 | 4 => {
            let [r, g, b] = [nibble(bytes[0])?, nibble(bytes[1])?, nibble(bytes[2])?];
            Some(Rgb::new(r * 17, g * 17, b * 17))
        }
        6 | 8 => {
            let pair = |i: usize| Some(nibble(bytes[i])? * 16 + nibble(bytes[i + 1])?);
            Some(Rgb::new(pair(0)?, pair(2)?, pair(4)?))
        }
        _ => None,
    }
}

/// Returns the argument text of `name(…)`, or `None` if `value` is not that call.
fn strip_call<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    let rest = value.strip_suffix(')')?;
    let (head, args) = rest.split_once('(')?;
    head.trim().eq_ignore_ascii_case(name).then_some(args)
}

/// Accepts both the legacy comma form `rgb(26, 26, 26)` and the modern space form
/// `rgb(26 26 26 / 50%)`; Chromium emits the first, Safari has started emitting the
/// second. Percentage components are not accepted — they parse as `None` and the
/// run keeps its inherited colour.
fn parse_rgb_arguments(args: &str) -> Option<Rgb> {
    // Modern syntax puts alpha after a slash, legacy after a fourth comma.
    let components: Vec<f32> = args
        .split('/')
        .next()?
        .split([',', ' ', '\t'])
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f32>().ok())
        .collect::<Option<_>>()?;
    if !(3..=4).contains(&components.len()) {
        return None;
    }
    let channel = |v: f32| v.clamp(0.0, 255.0).round() as u8;
    Some(Rgb::new(channel(components[0]), channel(components[1]), channel(components[2])))
}

/// The keywords a person actually types or a legacy editor emits. The full CSS list
/// is 148 entries of decorative names that no board has ever contained.
const NAMED_COLORS: &[(&str, Rgb)] = &[
    ("black", Rgb::new(0x00, 0x00, 0x00)),
    ("silver", Rgb::new(0xC0, 0xC0, 0xC0)),
    ("gray", Rgb::new(0x80, 0x80, 0x80)),
    ("grey", Rgb::new(0x80, 0x80, 0x80)),
    ("white", Rgb::new(0xFF, 0xFF, 0xFF)),
    ("maroon", Rgb::new(0x80, 0x00, 0x00)),
    ("red", Rgb::new(0xFF, 0x00, 0x00)),
    ("purple", Rgb::new(0x80, 0x00, 0x80)),
    ("fuchsia", Rgb::new(0xFF, 0x00, 0xFF)),
    ("green", Rgb::new(0x00, 0x80, 0x00)),
    ("lime", Rgb::new(0x00, 0xFF, 0x00)),
    ("olive", Rgb::new(0x80, 0x80, 0x00)),
    ("yellow", Rgb::new(0xFF, 0xFF, 0x00)),
    ("navy", Rgb::new(0x00, 0x00, 0x80)),
    ("blue", Rgb::new(0x00, 0x00, 0xFF)),
    ("teal", Rgb::new(0x00, 0x80, 0x80)),
    ("aqua", Rgb::new(0x00, 0xFF, 0xFF)),
    ("orange", Rgb::new(0xFF, 0xA5, 0x00)),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(css: &str) -> SpanStyle {
        let mut style = SpanStyle::default();
        apply_declarations(&mut style, css);
        style
    }

    /// The exact form Miro emits for `tc: 1710618`, cross-checked in
    /// `docs/02-miro-formats.md` §2.2 as `#1a1a1a`.
    #[test]
    fn parses_the_colour_forms_miro_emits() {
        assert_eq!(parse_color("#1a1a1a"), Some(Rgb::new(0x1A, 0x1A, 0x1A)));
        assert_eq!(parse_color("rgb(26, 26, 26)"), Some(Rgb::new(0x1A, 0x1A, 0x1A)));
        assert_eq!(applied("color: #1a1a1a").color, Some(Rgb::new(0x1A, 0x1A, 0x1A)));
    }

    #[test]
    fn parses_every_hex_length() {
        assert_eq!(parse_color("#f00"), Some(Rgb::new(0xFF, 0x00, 0x00)));
        assert_eq!(parse_color("#F00A"), Some(Rgb::new(0xFF, 0x00, 0x00)), "#rgba, alpha dropped");
        assert_eq!(parse_color("#ff0000"), Some(Rgb::new(0xFF, 0x00, 0x00)));
        assert_eq!(parse_color("#ff000080"), Some(Rgb::new(0xFF, 0x00, 0x00)));
        assert_eq!(parse_color("#ff"), None, "no hex form has 2 nibbles");
        assert_eq!(parse_color("#ff000"), None, "nor 5");
        assert_eq!(parse_color("#gg0000"), None);
    }

    #[test]
    fn parses_both_rgb_argument_syntaxes() {
        assert_eq!(parse_color("rgb(1 2 3)"), Some(Rgb::new(1, 2, 3)));
        assert_eq!(parse_color("rgba(1, 2, 3, 0.5)"), Some(Rgb::new(1, 2, 3)));
        assert_eq!(parse_color("rgb(1 2 3 / 50%)"), Some(Rgb::new(1, 2, 3)));
        assert_eq!(parse_color("RGB( 255 , 128 , 0 )"), Some(Rgb::new(255, 128, 0)));
        assert_eq!(parse_color("rgb(1 2)"), None);
        assert_eq!(parse_color("rgb(1 2 3 4 5)"), None);
        assert_eq!(parse_color("rgb(100%, 0%, 0%)"), None, "percentages unsupported");
        assert_eq!(parse_color("hsl(0 100% 50%)"), None, "unsupported, not guessed");
    }

    #[test]
    fn parses_named_colours_case_insensitively() {
        assert_eq!(parse_color("Red"), Some(Rgb::new(0xFF, 0, 0)));
        assert_eq!(parse_color("rebeccapurple"), None);
    }

    #[test]
    fn reads_weight_style_and_decoration() {
        assert!(applied("font-weight: bold").bold);
        assert!(applied("font-weight:700").bold);
        assert!(!applied("font-weight: 500").bold);
        assert!(applied("font-style: italic").italic);
        assert!(applied("text-decoration: underline").underline);
        assert!(applied("text-decoration: line-through").strikethrough);
        let both = applied("text-decoration: underline line-through");
        assert!(both.underline && both.strikethrough);
    }

    /// A nested `none` must be able to clear what an ancestor turned on, which is
    /// why declarations are applied on top of the inherited style rather than to a
    /// fresh one.
    #[test]
    fn decoration_none_clears_an_inherited_underline() {
        let mut style = SpanStyle { underline: true, ..SpanStyle::default() };
        apply_declarations(&mut style, "text-decoration: none");
        assert!(!style.underline);
    }

    /// An unparseable colour must leave the inherited one in place; resetting to
    /// `None` would repaint the run in the widget's default colour.
    #[test]
    fn an_unparseable_colour_leaves_the_inherited_one_alone() {
        let mut style = SpanStyle { color: Some(Rgb::new(1, 2, 3)), ..SpanStyle::default() };
        apply_declarations(&mut style, "color: var(--brand)");
        assert_eq!(style.color, Some(Rgb::new(1, 2, 3)));
    }

    #[test]
    fn junk_declarations_are_skipped_not_fatal() {
        let style = applied(";;  ; not-a-declaration ; color ; :  ; color: #00ff00 ;");
        assert_eq!(style.color, Some(Rgb::new(0, 0xFF, 0)));
    }

    #[test]
    fn later_declarations_win() {
        assert_eq!(applied("color: red; color: blue").color, Some(Rgb::new(0, 0, 0xFF)));
    }
}
