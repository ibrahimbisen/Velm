//! Errors this crate can return.
//!
//! There is exactly one, and it is deliberate that HTML conversion is not part of
//! it: `from_miro_html` is total. Everything that *can* fail here is about the
//! environment — whether any font exists to shape with — not about the content.

/// Something in the text pipeline could not be set up.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TextError {
    /// No font could be found at all, so nothing can be shaped, measured or drawn.
    ///
    /// Reported rather than tolerated: with an empty font database `cosmic-text`
    /// silently lays out zero glyphs, so every sticky would auto-fit to the maximum
    /// size and render blank — a failure that looks like a renderer bug and would
    /// be debugged as one.
    #[error("no fonts are available: install a system font or supply one with TextEngine::with_fonts")]
    NoFontsAvailable,
}

/// Convenience alias for fallible setup in this crate.
pub type Result<T> = std::result::Result<T, TextError>;
