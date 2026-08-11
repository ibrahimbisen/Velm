//! Velm's mark — *Viewport*: four corner brackets around a frame that is never
//! drawn.
//!
//! Drawn from geometry rather than loaded from `assets/logo/mark.svg`, for the same
//! reason [`Icon`](crate::Icon) is: an SVG would need a rasteriser, a cache and a
//! size-dependent atlas entry to draw four straight lines. The asset stays the
//! specification and this stays its implementation, and they are held together by
//! tests that read the SVG: the ones below check the grid, the stroke and which
//! corner is accented, and one of them checks that each cut wears the accent its own
//! mode specifies — `signal-teal` in the shipped light cut, and the frozen `xr-red`
//! in the retained dark one.
//!
//! The mark's meaning comes out of the product rather than being applied to it: it is
//! the same shape as the canvas selection indicator, so the user sees it thousands of
//! times a day without it reading as branding. The single accented corner gives it a
//! reading direction instead of anonymous four-fold symmetry, which is why the
//! accented bracket is *always* the top-left one and never rotated.
//!
//! Clear space is one bracket arm on every side — [`CLEAR_SPACE`] — and the minimum
//! size is [`MINIMUM_SIZE`], below which the negative space closes and it stops
//! reading as four brackets.

use crate::theme::Palette;
use egui::{Color32, Painter, Pos2, Rect, Stroke, Vec2, pos2};

/// The design grid the mark is drawn on, matching `assets/logo/mark.svg`'s viewBox.
const GRID: f32 = 48.0;

/// Distance from the edge of the grid to a bracket, in grid units.
const INSET: f32 = 8.0;

/// Length of one bracket arm, in grid units. Also the clear space.
const ARM: f32 = 12.0;

/// Stroke weight, in grid units.
const STROKE: f32 = 2.5;

/// Clear space on every side, as a fraction of the mark's own size.
///
/// One bracket arm — 12 units at the 48-unit grid. Not a suggestion: the mark is four
/// corners of an implied frame, and anything inside the clear space reads as content
/// *within* that frame.
pub const CLEAR_SPACE: f32 = ARM / GRID;

/// Below this the negative space between the brackets closes up and the mark stops
/// reading.
///
/// **Twenty-eight, not sixteen, and the arithmetic is why.** The gap between two
/// brackets is `GRID − 2·INSET − 2·ARM` = 8 grid units, which at size *s* is `s/6`
/// points. At 16 that is 2.67px, and the square-cap emulation took a further 1.0 —
/// leaving 1.67px against epaint's ~1px of feathering on each edge. The mark rendered
/// as a 12×12 box with one red corner, which is the opposite of what it means: a
/// pixel dump of the top edge at 16 found no background pixel between the brackets on
/// any of the four sides, and at 24 found two half-covered ones. At 28 the gap is
/// 4.67px, the cap is held back to [`MIN_OPEN_GAP`], and the negative space survives.
pub const MINIMUM_SIZE: f32 = 28.0;

/// The least negative space, in points, that has to survive between two brackets.
///
/// **An absolute figure, not a fraction of the mark**, because what closes the gap is
/// epaint's antialiasing — about a pixel along each edge whatever the shape's size.
/// A rule in grid units would scale with the mark and never bite.
const MIN_OPEN_GAP: f32 = 4.0;

/// The negative space left between two brackets when the mark is drawn at `size`,
/// after the stroke and its caps have taken their share.
///
/// Exposed so [`MINIMUM_SIZE`] is a measurement rather than an assertion.
pub fn open_gap(size: f32) -> f32 {
    let unit = size / GRID;
    (GRID - 2.0 * INSET - 2.0 * ARM) * unit - 2.0 * cap(unit)
}

/// How far each free end is pushed out past its endpoint, emulating the asset's
/// square caps.
///
/// Half a stroke — which is what a square cap is — **unless that would close the
/// negative space**. The caps are a fidelity detail worth having at display sizes and
/// worth losing at 28 points, because a mark that reads as four brackets with
/// slightly short arms is still the mark, and one that reads as a box is not.
fn cap(unit: f32) -> f32 {
    let weight = (STROKE * unit).max(1.0);
    let gap = (GRID - 2.0 * INSET - 2.0 * ARM) * unit;
    (weight / 2.0).min(((gap - MIN_OPEN_GAP) / 2.0).max(0.0))
}

/// The clear space a mark of `size` points needs on each side.
pub fn clear_space(size: f32) -> f32 {
    size * CLEAR_SPACE
}

