//! What can go wrong, and nothing that cannot.
//!
//! Export is serialisation, so most "errors" a naive design would raise are simply
//! facts about a board: an item with no fill, text with no resolvable font, an
//! image the blob store has lost. None of those stop an export — they degrade it,
//! and the degradation is reported through
//! [`Raster::warnings`](crate::raster::Raster::warnings) or is visible in the
//! output. The variants below are the cases where there is genuinely no file to
//! write.

use crate::item::ItemId;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The scope resolved to nothing visible. Writing a zero-byte PNG or a
    /// zero-page PDF would be worse than saying so.
    #[error("nothing to export: the selection is empty or entirely clipped away")]
    NothingToExport,

    /// A frame export named an id that is not a frame on this board.
    #[error("no frame with id {0:?} on this board")]
    UnknownFrame(ItemId),

    /// A PDF is one page per frame, so a board with no frames has no pages.
    /// The caller's remedy is to export the board as a single page, or as PNG.
    #[error("this board has no frames, and a PDF is one page per frame")]
    NoFrames,

    /// The requested raster exceeds the pixel budget. Carries the numbers so the
    /// caller can offer a smaller scale rather than a bare failure.
    #[error(
        "a {width}×{height} raster is {pixels} pixels, over the {limit} limit — \
         lower the scale or export a smaller region"
    )]
    RasterTooLarge { width: u32, height: u32, pixels: u64, limit: u64 },

    /// The rasteriser could not allocate its pixel buffer.
    #[error("could not allocate a {width}×{height} pixel buffer")]
    RasterAllocation { width: u32, height: u32 },

    #[error("encoding PNG: {0}")]
    Png(String),

    /// A font was supplied but could not be read well enough to embed.
    #[error("font {family:?} could not be embedded: {reason}")]
    Font { family: String, reason: String },

    #[error("writing {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl ExportError {
    pub(crate) fn io(path: impl std::fmt::Display, source: std::io::Error) -> Self {
        Self::Io { path: path.to_string(), source }
    }

    pub(crate) fn font(family: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Font { family: family.into(), reason: reason.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Error text is read by a user in a dialog, so it says what to do next.
    #[test]
    fn the_raster_limit_message_carries_the_numbers_and_a_remedy() {
        let e = ExportError::RasterTooLarge {
            width: 60_000,
            height: 40_000,
            pixels: 2_400_000_000,
            limit: 268_435_456,
        };
        let text = e.to_string();
        assert!(text.contains("60000×40000"), "{text}");
        assert!(text.contains("lower the scale"), "{text}");
    }

    #[test]
    fn a_missing_frame_names_the_id() {
        assert!(ExportError::UnknownFrame(ItemId(42)).to_string().contains("42"));
    }
}
