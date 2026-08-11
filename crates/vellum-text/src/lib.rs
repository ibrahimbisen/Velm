//! Text for Vellum — the project's acknowledged long pole.
//!
//! `docs/01-architecture.md` §6 names text the largest risk in the codebase, and is
//! specific about why: `cosmic-text` gives shaping, wrapping and plain-text editing,
//! but **not** rich runs, **not** HTML paste, and **not** Miro's auto-fit sizing.
//! This crate is those missing pieces, in the order they pay off:
//!
//! 1. [`from_miro_html`] — Miro stores every sticky and text widget as rich-text
//!    HTML. On the reference board that is 44 stickies and 46 text widgets, so this
//!    is the difference between an import that looks right and one that does not.
//! 2. [`TextEngine::layout`] and [`TextEngine::measure`] — shaping and wrapping,
//!    flattened into owned, block-relative glyphs a renderer can hold across frames.
//! 3. [`TextEngine::fit_font_size`] — Miro's `fs: 0` + `fsa: 1` auto-fit, which
//!    *every* sticky on the board uses.
//! 4. [`TextEngine::rasterise`] — glyph coverage bitmaps and their metrics, as data.
//!    No GPU code lives here.
//!
//! The mitigation §6 sets out is followed exactly: rich runs are *rendered* now
//! (cheap) while editing stays plain-text (expensive), and spans exist from the
//! first commit so nothing needs migrating later.
//!
//! ```no_run
//! use vellum_text::{AutoFit, FitBox, LayoutParams, TextEngine, from_miro_html};
//!
//! # fn main() -> Result<(), vellum_text::TextError> {
//! // A sticky exactly as it arrives from the clipboard.
//! let text = from_miro_html("<p>fan</p><p><br /></p>");
//! assert_eq!(text.to_plain(), "fan\n");
//!
//! let mut engine = TextEngine::new()?;
//! let params = LayoutParams { font_family: Some("Noto Sans".into()), ..Default::default() };
//!
//! // `fs: 0, fsa: 1` — size the text to the note.
//! let size = engine.fit_font_size(&text, &params, FitBox::new(199.0, 228.0), &AutoFit::default());
//!
//! let layout = engine.layout(&text, &params.with_font_size(size).with_max_width(Some(199.0)));
//! for (key, image) in engine.atlas_entries(&layout, (0.0, 0.0), 1.0) {
//!     let _ = (key, image.width, image.height, image.data);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Why this crate does not depend on `vellum-doc`
//!
//! The dependency direction in §2 is `doc → text`. [`StyledText`] here is therefore
//! a deliberate twin of `vellum_doc::StyledText` rather than a re-export — see
//! [`span`] for the field-by-field correspondence and the conversion. The importer
//! and the renderer can both use this crate without pulling in a CRDT.
//!
//! # What is not here yet
//!
//! Editing. §6 is explicit that shipping plain-text editing first is the plan, and
//! caret movement, selection and IME candidate placement are the next slice. What
//! this crate already produces — per-glyph [`cluster`] ranges and per-line baselines
//! — is what that slice will be built on.
//!
//! [`cluster`]: PlacedGlyph::cluster

mod css;
mod entity;

pub mod autofit;
pub mod error;
pub mod html;
pub mod layout;
pub mod raster;
pub mod span;

pub use autofit::{AutoFit, FitBox};
pub use error::{Result, TextError};
pub use html::from_miro_html;
pub use layout::{BUNDLED_FAMILY, BUNDLED_FONTS, 
    Caret, DEFAULT_FONT_SIZE, DEFAULT_LINE_HEIGHT, Decoration, DecorationRun, FontId, GlyphKey,
    LaidOutLine, Layout, LayoutParams, MAX_FONT_SIZE, MIN_FONT_SIZE, PhysicalGlyph, PlacedGlyph,
    SelectionBox, TextAlign, TextEngine, TextExtent,
};
pub use raster::{GlyphContent, GlyphImage};
pub use span::{Rgb, SpanStyle, StyledText, TextSpan};
