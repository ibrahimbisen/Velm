//! Board export for Vellum — **SVG, PDF, PNG and CSV**.
//!
//! `docs/features/README.md` §7 lists the four targets and `docs/04-ui-reference.md`
//! §4 puts them behind *Board → Export*, replacing the cloud entries Miro has there.
//! This crate is the whole of that: pure serialisation, no document layer, no GPU.
//!
//! # The shape of it
//!
//! ```text
//!   BoardSource ──collect──▶ Scene ──▶ svg::write   → String
//!   (a trait)                 │        pdf::write   → Vec<u8>
//!                             │        csv::write   → String
//!                             └──────▶ raster::…    → Raster → PNG
//! ```
//!
//! [`BoardSource`] is the input contract — an iterator of placed items with
//! geometry, style and text, plus lookups for images and fonts. [`source`] documents
//! how a `Board` maps onto it, field by field. [`Scene::collect`] resolves the three
//! things that are easy to get *differently* in four writers and must not be —
//! **scope, z-order and clipping** — once, so the four outputs cannot disagree.
//!
//! # Getting one out
//!
//! ```
//! use vellum_export::{
//!     Color, Geometry, Item, Kind, Rect, Scene, Scope, Snapshot, Style, csv, svg,
//! };
//!
//! let board = Snapshot::new(vec![
//!     Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 400.0, 300.0)))
//!         .with_name("Coolant System"),
//!     Item::new(2, Kind::Sticky, Geometry::rect(Rect::new(20.0, 20.0, 100.0, 100.0)))
//!         .in_frame(vellum_export::ItemId(1))
//!         .with_style(Style::filled(Color::from_hex("#fff79e").unwrap())),
//! ])
//! .with_title("Reference Board");
//!
//! let scene = Scene::collect(&board, &Scope::Board)?;
//! let vector = svg::write(&scene, &svg::SvgOptions::default())?;
//! assert!(vector.contains("data-frame=\"true\""));
//!
//! // The same scene, as a spreadsheet — or as one PDF page per frame.
//! let _ = csv::write(&scene, &csv::CsvOptions::default());
//! # Ok::<(), vellum_export::ExportError>(())
//! ```
//!
//! # Fidelity, checked against someone else's parser
//!
//! `vellum-import` already reads Miro's SVG export and counts what is in it, and
//! `docs/01-architecture.md` §8 calls that the strongest correctness signal in the
//! project. The SVG written here carries the same structural markers, so
//! `tests/roundtrip.rs` writes a board, reads it back through
//! `vellum_import::svg::read`, and compares the counts through the importer's own
//! `SvgInventory::compare`. Agreement between a writer and an independently written
//! reader is worth more than any assertion this crate could make about itself.
//!
//! # Known gaps
//!
//! Stated here rather than discovered later:
//!
//! - **Text is not shaped.** Lines arrive already laid out; see [`text`]. Advances
//!   used for centring are unkerned, which is a fraction of a percent for Latin and
//!   wrong for complex scripts.
//! - **PDF fonts are embedded whole, not subset.** See [`pdf`].
//! - **The CPU rasteriser decodes only PNG.** A JPEG must arrive as
//!   [`ImageData::Rgba8`] to appear in a PNG export; it embeds untouched in SVG and
//!   PDF either way. See [`raster`].
//! - **Only the SVG writer draws frame captions.** In a PDF the frame is the page
//!   and in a PNG the frame is the image, so a caption stamped inside either would
//!   be content the user never placed — but a whole-board PNG does therefore lose
//!   the frame names that the same board's SVG shows.
//! - **A large curved shape reads back as ink as well as a shape.** The SVG reader
//!   tells the two apart by path length; see [`svg`], where the limit is pinned by a
//!   test rather than left to be rediscovered.
//! - **No shadows or blurs.** Nothing in the item model has them yet.

pub mod csv;
pub mod error;
mod font;
pub mod geom;
pub mod item;
pub mod pdf;
pub mod raster;
pub mod scene;
pub mod source;
pub mod style;
pub mod svg;
pub mod text;

pub use error::ExportError;
pub use geom::{Affine, Path, Point, Rect, Segment, SubPath, Terminal, pt};
pub use item::{EndCap, Geometry, ImageRef, Item, ItemId, Kind, ZOrder};
pub use raster::{CpuRasteriser, Raster, RasterRequest, Rasteriser};
pub use scene::{Frame, Placed, Scene};
pub use source::{BoardSource, ImageData, Scope, Snapshot};
pub use style::{Color, Dash, LineCap, LineJoin, Stroke, Style};
pub use text::{Align, FaceKey, FontSpec, Span, TextBlock, TextLine, VAlign};
