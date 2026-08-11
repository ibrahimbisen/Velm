//! The colour picker, and the conversions between the document's colour and egui's.
//!
//! egui has a colour picker, and it is not the one a board app wants: it is an
//! sRGB/linear debugging tool with an additive-blending preview. This one is the
//! picker Miro users expect — a row of board swatches first, because 95% of picks
//! are one of them, with an HSV square and a hex field underneath for the rest.
//!
//! The hex field is the interesting part of the state machine. It has to accept
//! partial input while the user types without snapping the colour to whatever
//! `#ff` happens to parse as, so the buffer is kept as text and only committed when
//! it parses completely. [`parse_hex`] is where that decision lives, and it is
//! tested on its own.

use crate::theme::{Palette, radius, space};
use crate::widgets::swatch;
use egui::{Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, vec2};
use vellum_doc::Color;

/// Document colour to egui colour.
///
/// `Color32` stores **premultiplied** sRGBA while `vellum_doc::Color` stores straight
/// alpha, so this is a real conversion and not a re-spelling. Getting the direction
/// wrong is invisible on opaque colours and darkens every translucent one, which is
/// exactly the class of bug that survives a review.
pub fn to_egui(color: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
}

/// egui colour to document colour.
///
/// Exact for opaque colours. Premultiplication quantises to bytes, so a translucent
/// colour can come back a step or two off — unavoidable, and the reason the picker
/// keeps its value in [`Hsv`] and never round-trips through `Color32`.
pub fn from_egui(color: Color32) -> Color {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    Color::rgba(r, g, b, a)
}

/// The board palette — the colours a user paints *with*, as distinct from the ones
/// the chrome is drawn in.
///
/// Three rows, each with a different job:
///
/// 1. **Miro's sticky-note pack, verbatim.** `#fff79e` is the yellow 43 of the
///    reference board's 44 stickies use, so a user recolouring an imported note
///    finds the original where they expect it. These are *not* ours to restyle: the
///    importer produces them, and a swatch that is a shade off is a swatch that
///    silently fails to match the note beside it.
/// 2. **The system's own neutral ramp**, so a grey drawn on the board is one of the
///    greys the interface is made of rather than a fourth family of neutrals. A test
///    below holds it to the palette.
///
///    It used to carry `#0B0D10` — the *dark* cut's well — at the bottom, 15/255 from
///    `ink` above it, which in an app that only ships light was a cell nobody could
///    tell from its neighbour. Whitening the ramp would have added a second such pair
///    at the top, since `bone` is now 3/255 from white. Both slots went to one honest
///    mid-grey instead, which is what the ramp was actually missing: it jumped 90
///    levels from `frost` to `ink-faint` and had no middle at all.
/// 3. **Saturated hues for diagrams**, led by `signal-teal` and `monitor-cyan` — the
///    two accents the interface reserves — plus `xr-red`, which the chrome still uses
///    for destruction. So the board and the chrome agree on what a colour means. No
///    violet: `docs/05-design-language.md` §2 rules purple out, and the one place it
///    survives is row 1, where it is Miro's colour and not a choice.
pub const SWATCHES: &[&[Color]] = &[
    &[
        Color::rgb(0xFF, 0xF7, 0x9E),
        Color::rgb(0xFF, 0xCE, 0x8A),
        Color::rgb(0xFF, 0x9E, 0x9E),
        Color::rgb(0xF5, 0xA3, 0xD8),
        Color::rgb(0xD1, 0xA8, 0xF5),
        Color::rgb(0xA6, 0xC6, 0xFF),
        Color::rgb(0x9E, 0xE5, 0xF5),
        Color::rgb(0xA8, 0xE6, 0xB8),
    ],
    &[
        Color::rgb(0xFF, 0xFF, 0xFF),
        Color::rgb(0xFC, 0xFD, 0xFE), // bone
        // `paper` — the board's own colour, so an item painted "the same as the background"
        // really is. It replaced `pearl` in this row when the canvas was whitened and the two
        // tokens split; `pearl` is now a chrome-only surface and has no business on a board
        // swatch grid, which is why this is a substitution rather than a ninth cell.
        //
        // Neutral, unlike every other neutral here, because it is Miro's measured board grey.
        Color::rgb(0xF2, 0xF2, 0xF2), // paper
        Color::rgb(0xE5, 0xEA, 0xED), // frost
        Color::rgb(0xB4, 0xBD, 0xC2),
        Color::rgb(0x8B, 0x95, 0x9B), // ink-faint
        Color::rgb(0x5C, 0x65, 0x6B), // ink-muted
        Color::rgb(0x1A, 0x1D, 0x1F), // ink
    ],
    &[
        Color::rgb(0xE6, 0x5B, 0x58), // xr-red, which is `danger` now
        Color::rgb(0xE8, 0x7B, 0x1F),
        Color::rgb(0xE3, 0xB5, 0x0B),
        Color::rgb(0x1F, 0x7A, 0x55),
        Color::rgb(0x00, 0xA3, 0x8C), // signal-teal, the primary accent
        Color::rgb(0x6F, 0xD6, 0xE6), // monitor-cyan
        Color::rgb(0x37, 0x6F, 0xF5),
        Color::rgb(0xC2, 0x3A, 0x8E),
    ],
];

