//! Colour, and the names people actually type.
//!
//! Searching a visual board is not only searching its text. On the reference board
//! in `docs/02-miro-formats.md` there are 44 stickies, 43 of them `#fff79e` and one
//! `#ff9e9e`, and "the red one" is a far more natural way to find that outlier than
//! remembering what it says. So colour is indexed as a facet, and the query
//! `colour:red` has to reach `#ff9e9e` without anybody ever typing a hex triple.
//!
//! # Every colour is indexed under more than one name
//!
//! `#ff9e9e` is, honestly, salmon. Its hue is 0° — textbook red — but its lightness
//! is 0.81, which is the region most people call pink. Picking one of those and
//! discarding the other would make the search wrong for half its users, so a colour
//! reports a **primary name and an optional everyday alias**, and both are indexed.
//! `colour:red` and `colour:pink` both find that sticky; `colour:#ff9e9e` finds it
//! exactly.
//!
//! The alias is not a synonym table bolted on afterwards — it is the second name
//! for the *band*, chosen where the perceptual boundary genuinely falls inside a
//! band rather than between two:
//!
//! | Band | Primary | Alias, and when |
//! |---|---|---|
//! | hue 345–15° | `red` | `pink` when light (L > 0.72) — pale reds read as pink |
//! | hue 15–45° | `orange` | `brown` when dark (L < 0.42) — brown *is* dark orange |
//! | hue 165–195° | `cyan` | `teal` always — the two words name the same band |
//! | hue 255–290° | `purple` | `violet` always |
//! | hue 290–345° | `pink` | `magenta` always |
//! | achromatic mid | `grey` | `gray` — the spelling split, not a colour difference |
//!
//! # Why not a nearest-neighbour search over a palette
//!
//! The obvious alternative is a table of named colours and a nearest-match in Lab.
//! It was rejected because it fails in exactly the case that matters: Miro's sticky
//! palette is pastel, so *every* sticky is far from every saturated reference
//! colour, and the nearest neighbour is decided by lightness noise rather than by
//! hue. Banding on hue with lightness only used to pick between two names for the
//! same hue is stable under exactly the pastels this app is full of.

/// A 24-bit sRGB colour.
///
/// No alpha. Opacity is a rendering property, and two stickies that differ only in
/// how transparent they are are the same colour to somebody searching for one of
/// them. Folding alpha in would also split `#fff79e` into as many facet values as
/// there are opacity settings on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Colour(u32);

