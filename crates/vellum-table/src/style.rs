//! Cell and table styling, and the cascade that resolves one from the other.
//!
//! # No colour literals live in this crate
//!
//! Every colour here is an `Option`, and `None` means **"the theme decides"** —
//! `vellum-render` substitutes the token. `docs/05-design-language.md` §1 is
//! explicit that the accents and neutrals differ between light and dark ("every
//! colour must resolve through a token and no widget may contain a hex literal"),
//! and a table that baked `#E3E6E8` into its default grid line would be wrong in
//! dark mode and unfixable without a data migration. A `Some` colour is therefore
//! always a deliberate choice by the user — or one carried in from a Miro import —
//! and never a default.
//!
//! Widths and paddings *are* numbers here, because they do not change with the
//! theme. They follow the design language directly: a 1px hairline border, padding
//! on the 4px grid.
//!
//! # The cascade
//!
//! A cell's resolved style is assembled in six steps, later winning over earlier:
//!
//! 1. [`TableStyle::cell`] — the fully-specified base every cell starts from.
//! 2. [`TableStyle::header_column`], if the cell is in a header column.
//! 3. [`TableStyle::header_row`], if the cell is in a header row.
//! 4. the column's own [`CellStyle`].
//! 5. the row's own [`CellStyle`].
//! 6. the cell's own [`CellStyle`].
//!
//! Two orderings in there are decisions rather than accidents. **Header row beats
//! header column** at the corner cell, matching Word and Sheets: a table with both
//! reads down its first column as labels, so the corner belongs to the row. **Row
//! beats column**, because striping rows is the common gesture and a user who has
//! just clicked a row expects the row to win.
//!
//! Header styling defaults to **weight, not fill**. §5 of the design language:
//! hierarchy comes from type, spacing and hairlines, not from slabs of contrasting
//! grey. A bold header row needs no colour at all and therefore no theme.

use serde::{Deserialize, Serialize};

use crate::geometry::Insets;
use crate::span::Rgb;

/// Miro's default line height, `lh: 1.36` in the compact style keys — the same
/// constant `vellum_text::DEFAULT_LINE_HEIGHT` carries, so imported text lays out
/// identically inside a cell and outside one.
pub const DEFAULT_LINE_HEIGHT: f64 = 1.36;

/// Default cell text size. 13px is the design language's body and control size;
/// tables are dense, and a table is a control as much as it is content.
pub const DEFAULT_FONT_SIZE: f64 = 13.0;

/// A straight sRGB colour with alpha, for the fills a user or an import chooses
/// explicitly.
///
/// Alpha exists here and not on [`Rgb`] because a cell *fill* genuinely can be
/// translucent — a highlighted row over a board is a real thing — whereas a glyph
/// run cannot be, per the note on [`Rgb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 0xFF)
    }

    /// True when the colour would paint nothing, so layout can drop the fill rather
    /// than hand the renderer a fully transparent quad to blend.
    pub fn is_invisible(self) -> bool {
        self.a == 0
    }
}

/// How a border line is dashed. Mirrors `vellum_connect::LineStyle`, so a table
/// border and a connector drawn beside it can share one stroke pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BorderDash {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

/// One edge's stroke.
///
/// `color: None` means the theme's `frost` divider token, which is what almost
/// every border in a Vellum table is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BorderSide {
    pub width: f64,
    pub color: Option<Rgba>,
    pub dash: BorderDash,
}

impl BorderSide {
    /// The design language's default: a 1px line in `frost`, doing the work that a
    /// shadow would do in a less disciplined UI.
    pub const HAIRLINE: Self = Self { width: 1.0, color: None, dash: BorderDash::Solid };

    pub const fn new(width: f64, color: Option<Rgba>) -> Self {
        Self { width, color, dash: BorderDash::Solid }
    }

    /// A zero or negative width draws nothing. Treating that as "absent" rather
    /// than rejecting it lets a user turn one edge off by dragging its width to
    /// zero without the model needing a separate "hidden" flag.
    pub fn is_visible(self) -> bool {
        self.width > 0.0 && self.color.is_none_or(|c| !c.is_invisible())
    }

    /// Which of two borders is drawn where two cells share an edge.
    ///
    /// **The thicker wins; a tie goes to `a`.** Callers pass the top or left cell's
    /// border as `a`, so the rule reads "the earlier cell wins ties". It has to be a
    /// total, deterministic rule of exactly this shape: an interior edge is claimed
    /// by both of its cells, and drawing both would put two 1px hairlines on the
    /// same coordinate — which at 50% coverage each reads as a smudged 2px line, the
    /// precise artefact the design language's hairline separation cannot afford.
    pub fn resolve_shared(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(a), Some(b)) if b.width > a.width => Some(b),
            (Some(a), _) => Some(a),
            (None, b) => b,
        }
    }
}