/// The board's default ink — the same `#1A1D1F` the interface writes in.
///
/// A document colour rather than a [`Color32`], because it is applied to items rather
/// than drawn: the pen lays it down, the properties panel seeds the text picker with
/// it, and it is stored in the board. It is spelled here rather than derived from the
/// palette because [`SWATCHES`] is a `const` and the conversion is not.
/// `tests/tokens.rs` holds it to the palette.
pub const INK: Color = Color::rgb(0x1A, 0x1D, 0x1F);

/// The board's default fill — Miro's canonical sticky yellow, so an imported board's
/// notes recolour back to where they started.
pub const DEFAULT_FILL: Color = Color::rgb(0xFF, 0xF7, 0x9E);

/// Hue, saturation and value, each 0.0–1.0, plus straight alpha.
///
/// Kept alongside the picker rather than derived from the colour every frame,
/// because HSV→RGB is lossy in one direction: dragging value to zero and back up
/// would lose the hue if the hue were re-derived from the black it produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
    pub a: f32,
}

impl Hsv {
    pub fn from_color(color: Color) -> Self {
        let r = f32::from(color.r) / 255.0;
        let g = f32::from(color.g) / 255.0;
        let b = f32::from(color.b) / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let chroma = max - min;

        let h = if chroma <= f32::EPSILON {
            0.0
        } else if max == r {
            ((g - b) / chroma).rem_euclid(6.0)
        } else if max == g {
            (b - r) / chroma + 2.0
        } else {
            (r - g) / chroma + 4.0
        } / 6.0;

        Self {
            h,
            s: if max <= f32::EPSILON { 0.0 } else { chroma / max },
            v: max,
            a: f32::from(color.a) / 255.0,
        }
    }