impl Colour {
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self((r as u32) << 16 | (g as u32) << 8 | b as u32)
    }

    /// From a packed `0xRRGGBB`. Bits above 24 are discarded rather than rejected,
    /// so a caller holding `0xAARRGGBB` gets the colour it meant.
    pub const fn from_packed(rgb: u32) -> Self {
        Self(rgb & 0x00FF_FFFF)
    }

    pub const fn packed(self) -> u32 {
        self.0
    }

    pub const fn rgb(self) -> (u8, u8, u8) {
        ((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)
    }

    /// Miro stores colours as **decimal integers**, and `-1` means "none" rather
    /// than black — `docs/02-miro-formats.md` §2.2. Reading `-1` as a colour paints
    /// every unstyled widget black and makes `colour:black` match most of a board,
    /// so the sentinel is handled here, once, rather than at each call site.
    pub const fn from_miro_decimal(value: i64) -> Option<Self> {
        if value < 0 || value > 0x00FF_FFFF { None } else { Some(Self(value as u32)) }
    }

    /// Parses `#fff79e`, `fff79e`, `#fa0` or `fa0`. Case-insensitive.
    ///
    /// Returns `None` rather than a fallback colour: a filter of `colour:teel` must
    /// be recognised as a *name* that failed to parse as hex, not silently become
    /// black.
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(digits, 16).ok()?;
        match digits.len() {
            // The 3-digit form doubles each nibble: `fa0` is `ffaa00`, not `0ffa00`.
            3 => {
                let (r, g, b) = ((value >> 8) & 0xF, (value >> 4) & 0xF, value & 0xF);
                Some(Self::from_rgb((r * 17) as u8, (g * 17) as u8, (b * 17) as u8))
            }
            6 => Some(Self(value)),
            _ => None,
        }
    }

    /// Lowercase `#rrggbb`. This is the exact facet value the index stores, so it
    /// round-trips through [`Colour::parse`] and through the on-disk form.
    pub fn to_hex(self) -> String {
        format!("#{:06x}", self.0)
    }

    /// Hue in degrees `0..360`, saturation and lightness in `0..1`.
    ///
    /// Plain HSL rather than a perceptual space: the only decisions made from it are
    /// which of eight hue bands the colour is in and whether it is light or dark,
    /// and HSL's hue is the same angle a perceptual space would give to within far
    /// less than a band width. A Lab conversion would be more code for an identical
    /// answer.
    pub fn hsl(self) -> (f32, f32, f32) {
        let (r, g, b) = self.rgb();
        let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let chroma = max - min;
        let lightness = (max + min) / 2.0;

        if chroma <= f32::EPSILON {
            return (0.0, 0.0, lightness);
        }

        let saturation = chroma / (1.0 - (2.0 * lightness - 1.0).abs()).max(f32::EPSILON);
        let hue = if max == r {
            60.0 * (((g - b) / chroma) % 6.0)
        } else if max == g {
            60.0 * ((b - r) / chroma + 2.0)
        } else {
            60.0 * ((r - g) / chroma + 4.0)
        };
        (if hue < 0.0 { hue + 360.0 } else { hue }, saturation.min(1.0), lightness)
    }

    /// The names this colour is indexed and searched under. See the module docs for
    /// the bands and why there are two.
    pub fn names(self) -> ColourNames {
        let (hue, saturation, lightness) = self.hsl();

        // Achromatic first. The threshold is deliberately generous: the palette in
        // `docs/05-design-language.md` is built from neutrals only 4% apart in
        // luminance and 2% in saturation, and calling `#E3E6E8` "cyan" because its
        // hue rounds to 200° would be absurd.
        if saturation < 0.15 {
            return match lightness {
                l if l < 0.12 => ColourNames::single("black"),
                l if l > 0.92 => ColourNames::single("white"),
                _ => ColourNames::pair("grey", "gray"),
            };
        }
        // A colour dark enough that its hue is not perceptible reads as black
        // whatever the arithmetic says.
        if lightness < 0.10 {
            return ColourNames::single("black");
        }

        match hue {
            h if !(15.0..345.0).contains(&h) => {
                if lightness > 0.72 {
                    ColourNames::pair("red", "pink")
                } else {
                    ColourNames::single("red")
                }
            }
            h if h < 45.0 => {
                if lightness < 0.42 {
                    ColourNames::pair("orange", "brown")
                } else {
                    ColourNames::single("orange")
                }
            }
            h if h < 70.0 => ColourNames::single("yellow"),
            h if h < 165.0 => ColourNames::single("green"),
            h if h < 195.0 => ColourNames::pair("cyan", "teal"),
            h if h < 255.0 => ColourNames::single("blue"),
            h if h < 290.0 => ColourNames::pair("purple", "violet"),
            _ => ColourNames::pair("pink", "magenta"),
        }
    }

    /// Whether this colour answers to `name`, case-insensitively.
    pub fn is_named(self, name: &str) -> bool {
        self.names().into_iter().any(|n| n.eq_ignore_ascii_case(name))
    }
}

impl std::fmt::Display for Colour {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:06x}", self.0)
    }
}

/// The one or two names a colour is indexed under.
///
/// A fixed pair rather than a `Vec`: this is produced once per item during indexing
/// and once per bare query term, and neither is a place to allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourNames {
    /// The band's own name — what a colour picker would call it.
    pub primary: &'static str,
    /// A second word for the same band that a person is just as likely to type.
    pub alias: Option<&'static str>,
}

impl ColourNames {
    const fn single(primary: &'static str) -> Self {
        Self { primary, alias: None }
    }

    const fn pair(primary: &'static str, alias: &'static str) -> Self {
        Self { primary, alias: Some(alias) }
    }
}

impl IntoIterator for ColourNames {
    type Item = &'static str;
    type IntoIter = std::iter::Chain<
        std::iter::Once<&'static str>,
        std::option::IntoIter<&'static str>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self.primary).chain(self.alias)
    }
}

/// Every word [`Colour::names`] can return.
///
/// The query layer consults this to decide whether a bare term like `yellow` is a
/// colour worth treating as a facet. It is asserted in the tests to be exactly the
/// set the banding produces, so adding a band without adding its name here is a
/// test failure rather than a silently unsearchable colour.
pub const COLOUR_NAMES: &[&str] = &[
    "black", "blue", "brown", "cyan", "gray", "green", "grey", "magenta", "orange", "pink",
    "purple", "red", "teal", "violet", "white", "yellow",
];

