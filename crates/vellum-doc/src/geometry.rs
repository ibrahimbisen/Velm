//! Geometry and colour primitives shared by every item kind.
//!
//! These deliberately mirror `vellum_import::miro_model` so a decoded Miro widget
//! becomes a Vellum item without a lossy intermediate representation. Two
//! differences are intentional:
//!
//! - Size is resolved here (`f64`, not `Option<f64>`). Miro can omit width/height
//!   for auto-sized widgets; a *board* item always occupies space, so the importer
//!   resolves the default once rather than every renderer and hit-test rediscovering
//!   that a `None` means "measure the text".
//! - Colour carries alpha. Miro splits opacity into a separate style key (`lo` on
//!   ink, `bro` on borders), which would leave the renderer reconciling two fields
//!   that must always be applied together.

/// A point in board world space.
///
/// World space is `f64` because the canvas is effectively unbounded: the reference
/// Miro board is already 41283 × 17515 px, and at deep zoom `f32` world coordinates
/// visibly jitter. Camera-relative `f32` is computed per frame by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Where an item sits on the board.
///
/// `x`/`y` are **absolute** world coordinates, not offsets from the parent. This
/// matches Miro, whose `_position.offsetPx` stays absolute regardless of `_parent`,
/// and it means reparenting an item into a frame never moves it — parenting is a
/// grouping relationship, not a coordinate transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    /// Uniform scale applied about the item's centre.
    pub scale: f64,
    /// Rotation in degrees, clockwise — the same convention Miro uses, so imported
    /// values need no conversion.
    pub rotation: f64,
    pub width: f64,
    pub height: f64,
}

impl Placement {
    /// An unrotated, unscaled item at `(x, y)`.
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, scale: 1.0, rotation: 0.0, width, height }
    }

    /// Width and height after `scale`, which is what the renderer and hit-tester
    /// actually need.
    pub fn scaled_size(&self) -> (f64, f64) {
        (self.width * self.scale, self.height * self.scale)
    }
}

impl Default for Placement {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, scale: 1.0, rotation: 0.0, width: 0.0, height: 0.0 }
    }
}

/// A crop rectangle in *source image* pixels, matching Miro's `crop` field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// An sRGB colour with straight (non-premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xFF }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Packs to `0xRRGGBBAA`.
    ///
    /// The document stores colours as one integer rather than four keys so a colour
    /// change is a single CRDT operation and can never be observed half-applied.
    /// Note this is *not* Miro's packing (`0xRRGGBB`, alpha carried separately); the
    /// importer converts.
    pub const fn to_packed(self) -> i64 {
        ((self.r as i64) << 24) | ((self.g as i64) << 16) | ((self.b as i64) << 8) | self.a as i64
    }

    /// Inverse of [`Color::to_packed`]. Out-of-range values yield `None` rather than
    /// wrapping to an arbitrary colour, so a corrupt field reads as "no colour".
    pub fn from_packed(v: i64) -> Option<Self> {
        if !(0..=0xFFFF_FFFF).contains(&v) {
            return None;
        }
        Some(Self {
            r: (v >> 24) as u8,
            g: (v >> 16) as u8,
            b: (v >> 8) as u8,
            a: v as u8,
        })
    }

    /// CSS-style hex. Alpha is emitted only when it is not opaque, so the common
    /// case reads as the familiar six-digit form and compares equal to Miro's SVG
    /// export.
    pub fn to_hex(self) -> String {
        if self.a == 0xFF {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }

    /// Folds a separate 0.0–1.0 opacity into the alpha channel. This is how the
    /// importer resolves Miro's `lo`/`bro` keys into a single colour.
    pub fn with_opacity(self, opacity: f64) -> Self {
        let a = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self { a, ..self }
    }

    /// Straight alpha as a 0.0–1.0 fraction.
    pub fn opacity(self) -> f64 {
        self.a as f64 / 255.0
    }
}

/// Horizontal text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
    Right,
}

impl Align {
    /// The token stored in the document. Spelled out rather than Miro's `l`/`c`/`r`
    /// because this is Vellum's own format and a readable document is easier to
    /// debug than a compact one.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "left" => Some(Self::Left),
            "center" => Some(Self::Center),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packing has to survive a document round-trip exactly; a colour that
    /// drifts by one bit per save is the kind of bug that only shows up months in.
    #[test]
    fn colour_packing_round_trips() {
        for c in [
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
            Color::rgb(0xFF, 0xF7, 0x9E),
            Color::rgba(1, 2, 3, 4),
        ] {
            assert_eq!(Color::from_packed(c.to_packed()), Some(c), "{c:?}");
        }
    }

    #[test]
    fn out_of_range_packed_colour_is_none_not_wrapped() {
        assert_eq!(Color::from_packed(-1), None);
        assert_eq!(Color::from_packed(0x1_0000_0000), None);
    }

    /// Miro's canonical sticky yellow, which the SVG export renders as `#fff79e`.
    #[test]
    fn hex_omits_alpha_when_opaque() {
        assert_eq!(Color::rgb(0xFF, 0xF7, 0x9E).to_hex(), "#fff79e");
        assert_eq!(Color::rgba(0xFF, 0xF7, 0x9E, 0x80).to_hex(), "#fff79e80");
    }

    #[test]
    fn opacity_folds_into_alpha_and_back() {
        let c = Color::rgb(10, 20, 30).with_opacity(0.5);
        assert_eq!(c.a, 128);
        assert!((c.opacity() - 0.502).abs() < 0.001);
        assert_eq!(Color::rgb(1, 2, 3).with_opacity(2.0).a, 255);
        assert_eq!(Color::rgb(1, 2, 3).with_opacity(-1.0).a, 0);
    }

    #[test]
    fn align_tags_round_trip() {
        for a in [Align::Left, Align::Center, Align::Right] {
            assert_eq!(Align::from_tag(a.tag()), Some(a));
        }
        assert_eq!(Align::from_tag("justify"), None);
    }

    #[test]
    fn default_placement_is_identity_not_zero_scale() {
        let p = Placement::default();
        assert_eq!(p.scale, 1.0);
        assert_eq!(p.rotation, 0.0);
    }

    #[test]
    fn scaled_size_applies_scale() {
        let mut p = Placement::new(0.0, 0.0, 200.0, 100.0);
        p.scale = 1.85;
        assert_eq!(p.scaled_size(), (370.0, 185.0));
    }
}
