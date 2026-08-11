//! An 8-bit sRGB colour, and the two questions a chart actually asks of one.
//!
//! Chart geometry has to answer *"is this legible?"* before it can place a label,
//! so this type carries WCAG relative luminance and contrast rather than leaving
//! that to the renderer. A value label dropped inside a bar has to pick ink or paper
//! against the bar's own fill, and that decision belongs where the label is placed,
//! not where it is rasterised.
//!
//! No colour is *invented* here. Every value in the crate comes from
//! [`crate::palette`], which derives from `docs/05-design-language.md`.

use serde::{Deserialize, Serialize};

/// Straight (non-premultiplied) sRGB with an alpha channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Colour {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Colour {
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0 };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// From a `0xRRGGBB` literal. `const` so the palette can be a table of
    /// constants, which is what makes "no widget contains a hex literal"
    /// (design language §1) enforceable by grep.
    pub const fn hex(value: u32) -> Self {
        Self {
            r: ((value >> 16) & 0xFF) as u8,
            g: ((value >> 8) & 0xFF) as u8,
            b: (value & 0xFF) as u8,
            a: 255,
        }
    }

    /// The same colour at a different opacity. Area fills are the series colour at
    /// ~10% — a wash, not a second colour, so that an area chart's identity still
    /// comes from the line on top of it.
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub fn is_opaque(self) -> bool {
        self.a == 255
    }

    /// WCAG relative luminance, from linearised sRGB.
    pub fn relative_luminance(self) -> f32 {
        fn linear(channel: u8) -> f32 {
            let c = channel as f32 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        }
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG contrast ratio, `1.0..=21.0`. Alpha is ignored: a translucent fill's
    /// real contrast depends on what is behind it, which this crate does not know,
    /// and assuming the worst (fully opaque) is the safe direction for the one
    /// decision that uses it.
    pub fn contrast_ratio(self, other: Self) -> f32 {
        let (a, b) = (self.relative_luminance(), other.relative_luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Picks whichever of two text colours reads better on `self`.
    ///
    /// This is the one place text is allowed to change colour with the data: a label
    /// set *inside* a coloured fill has no surface of its own to sit on. Everywhere
    /// else text wears a text token and identity comes from the mark beside it —
    /// a light categorical hue is illegible as text.
    pub fn readable_ink(self, dark_ink: Self, light_ink: Self) -> Self {
        if self.contrast_ratio(dark_ink) >= self.contrast_ratio(light_ink) {
            dark_ink
        } else {
            light_ink
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_literals_unpack_in_the_order_they_are_written() {
        assert_eq!(Colour::hex(0xE65B58), Colour::rgb(0xE6, 0x5B, 0x58));
        assert_eq!(Colour::hex(0x000000).a, 255);
    }

    #[test]
    fn contrast_matches_the_wcag_extremes() {
        let white = Colour::hex(0xFFFFFF);
        let black = Colour::hex(0x000000);
        assert!((white.contrast_ratio(black) - 21.0).abs() < 0.01);
        assert!((white.contrast_ratio(white) - 1.0).abs() < 1e-6);
        // Symmetric, whichever way round it is asked.
        assert_eq!(white.contrast_ratio(black), black.contrast_ratio(white));
    }

    #[test]
    fn ink_inside_a_fill_follows_the_fill_not_the_theme() {
        let dark_ink = Colour::hex(0x1A1D1F);
        let paper = Colour::hex(0xF4F5F6);
        // A deep red bar takes paper-coloured text; a pale one takes ink.
        assert_eq!(Colour::hex(0xAE232A).readable_ink(dark_ink, paper), paper);
        assert_eq!(Colour::hex(0xB0B8C3).readable_ink(dark_ink, paper), dark_ink);
    }
}
