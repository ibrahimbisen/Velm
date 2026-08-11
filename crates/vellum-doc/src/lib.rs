//! Vellum's board document.
//!
//! A [`Board`] is an infinite canvas of [`Item`]s arranged in a tree: frames and
//! groups contain other items, and an item's position in its parent's child list is
//! its z-order. It is backed by the [Loro](https://loro.dev) CRDT, which gives
//! undo/redo, versioning and cheap incremental saves today and multiplayer later
//! without changing the format — see [`board`] for why that trade was made.
//!
//! ```
//! use vellum_doc::{Board, ItemKind, NewItem, Placement, StyledText};
//!
//! # fn main() -> Result<(), vellum_doc::DocError> {
//! let mut board = Board::new();
//! board.set_title("Engine bay")?;
//!
//! let frame = board.add(NewItem::new(
//!     ItemKind::Text { text: StyledText::plain("Cooling") },
//!     Placement::new(0.0, 0.0, 400.0, 80.0),
//! ))?;
//!
//! let note = board.add(
//!     NewItem::new(
//!         ItemKind::Sticky { text: StyledText::plain("fan"), background: None },
//!         Placement::new(20.0, 120.0, 199.0, 228.0),
//!     )
//!     .with_parent(frame),
//! )?;
//!
//! let bytes = board.to_bytes()?;
//! let reopened = Board::from_bytes(&bytes)?;
//! assert_eq!(reopened.parent_of(note), Some(frame));
//! # Ok(())
//! # }
//! ```
//!
//! Persistence — SQLite per board plus a shared content-addressed blob store —
//! lives in `vellum-store`, which is built on [`Board::to_bytes`],
//! [`Board::export_since`] and [`Board::apply`].

pub mod board;
pub mod error;
pub mod geometry;
pub mod item;
pub mod text;

pub use board::{Background, Board, Pattern, SCHEMA_VERSION, Version};
pub use error::{DocError, Result};
pub use geometry::{Align, Color, Crop, Placement, Point};
pub use item::{
    ArrowKind, CardMode, ConnectorCaption, ConnectorEnd, Dash, Item, ItemId, ItemKind, NewItem,
    ParseItemIdError, Routing, Style, ZIndex,
};
pub use text::{BlockStyle, ListKind, SpanStyle, StyledText, TextSpan};
