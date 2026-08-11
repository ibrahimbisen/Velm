//! What can go wrong between a piece of geometry and a pixel.
//!
//! Deliberately narrow. Almost everything a caller can get wrong here — a degenerate
//! rect, a zero-alpha colour, an index past the end of a mesh — is a *shape the data
//! can legitimately have* and is handled where it arises, by drawing nothing. Reaching
//! this type means either the caller handed over an image whose buffer does not match
//! its dimensions, or the atlas is configured too small for the text on screen. Both
//! are bugs in the caller rather than transient conditions, and both would otherwise
//! show up as a blank rectangle with no explanation.

/// Errors from uploading to the GPU.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("an image with a zero dimension cannot be uploaded")]
    EmptyImage,

    #[error("a {width}x{height} image needs {expected} bytes of RGBA, but {actual} were given")]
    ImageSize {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },

    #[error(
        "the glyph atlas cannot hold this frame's {glyphs} glyphs in {pages} pages of {page_size}px"
    )]
    AtlasFull {
        glyphs: usize,
        page_size: u32,
        pages: u32,
    },
}
