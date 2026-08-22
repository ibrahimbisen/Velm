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

pub mod connector;
pub mod frame;
pub mod look;
pub mod project;
pub mod runs;
pub mod theme;
