//! The bottom-right status cluster: zoom readout, zoom controls, minimap toggle.
//!
//! Floating over the canvas rather than docked, for the same reason as the tool
//! palette: the board should never be letterboxed by chrome.

use crate::command::{Command, CommandContext};
use crate::event::{EventSink, UiEvent};
use crate::icon::Icon;
use crate::theme::{
    Backing, GlassSurface, Palette, floating_frame, numeric, paint_glass_edge, radius, space,
};
use crate::widgets::icon_button;
use egui::{Align2, Id, Order, Ui, vec2};

/// The zoom steps the readout snaps to when picked from its menu, as fractions.
///
/// Chosen so the list reads as percentages a user would ask for rather than as
/// powers of the wheel step.
pub const ZOOM_STEPS: [f32; 9] = [0.1, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 4.0, 8.0];

/// Formats a zoom factor the way the readout shows it.
///
/// Rounded to whole percent and to no decimals ever: a canvas that reads "127.4%"
/// while the user drags is noise, and `zoom_label(1.0)` must be exactly "100%" or the
/// 100% command looks like it did not work.
///
/// **A board that is on screen never reads "0%".** Below half a percent the rounding
/// used to produce a readout that says the board is not being shown, on a board that
/// plainly is — so anything above zero floors at 1%. Genuine zero is unreachable
/// (`vellum_scene::MIN_ZOOM` is 1%) and is kept only so the function is total.
pub fn zoom_label(zoom: f32) -> String {
    let percent = f64::from(zoom) * 100.0;
    let rounded = percent.round() as i64;
    let shown = if rounded == 0 && percent > 0.0 { 1 } else { rounded };
    format!("{shown}%")
}

/// Edge length of the cluster's buttons. Smaller than the tool palette's: this is a
/// place the pointer visits, not one it lives in.
const BUTTON: f32 = space::of(7) - 2.0;

pub(crate) fn show(
    ui: &mut Ui,
    palette: Palette,
    zoom: f32,
    minimap_visible: bool,
    cmd_ctx: &CommandContext,
    events: &mut EventSink,
    glass: &mut Vec<GlassSurface>,
) {
    let mut cluster = egui::Rect::NOTHING;
    egui::Area::new(Id::new("vellum-status"))
        .anchor(Align2::RIGHT_BOTTOM, vec2(-space::of(4), -space::of(4)))
        .order(Order::Middle)
        .show(ui.ctx(), |ui| {
            let inner = floating_frame(palette).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;

                    if icon_button(ui, palette, Icon::Minimap, BUTTON, minimap_visible)
                        .on_hover_text("Minimap")
                        .clicked()
                    {
                        events.command(Command::ToggleMinimap);
                    }
                    crate::widgets::hairline_vertical(ui, palette, space::of(5));

                    if icon_button(ui, palette, Icon::ZoomOut, BUTTON, false)
                        .on_hover_text(Command::ZoomOut.label())
                        .clicked()
                    {
                        events.command(Command::ZoomOut);
                    }
                    zoom_readout(ui, palette, zoom, cmd_ctx, events);
                    if icon_button(ui, palette, Icon::ZoomIn, BUTTON, false)
                        .on_hover_text(Command::ZoomIn.label())
                        .clicked()
                    {
                        events.command(Command::ZoomIn);
                    }

                    crate::widgets::hairline_vertical(ui, palette, space::of(5));
                    if icon_button(ui, palette, Icon::ZoomToFit, BUTTON, false)
                        .on_hover_text(Command::ZoomToFit.label())
                        .clicked()
                    {
                        events.command(Command::ZoomToFit);
                    }
                });
            });
            cluster = inner.response.rect;
            paint_glass_edge(ui.painter(), cluster, palette, Backing::Canvas);
        });

    if let Some(spec) = palette.glass(Backing::Canvas)
        && cluster.is_positive()
    {
        glass.push(GlassSurface {
            rect: cluster,
            corner_radius: f32::from(radius::LARGE),
            opacity: spec.opacity,
        });
    }
}

/// The percentage itself, which is also the menu of zoom presets.
///
/// Monospace, and this is the single most load-bearing use of it in the crate: the
/// readout changes on every wheel notch of a pinch gesture, and proportional digits
/// make the whole cluster twitch left and right while the user zooms. Fixed-width so
/// `100%` and `1000%` occupy the same box, which is also why the button has a minimum
/// size rather than hugging its label.
fn zoom_readout(
    ui: &mut Ui,
    palette: Palette,
    zoom: f32,
    cmd_ctx: &CommandContext,
    events: &mut EventSink,
) {
    let button = egui::Button::new(numeric(zoom_label(zoom)).color(palette.text))
        .frame(false)
        .min_size(vec2(space::of(13), BUTTON));

    egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
        ui.set_min_width(space::of(35));
        for step in ZOOM_STEPS {
            let selected = (zoom - step).abs() < 0.001;
            if ui.selectable_label(selected, numeric(zoom_label(step))).clicked() {
                events.push(UiEvent::ZoomTo(step));
            }
        }
        crate::widgets::hairline(ui, palette);
        // *Zoom to selection* is the one entry here that can be unavailable, and it is
        // unavailable most of the time. It says why rather than sitting there inert.
        for command in [Command::ZoomToFit, Command::ZoomToSelection, Command::ZoomActualSize] {
            let available = command.availability(cmd_ctx);
            let mut response =
                ui.add_enabled(available.is_enabled(), egui::Button::new(command.label()));
            if let Some(why) = available.reason() {
                response = response.on_disabled_hover_text(why);
            }
            if response.clicked() {
                events.command(command);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Context;

    #[test]
    fn the_readout_is_whole_percent_and_exact_at_one_to_one() {
        assert_eq!(zoom_label(1.0), "100%");
        assert_eq!(zoom_label(0.1), "10%");
        assert_eq!(zoom_label(8.0), "800%");
        assert_eq!(zoom_label(1.274), "127%");
        // A board that is on screen never reads "0%".
        assert_eq!(zoom_label(0.004), "1%");
        assert_eq!(zoom_label(0.0), "0%");
    }

    #[test]
    fn the_zoom_steps_are_ascending_and_include_one_to_one() {
        assert!(ZOOM_STEPS.windows(2).all(|w| w[0] < w[1]));
        assert!(ZOOM_STEPS.contains(&1.0));
    }

    #[test]
    fn the_cluster_draws_and_emits_nothing_untouched() {
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut events = EventSink::default();
        let mut glass = Vec::new();
        let cmd_ctx = CommandContext { board_open: true, ..CommandContext::default() };
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            show(ui, Palette::LIGHT, 1.5, true, &cmd_ctx, &mut events, &mut glass);
        });
        assert!(events.take().is_empty());
        assert_eq!(glass.len(), 1, "the cluster floats over the canvas");
    }

    /// The readout is the most-changed number on screen. If its box resizes with the
    /// value, the whole cluster shifts sideways on every wheel notch — which is
    /// exactly the sort of stutter this project exists to refuse.
    #[test]
    fn the_readout_box_is_the_same_width_at_every_zoom() {
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let widths: Vec<f32> = [0.1, 1.0, 1.274, 8.0]
            .into_iter()
            .map(|zoom| {
                let mut events = EventSink::default();
                let mut glass = Vec::new();
                let cmd_ctx = CommandContext { board_open: true, ..CommandContext::default() };
                let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                    show(ui, Palette::LIGHT, zoom, false, &cmd_ctx, &mut events, &mut glass);
                });
                glass[0].rect.width()
            })
            .collect();
        assert!(
            widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.01),
            "the cluster resizes with the zoom value: {widths:?}"
        );
    }
}