/// Which sides of a cell carry an explicit border. `None` on a side means "inherit
/// the table's grid or outer line", resolved during layout because which of those
/// two applies depends on where the cell sits.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Borders {
    pub top: Option<BorderSide>,
    pub right: Option<BorderSide>,
    pub bottom: Option<BorderSide>,
    pub left: Option<BorderSide>,
}

impl Borders {
    /// No side stated — every one of them inherits.
    pub const NONE: Self = Self { top: None, right: None, bottom: None, left: None };

    pub fn uniform(side: BorderSide) -> Self {
        Self { top: Some(side), right: Some(side), bottom: Some(side), left: Some(side) }
    }

    /// Overlays `other`'s explicit sides onto `self`. Used by the cascade, where a
    /// row that sets only a bottom rule must not clear the cell's left border.
    pub fn overlay(self, other: Self) -> Self {
        Self {
            top: other.top.or(self.top),
            right: other.right.or(self.right),
            bottom: other.bottom.or(self.bottom),
            left: other.left.or(self.left),
        }
    }
}

/// Horizontal alignment. Mirrors `vellum_text::TextAlign` and Miro's `ta` key
/// exactly, including the American spelling, so the value crosses the boundary as a
/// one-arm `match` rather than a lookup table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Vertical alignment of the text block within its cell.
///
/// This has no counterpart in `vellum-text`: a text block does not know how tall
/// its container is, so somebody above it has to place it. In a table that is this
/// crate, and [`CellLayout::text_rect`](crate::CellLayout::text_rect) is the answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VerticalAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

/// Cell-level text properties — everything that is not per-span.
///
/// These are exactly `vellum_text::LayoutParams` minus its `max_width`, which
/// layout supplies from the resolved cell width. The mapping is field for field:
/// `font_family` → `font_family`, `font_size` → `font_size`, `line_height` →
/// `line_height`, `align` → `align`. Sizes are `f64` here to match world space and
/// narrow to `f32` at the boundary; the values in play are small integers, so the
/// cast is exact.
///
/// [`bold`](Self::bold) and [`italic`](Self::italic) have no `LayoutParams`
/// counterpart — they are folded into the spans by
/// [`StyledText::with_cell_defaults`](crate::StyledText::with_cell_defaults) on the
/// way into layout. See that method for why a header's weight lives here rather
/// than in the stored text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextStyle {
    /// Family name, e.g. Miro's `"Noto Sans"`. `None` uses the canvas default.
    pub font_family: Option<String>,
    pub font_size: f64,
    /// A *multiple* of the font size, matching Miro's `lh`.
    pub line_height: f64,
    pub align: TextAlign,
    /// Applied to every span that is not already bold.
    pub bold: bool,
    /// Applied to every span that is not already italic.
    pub italic: bool,
    /// Applied to every span that has no colour of its own. `None` is the theme's
    /// `ink` token.
    pub color: Option<Rgb>,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: DEFAULT_FONT_SIZE,
            line_height: DEFAULT_LINE_HEIGHT,
            align: TextAlign::Left,
            bold: false,
            italic: false,
            color: None,
        }
    }
}

impl TextStyle {
    /// Distance between consecutive baselines, in world px.
    pub fn line_advance(&self) -> f64 {
        let size = if self.font_size.is_finite() && self.font_size > 0.0 {
            self.font_size
        } else {
            DEFAULT_FONT_SIZE
        };
        let factor = if self.line_height.is_finite() && self.line_height > 0.0 {
            self.line_height
        } else {
            DEFAULT_LINE_HEIGHT
        };
        size * factor
    }

    /// A hash of everything that can change how this style *measures*.
    ///
    /// The measurement cache is keyed on cell identity and revision, which catches
    /// edits to the text. It cannot catch a change one level up — marking a row as a
    /// header bolds every cell in it without touching a single cell — so the key
    /// carries this fingerprint too. Without it, toggling the header row would leave
    /// the table laid out at the old widths and look like a stale frame.
    ///
    /// Alignment is deliberately excluded: it moves the text inside a box whose size
    /// it does not change, so including it would evict a cache that is still valid.
    pub fn measurement_fingerprint(&self) -> u64 {
        // FNV-1a. Chosen over `DefaultHasher` because that is explicitly not stable
        // across releases, and a fingerprint that changes under the caller's feet
        // would silently disable the cache rather than break a test.
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = OFFSET;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(PRIME);
            }
        };
        match &self.font_family {
            Some(name) => {
                eat(&[1]);
                eat(name.as_bytes());
            }
            None => eat(&[0]),
        }
        eat(&self.font_size.to_bits().to_le_bytes());
        eat(&self.line_height.to_bits().to_le_bytes());
        eat(&[u8::from(self.bold), u8::from(self.italic)]);
        h
    }
}

