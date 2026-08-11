//! Fill, stroke and colour, in the smallest form every writer can consume.
//!
//! There is no palette here and no token table. `docs/05-design-language.md` governs
//! Vellum's *chrome*; an export reproduces the user's board, whose colours came from
//! Miro or from the colour picker, and inventing a token for them would be wrong.
//! Colours arrive as literal RGBA and are written out as literal RGBA.

/// Straight (non-premultiplied) 8-bit RGBA.
///
/// Straight rather than premultiplied because every output format here wants it
/// that way: SVG writes `fill` and `fill-opacity` separately, PDF writes a colour
/// and a graphics-state alpha, and CSV does not care. Only the rasteriser
/// premultiplies, and it does so at the last moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`, with or without the `#`.
    ///
    /// Miro's clipboard payload and its SVG export both use the short and long
    /// forms interchangeably, so an importer round-tripping through the exporter
    /// must accept both.
    pub fn from_hex(text: &str) -> Option<Self> {
        let hex = text.trim().trim_start_matches('#');
        let nibble = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
        let byte = |i: usize| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok();
        match hex.len() {
            3 | 4 => {
                let a = if hex.len() == 4 { nibble(3)? } else { 15 };
                // `f` means `ff`, not `f0`: repeating the nibble is what CSS does.
                let expand = |v: u8| v * 17;
                Some(Self::rgba(
                    expand(nibble(0)?),
                    expand(nibble(1)?),
                    expand(nibble(2)?),
                    expand(a),
                ))
            }
            6 | 8 => {
                let a = if hex.len() == 8 { byte(3)? } else { 255 };
                Some(Self::rgba(byte(0)?, byte(1)?, byte(2)?, a))
            }
            _ => None,
        }
    }

    /// `#rrggbb`. Alpha is deliberately not encoded — every writer here carries it
    /// in a separate opacity channel, and `#rrggbbaa` is not universally understood
    /// by SVG renderers.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    pub fn opacity(self) -> f32 {
        f32::from(self.a) / 255.0
    }

    pub fn is_transparent(self) -> bool {
        self.a == 0
    }
}

/// How a stroke's dashes are laid out.
///
/// Miro exposes solid, dashed and dotted; the pattern lengths scale with the stroke
/// width so a 1px dashed border and an 8px one look like the same style rather than
/// like a dashed line and a row of blocks.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dashed,
    Dotted,
    /// An explicit dash array in board units, used as given.
    Custom(Vec<f64>),
}

