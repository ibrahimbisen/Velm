//! Turning a Velm document into something a renderer can draw.
//!
//! Three things that were always pure and are now reachable from a browser:
//!
//! - [`project`] — the document's items become [`vellum_scene::Scene`] entries, with the
//!   z-band arithmetic that puts frames behind everything and the bounds every hit-test and
//!   every camera fit is measured against.
//! - [`connector`] — where a line between two items actually attaches, and how it routes.
//! - [`theme`] — the canvas palette, kept in step with the chrome's by a test rather than by
//!   hope.
//!
//! Nothing here opens a window, a file or a socket. That is what makes it the layer a wasm
//! viewer needs, and it is why extracting it cost no change to any caller.

/// Where a link card's three voices go and what they say -- pure, and tested here
/// because `vellum-web` is `cfg(target_arch = "wasm32")` and can hold no runnable test.
pub mod card;
/// Every item's words, including the four whose text is inside an opaque JSON token —
/// so a search in a browser finds what a search on the Mac finds.
pub mod words;
/// The on-canvas caret: buffer, cursor, grapheme and word motion, and the splice that keeps a
/// half-bold sticky half bold. Pure — it names no window and no document, only offsets.
pub mod edit;
/// The exporter's adapter: a projection becomes a `vellum_export::Board`, with the one seam
/// the document cannot answer — where an image's bytes and a font's faces come from.
pub mod export;
/// Resize and rotate geometry, for one item and for a group. Pure arithmetic on rectangles.
pub mod handle;
pub mod connector;
pub mod frame;
pub mod look;
pub mod project;
pub mod runs;
pub mod theme;