    pub fn to_color(self) -> Color {
        let (r, g, b) = hsv_to_rgb(self.h, self.s, self.v);
        Color::rgba(
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
            (self.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(1.0) * 6.0;
    let (s, v) = (s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`, with or without the hash.
///
/// Returns `None` for anything else — including the half-typed `#ff3` that is also
/// a valid three-digit colour, which is why the picker commits on a complete parse
/// rather than on every keystroke: `#ff3` typed on the way to `#ff3b3b` would
/// otherwise flash a different colour onto the selection.
pub fn parse_hex(text: &str) -> Option<Color> {
    let digits = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    let nibble = |i: usize| {
        u8::from_str_radix(&digits[i..i + 1], 16).ok().map(|v| v * 0x11)
    };
    match digits.len() {
        3 => Some(Color::rgb(nibble(0)?, nibble(1)?, nibble(2)?)),
        6 => Some(Color::rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color::rgba(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

/// The picker's own state, held by the chrome between frames.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorPicker {
    hsv: Hsv,
    /// The hex field's live text, which may not parse yet.
    hex: String,
    /// Colours picked before, newest first, capped at [`RECENT`].
    ///
    /// *"there should be most recently used colors added at the bottom of that list."* A board
    /// is painted in a handful of colours the user keeps returning to, and none of them is
    /// necessarily in the fixed palette — a colour mixed once in the HSV square was previously
    /// unreachable a second time except by mixing it again or by typing its hex from memory.
    ///
    /// On the picker rather than in a global: the fill, the border and the text each open their
    /// own, so each remembers the colours *it* has been given. A shared list would offer the
    /// last stroke colour as a fill, which is a different decision the user did not make.
    recent: Vec<Color>,
    /// Whether alpha is offered. A sticky's background has no alpha in Miro; a
    /// shape's fill does.
    pub alpha: bool,
}

impl ColorPicker {
    pub fn new(color: Color, alpha: bool) -> Self {
        Self { hsv: Hsv::from_color(color), hex: color.to_hex(), alpha, recent: Vec::new() }
    }

    pub fn color(&self) -> Color {
        self.hsv.to_color()
    }

    /// Re-seeds the picker when it is reopened on a different value.
    pub fn set_color(&mut self, color: Color) {
        self.hsv = Hsv::from_color(color);
        self.hex = color.to_hex();
    }

    /// Draws the picker. Returns the colour when it changed this frame.
    pub fn ui(&mut self, ui: &mut Ui, palette: Palette) -> Option<Color> {
        let mut changed = false;

        ui.spacing_mut().item_spacing = vec2(space::UNIT, space::UNIT);
        for row in SWATCHES {
            ui.horizontal(|ui| {
                for color in *row {
                    let selected = self.color() == *color;
                    if swatch(
                        ui,
                        palette,
                        Some(to_egui(*color)),
                        Vec2::splat(space::of(5)),
                        selected,
                    )
                    .on_hover_text(crate::theme::numeric(color.to_hex()))
                    .clicked()
                    {
                        self.set_color(*color);
                        changed = true;
                    }
                }
            });
        }

        // **Below the fixed palette, because it is the newer half of the same question.**
        // Miro puts recents under its grid and so does every paint program; putting them on top
        // would move the swatch a user is aiming at every time they pick a different colour.
        if !self.recent.is_empty() {
            crate::widgets::hairline(ui, palette);
            ui.horizontal_wrapped(|ui| {
                for color in self.recent.clone() {
                    let selected = self.color() == color;
                    if swatch(ui, palette, Some(to_egui(color)), Vec2::splat(space::of(5)), selected)
                        .on_hover_text(crate::theme::numeric(color.to_hex()))
                        .clicked()
                    {
                        self.set_color(color);
                        changed = true;
                    }
                }
            });
        }

        ui.add_space(space::UNIT);
        changed |= self.saturation_value_square(ui, palette);
        changed |= self.hue_strip(ui, palette);
        if self.alpha {
            changed |= self.alpha_strip(ui, palette);
        }

        ui.add_space(space::UNIT);
        ui.horizontal(|ui| {
            let _ = swatch(ui, palette, Some(to_egui(self.color())), Vec2::splat(space::of(6)), false);
            // The hex field is mono for the same reason a coordinate is: six digits
            // that reflow as they are typed make the field feel like it is fighting
            // back. `docs/05` §3 names hex values explicitly.
            let response = crate::theme::tabular(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.hex)
                        .desired_width(space::of(24))
                        .char_limit(9)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("#rrggbb"),
                )
            });
            if response.changed()
                && let Some(color) = parse_hex(&self.hex)
            {
                let alpha = if self.alpha { color.a } else { 0xFF };
                self.hsv = Hsv::from_color(Color { a: alpha, ..color });
                changed = true;
            }
            // Leaving the field with unparseable text restores the real value rather
            // than stranding the user with a red-looking field and no way back.
            if response.lost_focus() && parse_hex(&self.hex).is_none() {
                self.hex = self.color().to_hex();
            }
        });

        if changed {
            self.hex = self.color().to_hex();
            self.remember(self.color());
            Some(self.color())
        } else {
            None
        }
    }

    /// Files a colour at the head of the recent list.
    ///
    /// Called on **every** change, dragging included, and that is deliberate rather than
    /// sloppy: the alternative is a "commit" the picker has no way to detect — it is a live
    /// control with no OK button, and a drag through the HSV square is a hundred changes of
    /// which only the last is a decision. Deduplicating on the way in and capping the list
    /// means a drag leaves exactly one entry: its final colour, at the head.
    fn remember(&mut self, color: Color) {
        /// How many to keep. Two rows of the grid's eight — enough to hold a session's
        /// working set, short enough that the row stays scannable and the popover does not
        /// grow a scrollbar.
        const RECENT: usize = 16;

        // A colour already in the fixed palette is always one click away up there, so
        // spending a recent slot on it would push out the mixed colours this list exists for.
        if SWATCHES.iter().any(|row| row.contains(&color)) {
            return;
        }
        self.recent.retain(|held| *held != color);
        self.recent.insert(0, color);
        self.recent.truncate(RECENT);
    }

    fn saturation_value_square(&mut self, ui: &mut Ui, palette: Palette) -> bool {
        let size = vec2(ui.available_width().min(space::of(54)), space::of(32));
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());

        if ui.is_rect_visible(rect) {
            // A coarse mesh rather than a texture: 12×8 quads with per-vertex colour
            // is visually indistinguishable from a per-pixel gradient at this size,
            // and costs no texture upload per frame.
            let (cols, rows) = (12, 8);
            let mut mesh = egui::Mesh::default();
            for row in 0..=rows {
                for col in 0..=cols {
                    let s = col as f32 / cols as f32;
                    let v = 1.0 - row as f32 / rows as f32;
                    let (r, g, b) = hsv_to_rgb(self.hsv.h, s, v);
                    mesh.colored_vertex(
                        egui::pos2(
                            rect.left() + s * rect.width(),
                            rect.top() + (1.0 - v) * rect.height(),
                        ),
                        Color32::from_rgb(
                            (r * 255.0) as u8,
                            (g * 255.0) as u8,
                            (b * 255.0) as u8,
                        ),
                    );
                }
            }
            let stride = cols + 1;
            for row in 0..rows {
                for col in 0..cols {
                    let i = (row * stride + col) as u32;
                    mesh.add_triangle(i, i + 1, i + stride as u32);
                    mesh.add_triangle(i + 1, i + stride as u32 + 1, i + stride as u32);
                }
            }
            ui.painter().add(egui::Shape::mesh(mesh));
            ui.painter().rect_stroke(
                rect,
                CornerRadius::same(radius::SMALL),
                palette.hairline_stroke(),
                StrokeKind::Inside,
            );

            let centre = egui::pos2(
                rect.left() + self.hsv.s * rect.width(),
                rect.top() + (1.0 - self.hsv.v) * rect.height(),
            );
            // Two rings, light over dark, so the handle stays visible on any colour —
            // which is why `handle` is near-white in *both* palettes rather than
            // following the ink: the square underneath it is not a surface, it is
            // every hue at once.
            let radius = space::of(2) - 2.0;
            ui.painter().circle_stroke(centre, radius, Stroke::new(2.0, palette.handle));
            ui.painter().circle_stroke(centre, radius, Stroke::new(1.0, palette.handle_edge));
        }

        if response.is_pointer_button_down_on()
            && let Some(pos) = response.interact_pointer_pos()
        {
            self.hsv.s = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            self.hsv.v = 1.0 - ((pos.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
            return true;
        }
        false
    }

    fn hue_strip(&mut self, ui: &mut Ui, palette: Palette) -> bool {
        let (rect, response) = strip(ui);
        if ui.is_rect_visible(rect) {
            let steps = 12;
            let mut mesh = egui::Mesh::default();
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                let (r, g, b) = hsv_to_rgb(t, 1.0, 1.0);
                let color = Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
                let x = rect.left() + t * rect.width();
                mesh.colored_vertex(egui::pos2(x, rect.top()), color);
                mesh.colored_vertex(egui::pos2(x, rect.bottom()), color);
            }
            for i in 0..steps {
                let v = (i * 2) as u32;
                mesh.add_triangle(v, v + 1, v + 2);
                mesh.add_triangle(v + 1, v + 3, v + 2);
            }
            ui.painter().add(egui::Shape::mesh(mesh));
            strip_handle(ui, palette, rect, self.hsv.h);
        }
        strip_drag(&response, rect).inspect(|h| self.hsv.h = *h).is_some()
    }

    fn alpha_strip(&mut self, ui: &mut Ui, palette: Palette) -> bool {
        let (rect, response) = strip(ui);
        if ui.is_rect_visible(rect) {
            let opaque = to_egui(Color { a: 0xFF, ..self.color() });
            // The clear end of the ramp is *this* colour at zero alpha rather than a
            // named transparent: premultiplied, they are the same four bytes, and
            // deriving it keeps the crate's rule — no widget names a colour — intact.
            let clear = to_egui(Color { a: 0x00, ..self.color() });
            let mut mesh = egui::Mesh::default();
            mesh.colored_vertex(rect.left_top(), clear);
            mesh.colored_vertex(rect.left_bottom(), clear);
            mesh.colored_vertex(rect.right_top(), opaque);
            mesh.colored_vertex(rect.right_bottom(), opaque);
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(1, 3, 2);
            ui.painter().rect_filled(rect, CornerRadius::same(radius::SMALL), palette.well);
            ui.painter().add(egui::Shape::mesh(mesh));
            strip_handle(ui, palette, rect, self.hsv.a);
        }
        strip_drag(&response, rect).inspect(|a| self.hsv.a = *a).is_some()
    }
}

fn strip(ui: &mut Ui) -> (Rect, Response) {
    ui.allocate_exact_size(
        vec2(ui.available_width().min(space::of(54)), space::of(4)),
        Sense::click_and_drag(),
    )
}

fn strip_handle(ui: &Ui, palette: Palette, rect: Rect, t: f32) {
    let x = rect.left() + t.clamp(0.0, 1.0) * rect.width();
    let handle = Rect::from_center_size(
        egui::pos2(x, rect.center().y),
        vec2(5.0, rect.height() + space::UNIT),
    );
    ui.painter().rect_filled(handle, CornerRadius::same(radius::TIGHT), palette.handle);
    ui.painter().rect_stroke(
        handle,
        CornerRadius::same(radius::TIGHT),
        Stroke::new(1.0, palette.handle_edge),
        StrokeKind::Inside,
    );
}

fn strip_drag(response: &Response, rect: Rect) -> Option<f32> {
    if !response.is_pointer_button_down_on() {
        return None;
    }
    let pos = response.interact_pointer_pos()?;
    Some(((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_accepts_the_three_lengths_a_user_might_type() {
        assert_eq!(parse_hex("#fff79e"), Some(Color::rgb(0xFF, 0xF7, 0x9E)));
        assert_eq!(parse_hex("fff79e"), Some(Color::rgb(0xFF, 0xF7, 0x9E)));
        assert_eq!(parse_hex("  #FFF79E  "), Some(Color::rgb(0xFF, 0xF7, 0x9E)));
        assert_eq!(parse_hex("#f9e"), Some(Color::rgb(0xFF, 0x99, 0xEE)));
        assert_eq!(parse_hex("#fff79e80"), Some(Color::rgba(0xFF, 0xF7, 0x9E, 0x80)));
    }

    /// The reason the hex field commits on a complete parse only.
    #[test]
    fn a_half_typed_hex_string_is_rejected_rather_than_guessed_at() {
        for text in ["#ff", "#ffff", "#fffffff", "", "#", "#gggggg", "#ff f79e"] {
            assert_eq!(parse_hex(text), None, "{text:?} should not parse");
        }
    }

    #[test]
    fn hex_round_trips_through_the_document_colour() {
        for color in SWATCHES.iter().flat_map(|row| row.iter()) {
            assert_eq!(parse_hex(&color.to_hex()), Some(*color), "{color:?}");
        }
    }

    #[test]
    fn hsv_round_trips_every_swatch_exactly() {
        for color in SWATCHES.iter().flat_map(|row| row.iter()) {
            assert_eq!(Hsv::from_color(*color).to_color(), *color, "{color:?}");
        }
    }

    /// Greys have no hue to recover; the conversion must still survive them rather
    /// than dividing by a zero chroma.
    #[test]
    fn achromatic_colours_convert_without_a_hue() {
        for grey in [Color::rgb(0, 0, 0), Color::rgb(128, 128, 128), Color::rgb(255, 255, 255)] {
            let hsv = Hsv::from_color(grey);
            assert_eq!(hsv.h, 0.0);
            assert_eq!(hsv.s, 0.0);
            assert_eq!(hsv.to_color(), grey);
        }
    }

    #[test]
    fn opaque_colours_survive_the_conversion_to_egui_exactly() {
        for color in SWATCHES.iter().flat_map(|row| row.iter()) {
            assert_eq!(from_egui(to_egui(*color)), *color, "{color:?}");
        }
    }

    /// Premultiplying into bytes loses precision; the contract is that it stays
    /// within a rounding step rather than that it is exact.
    #[test]
    fn translucent_colours_survive_the_conversion_to_within_a_rounding_step() {
        let color = Color::rgba(0x12, 0x34, 0x56, 0x78);
        let back = from_egui(to_egui(color));
        assert_eq!(back.a, color.a);
        for (a, b) in [(back.r, color.r), (back.g, color.g), (back.b, color.b)] {
            assert!(a.abs_diff(b) <= 2, "{a:#04x} vs {b:#04x}");
        }
    }

    /// Setting a colour with an unparseable hex buffer in flight must not corrupt
    /// the picker's value.
    #[test]
    fn reseeding_the_picker_replaces_both_the_value_and_the_hex_text() {
        let mut picker = ColorPicker::new(Color::rgb(0xFF, 0xF7, 0x9E), false);
        picker.hex = "#zz".to_owned();
        picker.set_color(Color::rgb(0x37, 0x6F, 0xF5));
        assert_eq!(picker.color(), Color::rgb(0x37, 0x6F, 0xF5));
        assert_eq!(picker.hex, "#376ff5");
    }

    #[test]
    fn the_picker_renders_and_reports_no_change_when_untouched() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut picker = ColorPicker::new(Color::rgb(0xFF, 0xF7, 0x9E), true);
        let mut result = Some(Color::rgb(0, 0, 0));
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            result = picker.ui(ui, Palette::LIGHT);
        });
        assert_eq!(result, None);
        assert_eq!(picker.color(), Color::rgb(0xFF, 0xF7, 0x9E));
    }

    /// Miro's canonical sticky yellow has to be the first thing a user reaches for
    /// after an import, or recolouring a note away and back loses the original.
    #[test]
    fn the_palette_leads_with_miros_sticky_yellow() {
        assert_eq!(SWATCHES[0][0], Color::rgb(0xFF, 0xF7, 0x9E));
        assert!(SWATCHES.iter().all(|row| row.len() == 8), "rows must align in the grid");
    }

    /// The board's neutrals are the interface's neutrals. Left to drift they become a
    /// fourth family of greys, and a rectangle drawn "grey" stops matching the panel
    /// it is being compared against.
    #[test]
    fn the_neutral_ramp_is_the_systems_own_ink() {
        let neutrals = SWATCHES[1];
        // The light cut's ramp, entire. `Palette::DARK.well` used to be asked for too
        // and no longer is: it is a dark-mode token in a light-only app, it sat 15/255
        // from `ink`, and holding a swatch grid to a palette nothing can select was
        // spending a cell to prevent a drift that cannot happen.
        for token in [
            Palette::LIGHT.surface,
            Palette::LIGHT.canvas,
            Palette::LIGHT.border,
            Palette::LIGHT.faint,
            Palette::LIGHT.muted,
            Palette::LIGHT.text,
        ] {
            assert!(
                neutrals.contains(&from_egui(token)),
                "{token:?} is a system neutral the board cannot paint with"
            );
        }
        // Ascending darkness, so the row reads as a ramp rather than as a set.
        let value = |c: &Color| u32::from(c.r) + u32::from(c.g) + u32::from(c.b);
        assert!(neutrals.windows(2).all(|w| value(&w[0]) > value(&w[1])), "the ramp is unsorted");
    }

    /// Both interface accents have to be reachable as paint, or the one red the app
    /// uses for selection cannot be drawn onto the board it is selecting things on.
    #[test]
    fn the_two_interface_accents_are_available_as_paint() {
        let hues = SWATCHES[2];
        assert!(hues.contains(&from_egui(Palette::LIGHT.accent)));
        assert!(hues.contains(&from_egui(Palette::LIGHT.info)));
        assert!(hues.contains(&from_egui(Palette::LIGHT.success)));
    }
}

#[cfg(test)]
mod recent_tests {
    use super::{Color, ColorPicker, SWATCHES};

    /// A mixed colour comes back; a palette colour does not take a slot.
    ///
    /// *"there should be most recently used colors added at the bottom of that list."* The
    /// list exists for colours that are **not** in the grid — one mixed in the HSV square was
    /// otherwise unreachable a second time except by mixing it again or typing its hex from
    /// memory. A palette colour filling a slot would push out the only entries that were hard
    /// to get back to.
    #[test]
    fn the_recent_row_holds_mixed_colours_and_not_palette_ones() {
        let mut picker = ColorPicker::new(Color::rgb(0, 0, 0), true);
        let mixed = Color::rgb(0xBD, 0x0A, 0x0A);
        picker.remember(mixed);
        assert_eq!(picker.recent, vec![mixed]);

        picker.remember(SWATCHES[0][0]);
        assert_eq!(picker.recent, vec![mixed], "a palette colour is already one click away");
    }

    /// Newest first, no duplicates, and a drag leaves one entry rather than a hundred.
    ///
    /// The picker is a live control with no OK button, so `remember` is called on every
    /// change — a drag through the HSV square reports a hundred colours of which only the
    /// last is a decision. De-duplication and the cap are what make that behave like a
    /// commit without the picker having to detect one.
    #[test]
    fn picking_the_same_colour_again_moves_it_to_the_front() {
        let mut picker = ColorPicker::new(Color::rgb(0, 0, 0), true);
        let (a, b) = (Color::rgb(1, 2, 3), Color::rgb(4, 5, 6));
        picker.remember(a);
        picker.remember(b);
        picker.remember(a);
        assert_eq!(picker.recent, vec![a, b], "newest first, and `a` moved rather than repeated");

        let mut dragging = ColorPicker::new(Color::rgb(0, 0, 0), true);
        for value in 0..40u8 {
            dragging.remember(Color::rgb(value, 0, 0));
        }
        assert_eq!(dragging.recent.len(), 16, "capped");
        assert_eq!(dragging.recent[0], Color::rgb(39, 0, 0), "the last one dragged to leads");
    }
}