/// Whether `word` is one of [`COLOUR_NAMES`], case-insensitively.
pub fn is_colour_name(word: &str) -> bool {
    COLOUR_NAMES.iter().any(|n| n.eq_ignore_ascii_case(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two sticky colours the SVG oracle recovered from the reference board.
    /// These are the exact values a real search has to reach, so they are the test.
    #[test]
    fn the_reference_boards_two_sticky_colours_get_the_names_people_would_type() {
        let common = Colour::parse("#fff79e").unwrap();
        assert!(common.is_named("yellow"), "43 of 44 stickies are this colour");
        assert_eq!(common.names().alias, None);

        let outlier = Colour::parse("#ff9e9e").unwrap();
        assert!(outlier.is_named("red"), "hue is 0deg");
        assert!(outlier.is_named("pink"), "but it is pale, and reads as pink");
    }

    #[test]
    fn the_design_languages_neutrals_are_never_mistaken_for_hues() {
        for hex in ["#F4F5F6", "#EFF1F2", "#E3E6E8", "#DDE2E5", "#1C1F23", "#2B2F34", "#3A4048"] {
            let colour = Colour::parse(hex).unwrap();
            let names: Vec<_> = colour.names().into_iter().collect();
            assert!(
                names.iter().all(|n| ["black", "white", "grey", "gray"].contains(n)),
                "{hex} named {names:?}",
            );
        }
    }

    #[test]
    fn the_accent_colours_land_in_their_own_bands() {
        assert!(Colour::parse("#E65B58").unwrap().is_named("red"), "xr-red, light mode");
        assert!(Colour::parse("#C8102E").unwrap().is_named("red"), "xr-red, dark mode");
        assert!(Colour::parse("#6FD6E6").unwrap().is_named("cyan"), "monitor-cyan, light");
        assert!(Colour::parse("#57E5FF").unwrap().is_named("teal"), "cyan answers to teal");
    }

    #[test]
    fn hex_parsing_accepts_the_forms_a_person_pastes() {
        let expected = Colour::from_rgb(0xFF, 0xF7, 0x9E);
        assert_eq!(Colour::parse("#fff79e"), Some(expected));
        assert_eq!(Colour::parse("FFF79E"), Some(expected));
        assert_eq!(Colour::parse("#FA0"), Some(Colour::from_rgb(0xFF, 0xAA, 0x00)));
        assert_eq!(Colour::parse("#ffff"), None, "four digits is not a colour");
        assert_eq!(Colour::parse("yellow"), None, "a name is not hex");
        assert_eq!(Colour::parse(""), None);
    }

    #[test]
    fn hex_round_trips_through_the_string_the_index_stores() {
        for value in [0x000000, 0xFFFFFF, 0xFFF79E, 0x123456] {
            let colour = Colour::from_packed(value);
            assert_eq!(Colour::parse(&colour.to_hex()), Some(colour));
        }
    }

    #[test]
    fn miros_minus_one_is_no_colour_rather_than_black() {
        assert_eq!(Colour::from_miro_decimal(-1), None);
        assert_eq!(Colour::from_miro_decimal(16775070), Some(Colour::parse("#fff79e").unwrap()));
        assert_eq!(Colour::from_miro_decimal(1710618), Some(Colour::parse("#1a1a1a").unwrap()));
        assert_eq!(Colour::from_miro_decimal(0x1_0000_0000), None);
    }

    /// Every name the banding can produce must be in `COLOUR_NAMES`, or a query for
    /// it would be treated as an ordinary word and match nothing.
    #[test]
    fn the_published_name_list_covers_every_name_the_bands_produce() {
        let mut produced = std::collections::BTreeSet::new();
        for r in (0..=255u32).step_by(15) {
            for g in (0..=255u32).step_by(15) {
                for b in (0..=255u32).step_by(15) {
                    let colour = Colour::from_rgb(r as u8, g as u8, b as u8);
                    produced.extend(colour.names());
                }
            }
        }
        let published: std::collections::BTreeSet<_> = COLOUR_NAMES.iter().copied().collect();
        assert_eq!(produced, published);
        assert!(COLOUR_NAMES.windows(2).all(|w| w[0] < w[1]), "kept sorted for review");
    }

    #[test]
    fn hsl_agrees_with_hand_computed_values() {
        let (hue, saturation, lightness) = Colour::parse("#fff79e").unwrap().hsl();
        assert!((hue - 55.0).abs() < 1.0, "hue {hue}");
        assert!(saturation > 0.95, "saturation {saturation}");
        assert!((lightness - 0.81).abs() < 0.01, "lightness {lightness}");

        let (_, saturation, lightness) = Colour::from_rgb(128, 128, 128).hsl();
        assert_eq!(saturation, 0.0, "a pure grey has no saturation and no hue");
        assert!((lightness - 0.502).abs() < 0.01);
    }
}
