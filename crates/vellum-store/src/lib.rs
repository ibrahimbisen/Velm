//! Vellum's persistence layer: where boards and their assets live on disk.
//!
//! Three pieces, because the three jobs have different shapes:
//!
//! - [`BoardDb`] — one SQLite database per board, holding the Loro document as a
//!   snapshot plus the incremental updates since, a one-row index so the board
//!   library can be listed without decoding a single document, and the board's
//!   version history.
//! - [`BlobStore`] — one **shared** content-addressed directory for images, PDFs
//!   and other assets, so a screenshot pasted onto twenty boards is stored once.
//!   Large assets go in and come out as streams; a 40MB image is never resident.
//! - [`Autosave`] — a background writer that keeps a board on disk continuously,
//!   without the UI thread ever waiting for a disk.
//!
//! ```
//! use vellum_doc::{Board, ItemKind, NewItem, Placement};
//! use vellum_store::{Autosave, BlobStore, BoardDb};
//!
//! # fn main() -> Result<(), vellum_store::StoreError> {
//! # let home = tempfile::tempdir()?;
//! let blobs = BlobStore::open(home.path().join("blobs"))?;
//! let logo = blobs.put(b"\x89PNG...")?;
//!
//! let mut board = Board::new();
//! board.set_title("Engine bay")?;
//!
//! let db = BoardDb::open(home.path().join("engine-bay.vellum"))?;
//! // Anything the previous session did not finish is reported here.
//! assert!(db.recovery().is_clean());
//!
//! let mut autosave = Autosave::start(db, &board)?;
//! board.add(NewItem::new(
//!     ItemKind::Image { asset_id: logo.to_hex(), crop: None },
//!     Placement::new(0.0, 0.0, 640.0, 480.0),
//! ))?;
//! autosave.record(&board)?; // microseconds; the write happens elsewhere
//! autosave.flush(&board)?; // only because this example is about to look
//!
//! // Listing the library never opens a document.
//! let library = vellum_store::list_boards(home.path())?;
//! assert_eq!(library[0].title, "Engine bay");
//! assert_eq!(library[0].item_count, 1);
//! # Ok(())
//! # }
//! ```

pub mod autosave;
pub mod blob;
pub mod board_db;
pub mod error;

pub use autosave::{Autosave, AutosaveConfig, AutosaveStats};
pub use blob::{BlobReader, BlobStore, Hash, corrupt_blob};
pub use board_db::{
    BOARD_EXTENSION, BoardDb, BoardIndex, MAX_UPDATE_CHUNKS, REPLACED_BY_RESTORE, Recovered,
    Recovery, RestorePoint, STORE_SCHEMA_VERSION, list_boards,
};
pub use error::{Result, StoreError};