impl Dash {
    /// The dash array for a stroke of `width`, or `None` for a solid line.
    pub fn pattern(&self, width: f64) -> Option<Vec<f64>> {
        let w = width.max(f64::EPSILON);
        match self {
            Self::Solid => None,
            Self::Dashed => Some(vec![w * 4.0, w * 3.0]),
            // A round-capped zero-length dash is a dot; a bare `0` would vanish
            // under butt caps, so the on-length is a hair rather than nothing.
            Self::Dotted => Some(vec![w * 0.01, w * 2.0]),
            Self::Custom(pattern) if pattern.is_empty() => None,
            Self::Custom(pattern) => Some(pattern.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

impl LineCap {
    pub(crate) const fn svg(self) -> &'static str {
        match self {
            Self::Butt => "butt",
            Self::Round => "round",
            Self::Square => "square",
        }
    }

    /// PDF's `J` operand.
    pub(crate) const fn pdf(self) -> i32 {
        match self {
            Self::Butt => 0,
            Self::Round => 1,
            Self::Square => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}

impl LineJoin {
    pub(crate) const fn svg(self) -> &'static str {
        match self {
            Self::Miter => "miter",
            Self::Round => "round",
            Self::Bevel => "bevel",
        }
    }

    /// PDF's `j` operand.
    pub(crate) const fn pdf(self) -> i32 {
        match self {
            Self::Miter => 0,
            Self::Round => 1,
            Self::Bevel => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub color: Color,
    /// Width in board units. Strokes are centred on the path, as in SVG and PDF.
    pub width: f64,
    pub dash: Dash,
    pub cap: LineCap,
    pub join: LineJoin,
}

impl Stroke {
    pub fn new(color: Color, width: f64) -> Self {
        Self { color, width, dash: Dash::Solid, cap: LineCap::Butt, join: LineJoin::Miter }
    }

    /// Ink and connectors are drawn with round caps and joins so a stroke reads as
    /// one continuous mark rather than a chain of mitred segments.
    pub fn ink(color: Color, width: f64) -> Self {
        Self { color, width, dash: Dash::Solid, cap: LineCap::Round, join: LineJoin::Round }
    }

    pub fn with_dash(mut self, dash: Dash) -> Self {
        self.dash = dash;
        self
    }

    /// True when the stroke would put no pixels down, and can therefore be skipped
    /// entirely rather than written as a no-op.
    pub fn is_invisible(&self) -> bool {
        self.width <= 0.0 || self.color.is_transparent()
    }
}

/// An item's paint.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Style {
    pub fill: Option<Color>,
    pub stroke: Option<Stroke>,
    /// Multiplies the whole item — fill, stroke, text and image alike — as Miro's
    /// item opacity does. Fill alpha is separate and multiplies on top of it.
    pub opacity: f32,
}

impl Style {
    pub const NONE: Self = Self { fill: None, stroke: None, opacity: 1.0 };

    pub fn filled(color: Color) -> Self {
        Self { fill: Some(color), stroke: None, opacity: 1.0 }
    }

    pub fn with_stroke(mut self, stroke: Stroke) -> Self {
        self.stroke = Some(stroke);
        self
    }

    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity.clamp(0.0, 1.0);
        self
    }

    /// The fill actually visible, after item opacity — `None` when nothing would
    /// be drawn.
    pub fn effective_fill(&self) -> Option<Color> {
        self.fill.filter(|c| !c.is_transparent() && self.opacity > 0.0)
    }

    pub fn effective_stroke(&self) -> Option<&Stroke> {
        self.stroke.as_ref().filter(|s| !s.is_invisible() && self.opacity > 0.0)
    }

    /// Half the stroke width, which is how far paint spills outside the geometry.
    pub fn stroke_overhang(&self) -> f64 {
        self.effective_stroke().map_or(0.0, |s| s.width / 2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hex_repeats_the_nibble() {
        assert_eq!(Color::from_hex("#fff"), Some(Color::WHITE));
        assert_eq!(Color::from_hex("f00"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::from_hex("#0000"), Some(Color::TRANSPARENT));
    }

    #[test]
    fn long_hex_round_trips() {
        let c = Color::from_hex("#FFF79E").expect("Miro's default sticky yellow");
        assert_eq!(c, Color::rgb(0xff, 0xf7, 0x9e));
        assert_eq!(c.to_hex(), "#fff79e");
        assert_eq!(Color::from_hex("#12345678").map(|c| c.a), Some(0x78));
    }

    #[test]
    fn malformed_hex_is_rejected_rather_than_guessed() {
        for bad in ["", "#", "#12", "#12345", "#zzz", "#1234567"] {
            assert_eq!(Color::from_hex(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn dash_lengths_scale_with_stroke_width() {
        let thin = Dash::Dashed.pattern(1.0).unwrap();
        let thick = Dash::Dashed.pattern(8.0).unwrap();
        assert_eq!(thick, thin.iter().map(|v| v * 8.0).collect::<Vec<_>>());
        assert!(Dash::Solid.pattern(4.0).is_none());
        assert!(Dash::Custom(Vec::new()).pattern(4.0).is_none(), "an empty array is solid");
    }

    #[test]
    fn invisible_paint_is_reported_as_nothing_to_draw() {
        assert!(Stroke::new(Color::BLACK, 0.0).is_invisible());
        assert!(Stroke::new(Color::TRANSPARENT, 2.0).is_invisible());

        let hidden = Style::filled(Color::BLACK).with_opacity(0.0);
        assert!(hidden.effective_fill().is_none());
        assert!(Style::filled(Color::TRANSPARENT).effective_fill().is_none());
    }

    #[test]
    fn stroke_overhang_is_half_the_width() {
        let s = Style::NONE.with_stroke(Stroke::new(Color::BLACK, 6.0));
        assert_eq!(s.stroke_overhang(), 3.0);
        assert_eq!(Style::NONE.stroke_overhang(), 0.0);
    }
}