/// A cell's style *overrides*. Every field is optional; `None` inherits from the
/// level above in the cascade.
///
/// Note that clearing an override restores the inherited value — there is no way to
/// say "explicitly no font family". That asymmetry is deliberate: the alternative is
/// `Option<Option<String>>` on four fields, and the case it buys is a user who wants
/// one cell to opt out of the table's family and back to the canvas default, which
/// no table UI offers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CellStyle {
    pub fill: Option<Rgba>,
    pub padding: Option<Insets>,
    pub align: Option<TextAlign>,
    pub vertical_align: Option<VerticalAlign>,
    pub font_family: Option<String>,
    pub font_size: Option<f64>,
    pub line_height: Option<f64>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub text_color: Option<Rgb>,
    pub borders: Borders,
}

impl CellStyle {
    /// Overrides nothing. Equal to [`Default`], but usable in a `static`, which is
    /// what lets [`Cell::style`](crate::Cell::style) hand out a reference for a cell
    /// that carries no override at all — see the note on `Cell`'s size.
    pub const EMPTY: Self = Self {
        fill: None,
        padding: None,
        align: None,
        vertical_align: None,
        font_family: None,
        font_size: None,
        line_height: None,
        bold: None,
        italic: None,
        text_color: None,
        borders: Borders::NONE,
    };

    /// True when this level of the cascade contributes nothing, which lets the
    /// resolver skip it entirely.
    pub fn is_empty(&self) -> bool {
        self == &Self::EMPTY
    }

    /// The default styling for a header row or column: bold, nothing else.
    pub fn header() -> Self {
        Self { bold: Some(true), ..Self::default() }
    }

    /// Applies this level's overrides on top of an already-resolved style.
    pub fn apply_to(&self, base: &mut ResolvedCellStyle) {
        if self.fill.is_some() {
            base.fill = self.fill;
        }
        if let Some(padding) = self.padding {
            base.padding = padding;
        }
        if let Some(align) = self.align {
            base.text.align = align;
        }
        if let Some(valign) = self.vertical_align {
            base.vertical_align = valign;
        }
        if self.font_family.is_some() {
            base.text.font_family = self.font_family.clone();
        }
        if let Some(size) = self.font_size {
            base.text.font_size = size;
        }
        if let Some(lh) = self.line_height {
            base.text.line_height = lh;
        }
        if let Some(bold) = self.bold {
            base.text.bold = bold;
        }
        if let Some(italic) = self.italic {
            base.text.italic = italic;
        }
        if self.text_color.is_some() {
            base.text.color = self.text_color;
        }
        base.borders = base.borders.overlay(self.borders);
    }
}

/// A cell's style with every question answered — what layout and the renderer
/// consume.
///
/// [`borders`](Self::borders) is the one field that is still optional after
/// resolution, and for a positional reason: whether an unset side falls back to
/// [`TableStyle::grid_border`] or [`TableStyle::outer_border`] depends on where the
/// cell is, which the cascade does not know and layout does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedCellStyle {
    pub fill: Option<Rgba>,
    pub padding: Insets,
    pub text: TextStyle,
    pub vertical_align: VerticalAlign,
    pub borders: Borders,
}

impl Default for ResolvedCellStyle {
    fn default() -> Self {
        Self {
            fill: None,
            padding: Insets::default(),
            text: TextStyle::default(),
            vertical_align: VerticalAlign::Top,
            borders: Borders::default(),
        }
    }
}

/// Everything about a table's appearance that is not per-cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableStyle {
    /// Painted behind every cell, before the per-cell fills. `None` leaves the
    /// canvas showing through, which is what an unfilled table on a board wants.
    pub fill: Option<Rgba>,
    /// The base every cell's cascade starts from.
    pub cell: ResolvedCellStyle,
    /// Overlaid on cells in the first [`header_rows`](crate::Table::header_rows).
    pub header_row: CellStyle,
    /// Overlaid on cells in the first [`header_columns`](crate::Table::header_columns).
    pub header_column: CellStyle,
    /// The table's perimeter, where a cell states nothing.
    pub outer_border: Option<BorderSide>,
    /// Every interior line, where neither adjacent cell states anything.
    pub grid_border: Option<BorderSide>,
}

