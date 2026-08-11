//! Vellum's shape library — Miro's basic and flowchart shapes, defined once.
//!
//! Every shape is a parametric description inside a **unit box**: `x` and `y` both
//! run `0..1` with y downwards, and the shape always fills that box exactly. The
//! caller maps the box onto an item rect, so one definition serves a 40px icon and
//! a 4000px diagram, and connector anchors — which Miro already stores as
//! normalised `{x, y}` pairs — need no conversion at all.
//!
//! # Two output paths
//!
//! `docs/01-architecture.md` §3 draws the line: analytic shapes are evaluated in a
//! fragment shader, everything else is tessellated. This crate produces the data
//! for both and no GPU code for either.
//!
//! - [`Shape::sdf_params`] returns the parameters a fragment shader needs, for the
//!   shapes whose parameters survive a non-uniform scale — boxes with corner radii,
//!   ellipses, and any polygon. The [`sdf`] module explains why that is exactly the
//!   right dividing line, and gives each formula in WGSL and in Rust.
//! - [`Shape::outline`] returns the geometry for everything, from which
//!   [`Outline::fill_path`] gives a `lyon` path, and [`Outline::tessellate`] gives
//!   an indexed fill mesh plus a stroke mesh.
//!
//! The two paths are checked against each other: for every shape that reports SDF
//! parameters, the test suite asserts the analytic sign agrees with the tessellated
//! outline's hit test across a dense grid.
//!
//! # What a shape can tell you
//!
//! ```
//! use vellum_shapes::{AnchorKind, Shape, Size, p};
//!
//! let star = Shape::star();
//! let outline = star.outline(1.0);
//!
//! // Hit-testing is a real point-in-polygon test, not a box test: most of a
//! // star's bounding box is not the star.
//! assert!(outline.contains(p(0.5, 0.3)));
//! assert!(!outline.contains(p(0.05, 0.05)));
//!
//! // Connector anchors resolve onto the silhouette, so a connector to the north
//! // anchor meets the tip of the star's top point rather than the top of its box.
//! assert!(outline.anchor(AnchorKind::North).point.y < 0.01);
//!
//! // A star is a polygon, so it can be drawn analytically after all.
//! assert!(star.sdf_params(Size::new(200.0, 200.0)).is_some());
//! ```
//!
//! # Layout
//!
//! - [`shape`] — the [`Shape`] enum, its parameters and its geometry. The only
//!   per-shape code in the crate.
//! - [`outline`] — contours, and everything derived from them: bounds, hit-testing,
//!   paths.
//! - [`sdf`] — analytic parameters and the formulas that consume them.
//! - [`mesh`] — `lyon` tessellation into fill and stroke meshes.
//! - [`anchor`] — connector attachment points.
//! - [`mod@unit`] — the unit box's own arithmetic.

pub mod anchor;
pub mod error;
pub mod mesh;
pub mod outline;
pub mod sdf;
pub mod shape;
pub mod unit;

pub use anchor::{Anchor, AnchorKind};
pub use error::ShapeError;
pub use mesh::{Mesh, ShapeMesh, StrokeMesh, StrokeVertex, TessellationOptions};
pub use outline::{Contour, Outline, Segment};
pub use sdf::SdfParams;
pub use shape::{ArrowForm, CATALOGUE, Direction, Shape};
pub use unit::{Bounds, Point, Size, p};
