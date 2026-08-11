//! Colour, in the one form every instance struct carries.
//!
//! Straight (non-premultiplied) RGBA, in the surface's colour space. Miro stores
//! colours as sRGB hex and blends in sRGB as browsers do, and `vellum-app` picks a
//! non-sRGB `Unorm` surface format so `#fff79e` reaches the display as `#fff79e`;
//! this type is the value that travels that path unchanged.
//!
//! Shaders premultiply on output — see `shaders/common.wgsl` — so nothing outside
//! WGSL ever has to reason about premultiplied arithmetic.

/// Straight RGBA, each channel `0.0..=1.0`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Self = Self::new(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Self = Self::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0, 1.0);

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// From 8-bit sRGB channels, which is the form Miro's `#rrggbb` decodes to.
    pub fn from_rgb8(r: u8, g: u8, b: u8) -> Self {
        Self::new(f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0)
    }

    /// From a packed `0xRRGGBB`, the literal Miro writes.
    pub fn from_hex(hex: u32) -> Self {
        Self::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    pub fn is_invisible(self) -> bool {
        self.a <= 0.0
    }

    /// Rounded to 8 bits per channel, which is how a mesh vertex stores its colour.
    ///
    /// Meshes pay for colour once per *vertex* rather than once per instance, and a
    /// stroke at deep zoom is a hundred thousand of them; four bytes against sixteen
    /// is 1.2 MB saved on the reference board's ink alone, for a quantisation no
    /// 8-bit-per-channel display can resolve.
    pub fn pack(self) -> [u8; 4] {
        [
            quantise(self.r),
            quantise(self.g),
            quantise(self.b),
            quantise(self.a),
        ]
    }
}

impl From<[f32; 4]> for Rgba {
    /// Accepts the array form `vellum_scene::RenderPayload::SolidQuad` carries.
    fn from(v: [f32; 4]) -> Self {
        Self::new(v[0], v[1], v[2], v[3])
    }
}

impl From<Rgba> for [f32; 4] {
    fn from(c: Rgba) -> Self {
        [c.r, c.g, c.b, c.a]
    }
}

/// Clamps before scaling: a NaN from a corrupt style value would otherwise cast to
/// an unspecified byte and render as a random colour rather than as black.
fn quantise(v: f32) -> u8 {
    if v.is_nan() {
        return 0;
    }
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_through_eight_bit_channels() {
        // Miro's default sticky yellow.
        let c = Rgba::from_hex(0xff_f79e);
        assert_eq!(c.pack(), [0xff, 0xf7, 0x9e, 0xff]);
        assert_eq!(Rgba::from_rgb8(0xff, 0xf7, 0x9e), c);
    }

    #[test]
    fn packing_clamps_and_survives_nonsense() {
        assert_eq!(Rgba::new(-1.0, 2.0, f32::NAN, 0.5).pack(), [0, 255, 0, 128]);
        assert_eq!(Rgba::WHITE.pack(), [255; 4]);
        assert_eq!(Rgba::TRANSPARENT.pack(), [0; 4]);
    }

    #[test]
    fn the_array_form_round_trips() {
        let c = Rgba::new(0.25, 0.5, 0.75, 1.0);
        assert_eq!(Rgba::from(<[f32; 4]>::from(c)), c);
    }

    #[test]
    fn alpha_zero_is_invisible() {
        assert!(Rgba::TRANSPARENT.is_invisible());
        assert!(Rgba::BLACK.with_alpha(0.0).is_invisible());
        assert!(!Rgba::BLACK.is_invisible());
    }
}
