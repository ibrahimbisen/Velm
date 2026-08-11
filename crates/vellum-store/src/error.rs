//! Errors the persistence layer can produce.

use crate::blob::Hash;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A blob's bytes no longer hash to the key they are filed under. Content
    /// addressing makes silent corruption detectable, so it is reported rather
    /// than handed to a decoder that would draw garbage.
    #[error("blob {expected} is corrupt: its contents hash to {actual}")]
    CorruptBlob { expected: Hash, actual: Hash },

    #[error("`{0}` is not a 64-character BLAKE3 hex hash")]
    BadHash(String),

    /// The chunk table exists but does not start with a snapshot, so there is
    /// nothing to replay the updates onto.
    #[error("board file is damaged: its first stored chunk is not a snapshot")]
    MissingSnapshot,

    #[error("this board has no restore point {0}")]
    NoSuchRestorePoint(i64),

    /// The background writer is gone, so nothing more can be persisted. Either it
    /// hit an error it could not recover from, or the handle was shut down and then
    /// used. Either way the caller must stop assuming its edits are being saved.
    #[error("the autosave writer is no longer running")]
    AutosaveStopped,

    #[error(
        "this board file uses store schema v{found}; this build understands up to v{supported}"
    )]
    UnsupportedSchema { found: i64, supported: i64 },

    #[error(transparent)]
    Document(#[from] vellum_doc::DocError),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;