impl Default for TableStyle {
    fn default() -> Self {
        Self {
            fill: None,
            cell: ResolvedCellStyle::default(),
            header_row: CellStyle::header(),
            header_column: CellStyle::header(),
            outer_border: Some(BorderSide::HAIRLINE),
            grid_border: Some(BorderSide::HAIRLINE),
        }
    }
}

/// Where a cell sits relative to the table's headers. Carried on
/// [`CellLayout`](crate::CellLayout) so the renderer can key on it without
/// re-deriving it from two counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CellKind {
    Body,
    HeaderRow,
    HeaderColumn,
    /// Both — the top-left cell of a table with a header row *and* a header column.
    HeaderCorner,
}

impl CellKind {
    pub(crate) fn of(in_header_row: bool, in_header_column: bool) -> Self {
        match (in_header_row, in_header_column) {
            (true, true) => Self::HeaderCorner,
            (true, false) => Self::HeaderRow,
            (false, true) => Self::HeaderColumn,
            (false, false) => Self::Body,
        }
    }

    pub fn is_header(self) -> bool {
        self != Self::Body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both cells claim the interior edge between them. Drawing both would put two
    /// hairlines on one coordinate; the resolution has to be total and stable.
    #[test]
    fn a_shared_edge_takes_the_thicker_border_and_ties_go_to_the_earlier_cell() {
        let thin = BorderSide::new(1.0, None);
        let thick = BorderSide::new(3.0, None);
        let red = BorderSide::new(1.0, Some(Rgba::opaque(0xE6, 0x5B, 0x58)));

        assert_eq!(BorderSide::resolve_shared(Some(thin), Some(thick)), Some(thick));
        assert_eq!(BorderSide::resolve_shared(Some(thick), Some(thin)), Some(thick));
        assert_eq!(BorderSide::resolve_shared(Some(thin), Some(red)), Some(thin), "tie: a wins");
        assert_eq!(BorderSide::resolve_shared(None, Some(red)), Some(red));
        assert_eq!(BorderSide::resolve_shared(None, None), None);
    }

    #[test]
    fn a_zero_width_or_transparent_border_draws_nothing() {
        assert!(BorderSide::HAIRLINE.is_visible());
        assert!(!BorderSide::new(0.0, None).is_visible());
        assert!(!BorderSide::new(2.0, Some(Rgba::new(0, 0, 0, 0))).is_visible());
    }

    /// A row that sets only a bottom rule must not clear the cell's own left border.
    #[test]
    fn overlaying_borders_keeps_the_sides_the_upper_level_says_nothing_about() {
        let base = Borders { left: Some(BorderSide::HAIRLINE), ..Borders::default() };
        let rule = BorderSide::new(2.0, None);
        let merged = base.overlay(Borders { bottom: Some(rule), ..Borders::default() });
        assert_eq!(merged.left, Some(BorderSide::HAIRLINE));
        assert_eq!(merged.bottom, Some(rule));
        assert_eq!(merged.right, None);
    }

    #[test]
    fn applying_a_level_touches_only_the_fields_it_states() {
        let mut resolved = ResolvedCellStyle::default();
        resolved.text.font_size = 20.0;
        CellStyle { bold: Some(true), ..CellStyle::default() }.apply_to(&mut resolved);
        assert!(resolved.text.bold);
        assert_eq!(resolved.text.font_size, 20.0);
        assert_eq!(resolved.padding, Insets::default());
    }

    /// The fingerprint is the cache's only defence against a change one level up.
    #[test]
    fn the_fingerprint_moves_with_weight_and_size_but_not_with_alignment() {
        let plain = TextStyle::default();
        let bold = TextStyle { bold: true, ..TextStyle::default() };
        let big = TextStyle { font_size: 24.0, ..TextStyle::default() };
        let centred = TextStyle { align: TextAlign::Center, ..TextStyle::default() };

        assert_ne!(plain.measurement_fingerprint(), bold.measurement_fingerprint());
        assert_ne!(plain.measurement_fingerprint(), big.measurement_fingerprint());
        assert_eq!(
            plain.measurement_fingerprint(),
            centred.measurement_fingerprint(),
            "alignment moves text inside a box whose size it does not change"
        );
    }

    #[test]
    fn a_degenerate_font_size_still_advances_the_line() {
        let broken = TextStyle { font_size: 0.0, line_height: f64::NAN, ..TextStyle::default() };
        assert_eq!(broken.line_advance(), DEFAULT_FONT_SIZE * DEFAULT_LINE_HEIGHT);
    }

    #[test]
    fn header_defaults_are_weight_not_colour() {
        let style = TableStyle::default();
        assert_eq!(style.header_row.bold, Some(true));
        assert_eq!(style.header_row.fill, None, "design language §5: type carries the hierarchy");
    }
}
