//! Reads Miro board content into Vellum.
//!
//! Miro's own `.rtb` backup ships its board content (`canvas.json`) encrypted with a
//! server-side key, so it cannot be read — `docs/02-miro-formats.md` records the
//! analysis that established this. What *is* readable, and what this crate is built
//! around, is Miro's clipboard: copying objects in Miro places its full internal
//! widget model on the system clipboard, obfuscated but not encrypted.
//!
//! That route carries more than any Miro API does — notably `paint` (freehand ink),
//! which no REST or Web SDK endpoint exposes at all.
//!
//! [`import`] is the whole paste path: a clipboard payload, optionally joined
//! against a `.rtb` for asset bytes, becomes items on a real [`Board`] with their
//! hierarchy, z-order and assets intact. [`import_clipboard`] stops one step short,
//! at Miro's widget model, for tooling that wants to inspect rather than build.
//!
//! ```no_run
//! use vellum_import::{import, rtb::ArchiveSet};
//!
//! # fn main() -> anyhow::Result<()> {
//! let html = std::fs::read_to_string("clipboard.html")?;
//! // A single `.rtb`, or a whole folder of them searched as one. A migration is
//! // many boards and so many backups; resolution is by resource id across the set.
//! let mut archive = ArchiveSet::open("~/Library/Application Support/Vellum/archives")?;
//! let blobs = vellum_store::BlobStore::open("~/.vellum/blobs")?;
//! let mut board = vellum_doc::Board::new();
//!
//! if let Some(outcome) = import(&html, Some(&mut archive), &blobs, &mut board)? {
//!     println!("{outcome}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`Board`]: vellum_doc::Board

pub mod clipboard;
pub mod mapper;
pub mod miro_model;
pub mod pipeline;
pub mod rtb;
pub mod svg;

pub use miro_model::{FidelityReport, Widget, WidgetKind};
pub use pipeline::{
    AssetGap, Degradation, ImportOutcome, MissingAsset, PrefetchedAssets, import,
    import_widgets_with, prefetch_assets, requested_assets,
};

/// A board fragment pasted out of Miro.
#[derive(Debug, Clone)]
pub struct ImportedBoard {
    /// Miro's board id, e.g. `"bTBja0JvYXJkSWQ="`. Several pastes sharing this id came
    /// from the same board and can be merged.
    pub source_board_id: Option<String>,
    pub widgets: Vec<Widget>,
    pub report: FidelityReport,
    /// The byte offset the payload actually used. Recorded so a change in Miro's
    /// obfuscation shows up in logs rather than passing unnoticed.
    pub byte_shift: u8,
}

/// Imports a Miro clipboard payload from the clipboard's HTML flavour.
///
/// `Ok(None)` means the HTML was not Miro's — the ordinary case when pasting from
/// anywhere else. `Err` means it *was* Miro's and could not be read.
pub fn import_clipboard(clipboard_html: &str) -> anyhow::Result<Option<ImportedBoard>> {
    let Some(decoded) = clipboard::decode(clipboard_html)? else {
        return Ok(None);
    };
    let (widgets, report) = mapper::map_objects(decoded.objects());
    Ok(Some(ImportedBoard {
        source_board_id: decoded.board_id().map(str::to_string),
        widgets,
        report,
        byte_shift: decoded.byte_shift,
    }))
}

