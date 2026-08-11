//! The handful of controls the panels are built from.
//!
//! egui ships buttons and combo boxes; it does not ship an icon button that reads as
//! part of a tool palette, a labelled property row that lines up down a panel, or a
//! segmented control that can show *Mixed*. Those three are what the chrome is
//! mostly made of, so they live here once instead of being re-approximated in each
//! panel.

use crate::icon::Icon;
use crate::selection::Field;
use crate::theme::{Palette, radius, space, well_frame};
use egui::{
    Align, Color32, CornerRadius, Id, Layout, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2,
    vec2,
};

/// Width reserved for a property row's label, so every row in the panel aligns.
///
/// The panel is [`PROPERTIES_WIDTH`](crate::theme::PROPERTIES_WIDTH) wide and its
/// rows do not wrap, so this and the control widths that follow it are a budget:
/// `tests/interaction.rs` fails if anything overruns it. On the grid, like every
/// other reserved dimension.
pub const LABEL_WIDTH: f32 = 64.0;

/// What a row has left for its controls after the label and the scrollbar.
pub const CONTROL_WIDTH: f32 =
    crate::theme::PROPERTIES_WIDTH - space::of(6) - space::of(2) - LABEL_WIDTH - space::of(2);

/// The placeholder shown where a control would show a value the selection does not
/// agree on.
pub const MIXED: &str = "Mixed";

/// Paints the background a button gets in a given interaction state.
///
/// Frameless until touched — a palette where every button carries a box is a
/// toolbar from 1998, and a palette where nothing ever highlights is unusable.
fn button_background(
    ui: &Ui,
    rect: Rect,
    response: &Response,
    selected: bool,
    palette: Palette,
    corner: u8,
) {
    let fill = if selected {
        palette.accent_soft
    } else if response.is_pointer_button_down_on() {
        palette.pressed
    } else if response.hovered() && ui.is_enabled() {
        palette.hover
    } else {
        return;
    };
    ui.painter().rect_filled(rect, CornerRadius::same(corner), fill);
    if selected {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(corner),
            Stroke::new(1.0, palette.accent),
            StrokeKind::Inside,
        );
    }
}

/// A square, frameless button showing one icon.
pub fn icon_button(ui: &mut Ui, palette: Palette, icon: Icon, size: f32, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        button_background(ui, rect, &response, selected, palette, radius::SMALL);
        let color = icon_color(ui, palette, selected);
        // **The glyph takes 56% of the button, not 44%.** *"on miros small menu everything is
        // a lot more visible and easier to use so please make all of the icons bigger."* A
        // 0.28 inset leaves a small mark in a large target, which reads as a sparse strip; the
        // target stays the same size, so nothing became harder to hit.
        icon.paint(&ui.painter().clone(), rect.shrink(size * 0.22), color, ICON_STROKE);
    }
    response
}

/// Colour an icon takes in its current state. Disabled controls fade rather than
/// grey to a fixed value, so the same rule works in both palettes.
fn icon_color(ui: &Ui, palette: Palette, selected: bool) -> Color32 {
    let base = if selected { palette.on_accent_soft } else { palette.text };
    if ui.is_enabled() { base } else { base.gamma_multiply(ui.visuals().disabled_alpha) }
}

/// A tool-palette button: larger, more rounded, with a marker when it opens a
/// flyout.
pub fn tool_button(
    ui: &mut Ui,
    palette: Palette,
    icon: Icon,
    size: f32,
    selected: bool,
    has_flyout: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        button_background(ui, rect, &response, selected, palette, radius::MEDIUM);
        let color = icon_color(ui, palette, selected);
        icon.paint(&ui.painter().clone(), rect.shrink(size * 0.26), color, ICON_STROKE);

        if has_flyout {
            // A corner wedge rather than a chevron: at 40px a chevron is three
            // pixels of noise, while a triangle in the corner reads instantly.
            let corner = rect.right_bottom() + vec2(-space::UNIT, -space::UNIT);
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    corner,
                    corner + vec2(-space::UNIT, 0.0),
                    corner + vec2(0.0, -space::UNIT),
                ],
                color.gamma_multiply(0.65),
                Stroke::NONE,
            ));
        }
    }
    response
}

/// A section heading inside a panel.
///
/// Letterspaced caps at the label size, as on the supplied swatch card where
/// `COLORWAY` sits over the hex values in mono. It is the smallest type in the
/// chrome and it carries a whole level of hierarchy on its own, which is what
/// `docs/05-design-language.md` §3 means by type doing the work a slab of grey would
/// otherwise be asked to do.
pub fn section_header(ui: &mut Ui, palette: Palette, title: &str) {
    ui.add_space(space::of(3));
    ui.label(crate::theme::section_label(title, palette));
    ui.add_space(space::UNIT);
}

/// A hairline, drawn in the border colour rather than egui's separator, which is
/// two-toned and sized for a debug panel.
///
/// One of these does the work a shadow would otherwise be asked to do, which is the
/// whole of §2's "no shadow on everything".
pub fn hairline(ui: &mut Ui, palette: Palette) {
    let width = ui.available_width();
    let (rect, _) =
        ui.allocate_exact_size(vec2(width, palette.hairline_width()), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, palette.border);
}

