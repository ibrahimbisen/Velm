//! Errors a board document can produce.

use crate::item::ItemId;

/// Something went wrong reading or editing a board.
///
/// The variants separate *caller mistakes* (asking for an item that is gone,
/// reparenting into a cycle) from *file problems* (a schema from the future, a
/// malformed document) because the UI treats them differently: the first is a
/// no-op, the second needs telling the user their file is not openable.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("no item {0} on this board")]
    NoSuchItem(ItemId),

    #[error("item {id} is a {found}, not a {expected}")]
    WrongKind { id: ItemId, expected: &'static str, found: &'static str },

    #[error("cannot make {child} a child of {parent}: an item cannot contain itself")]
    CyclicReparent { child: ItemId, parent: ItemId },

    #[error(
        "this board uses document schema v{found}; this build understands up to v{supported}"
    )]
    UnsupportedSchema { found: i64, supported: i64 },

    #[error("board document is malformed: {0}")]
    Malformed(String),

    #[error(transparent)]
    Crdt(#[from] loro::LoroError),

    #[error(transparent)]
    Encoding(#[from] loro::LoroEncodeError),
}

pub type Result<T> = std::result::Result<T, DocError>;