/// Paints the mark to fill `rect`, with one corner in the accent.
///
/// `rect` is the 48-unit grid, not the outer clear-space box: pass the square the
/// brackets should span and leave [`clear_space`] around it yourself. The mark is
/// drawn square inside `rect` and centred if `rect` is not, because four brackets
/// stretched into a letterbox are no longer a viewport.
pub fn paint(painter: &Painter, rect: Rect, palette: Palette) {
    paint_in(painter, rect, palette.text, palette.accent);
}

/// The single-colour cut — `assets/logo/mark-mono.svg`.
///
/// Used where the accent would be wrong: inside a menu row, in a disabled state, or
/// on any ground the red does not sit on cleanly. A mark has to work without colour
/// before colour is allowed to help it.
pub fn paint_mono(painter: &Painter, rect: Rect, color: Color32) {
    paint_in(painter, rect, color, color);
}

fn paint_in(painter: &Painter, rect: Rect, ink: Color32, accent: Color32) {
    let size = rect.width().min(rect.height());
    if size <= 0.0 {
        return;
    }
    let origin = rect.center() - Vec2::splat(size / 2.0);
    let unit = size / GRID;
    let at = |x: f32, y: f32| pos2(origin.x + x * unit, origin.y + y * unit);

    // Square caps, so the corner of a bracket is a corner rather than a bulge. epaint
    // has no cap style, so the two arms of each bracket are drawn as one three-point
    // polyline, which mitres the joint, and the free ends are extended by half the
    // stroke to land where a square cap would have put them.
    let weight = (STROKE * unit).max(1.0);

    let near = INSET;
    let far = GRID - INSET;
    let arm_in = INSET + ARM;
    let arm_out = GRID - INSET - ARM;

    let bracket = |corner: (f32, f32), horizontal: f32, vertical: f32| -> [Pos2; 3] {
        let (cx, cy) = corner;
        [at(cx, vertical), at(cx, cy), at(horizontal, cy)]
    };

    // Top-left is the accented corner, always. It is what gives the mark a reading
    // direction, so it is not a parameter.
    let top_left = bracket((near, near), arm_in, arm_in);
    let others = [
        bracket((far, near), arm_out, arm_in),
        bracket((far, far), arm_out, arm_out),
        bracket((near, far), arm_in, arm_out),
    ];

    let cap = cap(unit);
    let extend = |mut points: [Pos2; 3]| {
        // Push the two free ends outward by the cap so the arms measure 12 units to
        // the outside of the stroke, as the SVG's square caps do — held back at small
        // sizes so the caps cannot close the negative space. See [`cap`].
        let ends = [(0usize, 1usize), (2, 1)];
        for (end, inner) in ends {
            let direction = (points[end] - points[inner]).normalized();
            points[end] += direction * cap;
        }
        points
    };

    painter.add(egui::Shape::line(
        extend(top_left).to_vec(),
        Stroke::new(weight, accent),
    ));
    for corner in others {
        painter.add(egui::Shape::line(
            extend(corner).to_vec(),
            Stroke::new(weight, ink),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    /// The asset is the specification. If someone edits `mark.svg`'s grid or stroke,
    /// this fails rather than the two drifting apart in silence.
    #[test]
    fn the_geometry_matches_the_asset_it_implements() {
        let svg = include_str!("../../../assets/logo/mark.svg");
        assert!(svg.contains("viewBox=\"0 0 48 48\""), "the grid moved");
        assert!(svg.contains("stroke-width=\"2.5\""), "the stroke weight moved");
        assert!(svg.contains("stroke-linecap=\"square\""), "the caps changed");
        // The accented path is the top-left bracket: M8 20 V8 H20.
        assert!(svg.contains("d=\"M8 20V8h12\""), "the accented corner moved");
        // …and it is the only one wearing the light-mode accent.
        assert!(svg.contains("stroke=\"#00A38C\""));
        assert_eq!(svg.matches("#00A38C").count(), 1, "more than one corner is accented");

        assert_eq!(GRID, 48.0);
        assert_eq!(STROKE, 2.5);
        assert_eq!(INSET, 8.0);
        assert_eq!(ARM, 12.0);
    }

    /// The two cuts are two files because the two reds are two specifications. The
    /// palette has to agree with them, or the app and its icon are different products.
    #[test]
    fn each_cut_uses_the_accent_its_mode_specifies() {
        let light = include_str!("../../../assets/logo/mark.svg");
        let dark = include_str!("../../../assets/logo/mark-dark.svg");
        let hex = |c: Color32| format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b());

        assert!(light.contains(&hex(Palette::LIGHT.accent)), "light accent disagrees");
        assert!(light.contains(&hex(Palette::LIGHT.text)), "light ink disagrees");
        assert!(dark.contains(&hex(Palette::DARK.accent)), "dark accent disagrees");
        assert!(dark.contains(&hex(Palette::DARK.text)), "dark ink disagrees");
        // The dark cut names the red pairing in a comment explaining why it is a
        // separate file rather than a filter — that mention is the specification, so
        // it has to survive an edit to either. It describes the arrangement the dark
        // cut is *frozen at*: `xr-red` was the primary accent in both modes then, and
        // the light one has since moved to `signal-teal` without it.
        assert!(dark.contains("#E65B58 -> #C8102E"));
        assert!(!light.contains("#E65B58"), "the light mark still wears the old accent");
        assert!(
            include_str!("../../../assets/logo/mark-mono.svg").contains("currentColor"),
            "the mono cut must inherit its colour"
        );
    }

    #[test]
    fn clear_space_is_one_bracket_arm() {
        assert_eq!(CLEAR_SPACE, 0.25);
        assert_eq!(clear_space(48.0), 12.0);
        assert_eq!(clear_space(MINIMUM_SIZE), MINIMUM_SIZE * ARM / GRID);
    }

    /// The minimum size has to be the size at which the mark is still four brackets.
    /// At 16 it was not: the gap measured 1.67px against epaint's ~1.5px feathering,
    /// and the mark rendered as a closed box with one red corner — the opposite of
    /// what it means.
    #[test]
    fn the_negative_space_survives_at_the_minimum_size() {
        for size in [MINIMUM_SIZE, 32.0, 48.0, 96.0, 256.0] {
            assert!(
                open_gap(size) >= MIN_OPEN_GAP,
                "the brackets close at {size}px: {}px of gap",
                open_gap(size)
            );
        }
        assert!(open_gap(48.0) > open_gap(MINIMUM_SIZE), "the gap grows with the mark");

        // The size that was rejected, and the reason: 16 cannot reach the floor even
        // with the caps taken off entirely, so no cap rule could have saved it.
        let uncapped = |size: f32| (GRID - 2.0 * INSET - 2.0 * ARM) * (size / GRID);
        assert!(
            uncapped(16.0) < MIN_OPEN_GAP,
            "16 would now pass with {}px of gap",
            uncapped(16.0)
        );
        assert!(uncapped(MINIMUM_SIZE) >= MIN_OPEN_GAP);

        // At display sizes the caps are full half-strokes, so the drawn arms still
        // match the asset's `stroke-linecap="square"`.
        let unit = 96.0 / GRID;
        assert!((cap(unit) - (STROKE * unit) / 2.0).abs() < 1e-6);
    }

    /// Four brackets, three of them ink and one accent — and every one of them has to
    /// tessellate, or the mark draws as an empty square in the start screen.
    #[test]
    fn the_mark_paints_four_brackets_in_two_colours() {
        for theme in [Theme::Light, Theme::Dark] {
            let ctx = egui::Context::default();
            let palette = Palette::of(theme);
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                paint(
                    &ui.painter().clone(),
                    Rect::from_min_size(Pos2::ZERO, Vec2::splat(48.0)),
                    palette,
                );
            });
            let shapes: Vec<_> = output.shapes.iter().collect();
            assert_eq!(shapes.len(), 4, "{theme:?} drew {} brackets", shapes.len());

            let triangles: usize = ctx
                .tessellate(output.shapes, 1.0)
                .iter()
                .map(|clipped| match &clipped.primitive {
                    egui::epaint::Primitive::Mesh(mesh) => mesh.indices.len() / 3,
                    egui::epaint::Primitive::Callback(_) => 0,
                })
                .sum();
            assert!(triangles > 0, "{theme:?} tessellated to nothing");
        }
    }

    /// A collapsed or non-square box must not stretch the brackets into a letterbox,
    /// and a zero-sized one must not divide by it.
    #[test]
    fn a_degenerate_or_oblong_box_is_survived_and_squared() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            paint(&painter, Rect::from_min_size(Pos2::ZERO, Vec2::ZERO), Palette::LIGHT);
            paint(
                &painter,
                Rect::from_min_size(Pos2::ZERO, egui::vec2(200.0, 16.0)),
                Palette::LIGHT,
            );
            paint_mono(
                &painter,
                Rect::from_min_size(Pos2::ZERO, Vec2::splat(MINIMUM_SIZE)),
                Palette::LIGHT.muted,
            );
        });
    }
}