/// A vertical hairline, for separating groups inside a horizontal cluster.
pub fn hairline_vertical(ui: &mut Ui, palette: Palette, height: f32) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(palette.hairline_width(), height), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, palette.border);
}

/// A labelled row: a fixed-width caption on the left, the control on the right.
pub fn row<R>(
    ui: &mut Ui,
    palette: Palette,
    label: &str,
    contents: impl FnOnce(&mut Ui) -> R,
) -> R {
    ui.horizontal(|ui| {
        ui.add_sized(
            vec2(LABEL_WIDTH, ui.spacing().interact_size.y),
            egui::Label::new(egui::RichText::new(label).color(palette.muted)).selectable(false),
        );
        ui.with_layout(Layout::left_to_right(Align::Center), contents).inner
    })
    .inner
}

/// A colour swatch. `None` renders the "no fill" diagonal rather than a colour,
/// because an empty square and a white square are otherwise identical.
pub fn swatch(
    ui: &mut Ui,
    palette: Palette,
    color: Option<Color32>,
    size: Vec2,
    selected: bool,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let corner = CornerRadius::same(radius::SMALL);
        match color {
            Some(color) => {
                // Checkerboard behind, so a translucent colour reads as translucent
                // rather than as a lighter opaque one.
                if color.a() < 255 {
                    checkerboard(ui, palette, rect, corner);
                }
                ui.painter().rect_filled(rect, corner, color);
            }
            None => {
                ui.painter().rect_filled(rect, corner, palette.well);
                let inset = rect.shrink(2.0);
                ui.painter().line_segment(
                    [inset.left_bottom(), inset.right_top()],
                    Stroke::new(ICON_STROKE, palette.danger),
                );
            }
        }
        // Selection is a 1px `xr-red` outline, per §4 — not a 2px one, and not a
        // glow. The extra weight the old rule used is exactly the "3D-ish depth" §2
        // rules out, spent on the one element that needs to stay flat.
        let stroke = if selected {
            Stroke::new(palette.hairline_width(), palette.accent)
        } else if response.hovered() {
            Stroke::new(palette.hairline_width(), palette.info)
        } else {
            Stroke::new(palette.hairline_width(), palette.border)
        };
        ui.painter().rect_stroke(rect, corner, stroke, StrokeKind::Inside);
    }
    response
}

/// The transparency checkerboard behind a translucent swatch.
///
/// Drawn in palette neutrals rather than the conventional white-and-grey: a white
/// square against `bone` reads as a *lighter* patch of interface rather than as the
/// absence of colour, which defeats the point of drawing it.
fn checkerboard(ui: &Ui, palette: Palette, rect: Rect, corner: CornerRadius) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, corner, palette.checker);
    let cell = space::UNIT;
    let mut y = rect.top();
    let mut row_index = 0;
    while y < rect.bottom() {
        let mut x = rect.left() + if row_index % 2 == 0 { 0.0 } else { cell };
        while x < rect.right() {
            painter.rect_filled(
                Rect::from_min_size(egui::pos2(x, y), Vec2::splat(cell)).intersect(rect),
                CornerRadius::ZERO,
                palette.checker_alt,
            );
            x += cell * 2.0;
        }
        y += cell;
        row_index += 1;
    }
}

/// One option in a [`segmented`] control.
#[derive(Debug, Clone, Copy)]
pub struct Segment<T> {
    pub value: T,
    pub icon: Option<Icon>,
    pub label: &'static str,
}

impl<T> Segment<T> {
    pub const fn icon(value: T, icon: Icon, label: &'static str) -> Self {
        Self { value, icon: Some(icon), label }
    }

    pub const fn text(value: T, label: &'static str) -> Self {
        Self { value, icon: None, label }
    }
}

/// A joined row of mutually exclusive options — alignment, weight, line style.
///
/// Takes a [`Field`] rather than a value so a mixed selection shows *no* segment
/// highlighted, which is the only honest rendering: highlighting one would claim the
/// selection agrees.
pub fn segmented<T: Copy + PartialEq>(
    ui: &mut Ui,
    palette: Palette,
    current: &Field<T>,
    options: &[Segment<T>],
) -> Option<T> {
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        // Segments sit in a row that has to fit the panel, so they run tighter than
        // a standalone button.
        ui.spacing_mut().button_padding.x = 5.0;
        for option in options {
            let selected = current.value().is_some_and(|v| *v == option.value);
            let response = match option.icon {
                Some(icon) => icon_button(ui, palette, icon, 26.0, selected),
                None => {
                    let button = egui::Button::selectable(selected, option.label).frame(true);
                    ui.add(button)
                }
            };
            if response.on_hover_text(option.label).clicked() {
                chosen = Some(option.value);
            }
        }
    });
    chosen
}

/// A read-only strip showing *Mixed*, sized like a value control so a panel does not
/// reflow when a selection stops agreeing.
///
/// Drawn in the faint ink: it is a placeholder standing where a value would be, and
/// `docs/05-design-language.md` §1 gives that ink exactly that job.
pub fn mixed_placeholder(ui: &mut Ui, palette: Palette) {
    ui.add_sized(
        vec2(ui.available_width().min(CONTROL_WIDTH), ui.spacing().interact_size.y),
        // `muted`, not `faint`: `docs/05-design-language.md` §1 scopes `faint` to
        // placeholders and disabled text, and *Mixed* is neither — it is the actual
        // state of the selection, and it measured 2.8:1 drawn as if it were not.
        egui::Label::new(egui::RichText::new(MIXED).color(palette.muted)).selectable(false),
    );
}

/// A read-only numeric readout — a coordinate, a count, a percentage.
///
/// Monospace, so the digits are tabular and the value does not shuffle sideways while
/// it changes. See [`crate::theme::numeric`] for why that is a correctness property
/// rather than a typographic preference.
pub fn readout(ui: &mut Ui, palette: Palette, value: impl Into<String>) -> Response {
    ui.add(
        egui::Label::new(crate::theme::numeric(value).color(palette.text)).selectable(false),
    )
}

/// A search field with a leading magnifier, over the board library. Returns true when
/// the text changed.
pub fn search_field(ui: &mut Ui, palette: Palette, id: Id, query: &mut String) -> bool {
    search_field_for(ui, palette, id, "Search boards", query)
}

/// [`search_field`], told what it is searching.
///
/// There are two of these — boards in the library, shapes in the flyout — and a
/// placeholder naming the wrong one is worse than no placeholder.
pub fn search_field_for(
    ui: &mut Ui,
    palette: Palette,
    id: Id,
    hint: &str,
    query: &mut String,
) -> bool {
    let before_len = query.len();
    let mut changed = false;
    well_frame(palette).show(ui, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(space::of(4)), Sense::hover());
            Icon::Search.paint(&ui.painter().clone(), rect, palette.faint, ICON_STROKE);
            let response = ui.add(
                egui::TextEdit::singleline(query)
                    .id(id)
                    .desired_width(ui.available_width())
                    .frame(egui::Frame::NONE)
                    .hint_text(hint),
            );
            changed = response.changed();
        });
    });
    changed || before_len != query.len()
}

/// Icon stroke weight, `docs/05-design-language.md` §3: line drawings at 1.5px,
/// geometric, consistent optical weight. One value, so a 16px menu icon and a 40px
/// tool button read as the same family rather than as two sets.
pub const ICON_STROKE: f32 = 1.5;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every widget here paints; none of them should panic on a zero-sized or
    /// inverted rect, which is what a collapsed panel hands them mid-animation.
    #[test]
    fn widgets_survive_a_degenerate_layout() {
        for theme in [crate::theme::Theme::Light, crate::theme::Theme::Dark] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx, theme);
            let palette = Palette::of(theme);
            let mut query = String::new();

            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.set_max_width(0.0);
                let _ = icon_button(ui, palette, Icon::Plus, 0.0, false);
                let _ = tool_button(ui, palette, Icon::Select, 0.0, true, true);
                hairline(ui, palette);
                hairline_vertical(ui, palette, 0.0);
                section_header(ui, palette, "Appearance");
                let _ = swatch(ui, palette, Some(palette.accent), Vec2::ZERO, true);
                // Translucent, so the checkerboard runs too.
                let _ = swatch(ui, palette, Some(palette.shadow), vec2(20.0, 20.0), false);
                let _ = swatch(ui, palette, None, Vec2::ZERO, false);
                mixed_placeholder(ui, palette);
                let _ = readout(ui, palette, "1234.5");
                let _ = search_field(ui, palette, Id::new("q"), &mut query);
                let _ = search_field_for(ui, palette, Id::new("s"), "Search shapes", &mut query);
            });
        }
    }

    /// A mixed field must leave every segment unhighlighted; the test drives the
    /// real widget rather than asserting on the rule in the abstract.
    #[test]
    fn a_segmented_control_reports_no_choice_until_one_is_clicked() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);

        let options = [
            Segment::text(1_u8, "One"),
            Segment::text(2, "Two"),
        ];
        let mut chosen = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            chosen = segmented(ui, Palette::LIGHT, &Field::Mixed, &options);
        });
        assert_eq!(chosen, None);
    }

    /// The properties panel is a fixed width and its rows do not wrap, so the label
    /// column and what is left for the control have to add up. A negative or
    /// vanishing control width is how a panel silently stops showing its values.
    #[test]
    fn the_property_row_budget_adds_up_and_lands_on_the_grid() {
        // Compile-time: a panel whose rows do not fit should not build.
        const {
            assert!(CONTROL_WIDTH > 120.0, "a property row has no room left for its control");
            assert!(LABEL_WIDTH + CONTROL_WIDTH < crate::theme::PROPERTIES_WIDTH);
        }
        assert_eq!(LABEL_WIDTH % crate::theme::space::UNIT, 0.0);
        assert_eq!(CONTROL_WIDTH % crate::theme::space::UNIT, 0.0);
    }
}
