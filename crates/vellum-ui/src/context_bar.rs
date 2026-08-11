//! The small toolbar that floats directly above whatever is selected.
//!
//! *"almost all controls appear right above the what i right clicked … there is a
//! small menu for the lets say the pen stroke itself"*. This is that bar, and it is
//! the reason `crate::properties`' docked column is no longer drawn by default.
//!
//! # Why above the selection rather than down the side
//!
//! A canvas app spends its screen on the canvas. A permanent 250px column costs board
//! space on every frame to hold controls that are relevant on some of them, and it
//! puts the control at the far right of the window while the thing it changes is
//! wherever the user happens to be working. Miro's answer — and now ours — is to put
//! the controls **where the attention already is** and take them away with the
//! selection.
//!
//! # What goes in it
//!
//! Not everything. The bar holds the controls a selection is most likely to want
//! next, and [`crate::context_menu`] holds the rest — which is the same split Miro
//! makes and the reason `⋮` is the last button in every configuration. Nothing was
//! removed: the docked panel is still there, still complete, one row down that menu
//! or `⌥⌘P` away.
//!
//! # The controls are data
//!
//! [`controls`] answers a `Vec<Control>` with no `egui` involved, so *which* controls
//! a selection gets is decided in one pure function and tested without a window. An
//! ink stroke gets a width, a colour, a lock and a `⋮` — exactly the four Miro shows —
//! not because a `match` on `ItemFacet::Ink` says so, but because ink has a stroke and
//! no fill and no words. Deriving it from the properties the selection actually has
//! is what keeps a new item kind from arriving with a blank bar.
//!
//! # Everything it emits, the panel emits too
//!
//! Every control here pushes the same [`StyleEdit`] the equivalent panel row does, so
//! the app needed no new event to wire this up and the two can never disagree about
//! what a fill change means.

use egui::{Align2, Context, Id, Order, Pos2, Rect, Vec2, pos2, vec2};

use crate::color::{ColorPicker, to_egui};
use crate::command::CommandContext;
use crate::event::{EventSink, StyleEdit, UiEvent};
use crate::icon::Icon;
use crate::selection::{Field, FontWeight, PanelModel};
use crate::theme::{Backing, Palette, floating_frame, floating_frame_over, paint_glass_edge, space};
use crate::widgets::{icon_button, swatch};
use vellum_connect::{Arrowhead, LineStyle, RoutingMode};
use vellum_doc::{Align, CardMode, Color};

/// One control in the bar.
///
/// Ordered by how often it is reached for, left to right, with the two that every
/// selection gets pinned to the right end so their position never moves — a lock
/// button that slides horizontally with the selection's kind is a button you have to
/// look for every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// A card's display form: Row, Card or Large.
    CardMode,
    /// Open the one selected card's page.
    OpenPage,
    /// The item's own fill colour.
    Fill,
    /// A shape's outline, an ink stroke, or a connector's line — all three are
    /// `Style::stroke`, which is why one control serves them. See
    /// `vellum_app::inspect::stroke_home`.
    Stroke,
    StrokeWidth,
    /// Solid, dashed or dotted. Connectors only — a shape's SDF border is one
    /// coverage band and cannot be dashed, and ink has never asked to be.
    LineStyle,
    Routing,
    /// The terminator at the far end, which is the one anybody changes.
    EndArrow,
    /// Whole-item opacity — **only when nothing else on the bar already carries an
    /// alpha channel**. See the rule in [`controls`].
    Opacity,
    TextColor,
    /// The typeface, as a dropdown showing its current name.
    ///
    /// Miro puts this on the bar for a shape and for a text box — *"Noto Sans"*, left of the
    /// size — and it was reachable in Velm only from the docked properties panel, which is off
    /// by default. Of everything the panel holds that the bar does not, this is the one the
    /// user's own reference screenshots show, and the one a person changes without first
    /// deciding to go looking for it.
    FontFamily,
    FontSize,
    /// A single **B**, not the panel's three-way weight control: a bar has no room
    /// for three labelled segments and *bold* is the weight anybody presses.
    Bold,
    TextAlign,
    /// A hairline between bands.
    Separator,
    Lock,
    /// Opens [`crate::context_menu`] — everything that did not fit.
    More,
}

/// Which controls a selection gets.
///
/// Pure, and derived from the properties the selection *has* rather than from its
/// kind. The bands are: what the item is (a card), how it is painted, what its words
/// look like, and then the two every selection gets.
pub fn controls(model: &PanelModel) -> Vec<Control> {
    let mut out: Vec<Control> = Vec::new();
    if model.count == 0 {
        return out;
    }

    // A card first: its display form is the property somebody selected it to change.
    if !model.card_mode.is_absent() {
        out.push(Control::CardMode);
        if model.link_url.is_some() {
            out.push(Control::OpenPage);
        }
    }

    let paint_from = out.len();
    if !model.fill.is_absent() {
        out.push(Control::Fill);
    }
    if !model.border_color.is_absent() {
        out.push(Control::Stroke);
        out.push(Control::StrokeWidth);
        // Connectors only. `routing` is `Absent` for every other kind, which makes it
        // the honest test for "this selection is wire" — and keeps an ink stroke's bar
        // to the four controls Miro gives it.
        if !model.routing.is_absent() {
            out.push(Control::LineStyle);
            out.push(Control::Routing);
            out.push(Control::EndArrow);
        }
    }
    // Opacity, for anything without a **fill** swatch.
    //
    // It used to require no stroke either, which kept an ink stroke to the four controls
    // Miro gives it — the user had sent a picture of exactly those four. They have since
    // asked for the opposite, having seen that Miro puts an Opacity slider inside the
    // *flyout* behind its stroke swatch: *"for the pen strokes you can make it so that there
    // is also opacity etc"*. So a pen stroke gets five.
    //
    // A **fill** still suppresses it, and that part is unchanged: a fill swatch's own picker
    // carries alpha, so an unconditional control would put a second one beside it — two
    // controls for one property, disagreeing whenever either is touched.
    if !model.opacity.is_absent() && model.fill.is_absent() {
        out.push(Control::Opacity);
    }
    if out.len() > paint_from && paint_from > 0 {
        out.insert(paint_from, Control::Separator);
    }

    let text_from = out.len();
    if !model.text_color.is_absent() {
        out.push(Control::TextColor);
        // Only when the selection actually reports one — a link card has text drawn on it and
        // no typography to change (`inspect::text_of` returns `None` for a card, because its
        // title is fetched metadata rather than something the panel should restyle).
        if !model.font_family.is_absent() {
            out.push(Control::FontFamily);
        }
        out.push(Control::FontSize);
        // Miro rules between the *kinds* of typographic control — the face and its size, then
        // the emphasis and the alignment — rather than only at the edges of the group. It is
        // what makes a long bar scannable instead of a row of adjacent buttons.
        out.push(Control::Separator);
        out.push(Control::Bold);
        out.push(Control::TextAlign);
        if text_from > 0 {
            out.insert(text_from, Control::Separator);
        }
    }

    if !out.is_empty() {
        out.push(Control::Separator);
    }
    out.push(Control::Lock);
    out.push(Control::More);
    out
}

/// Which swatch in the bar has its picker open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swatch {
    Fill,
    Stroke,
    Text,
}

/// The bar's memory between frames.
#[derive(Debug, Default)]
pub struct ContextBarState {
    open: Option<Swatch>,
    picker: Option<ColorPicker>,
}

impl ContextBarState {
    fn open_picker(&mut self, which: Swatch, seed: Color, alpha: bool) {
        if self.open == Some(which) {
            self.open = None;
            return;
        }
        self.open = Some(which);
        self.picker = Some(ColorPicker::new(seed, alpha));
    }
}

/// What the caller has to act on after the bar has drawn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Output {
    /// What the bar took, for the glass register. `Rect::NOTHING` when it did not draw.
    pub rect: Rect,
    /// Where to open the context menu, if `⋮` was pressed. The bar does not open it
    /// itself so that this module never has to know the menu exists.
    pub more_at: Option<Pos2>,
}

impl Default for Output {
    fn default() -> Self {
        // `Rect::NOTHING` rather than a derived zero rect: `register_glass` tests
        // `is_positive`, and an inverted-infinity rectangle is the value egui itself
        // uses for "there isn't one".
        Self { rect: Rect::NOTHING, more_at: None }
    }
}

/// How tall the bar is, near enough to decide whether it fits above the selection.
///
/// A guess, and deliberately one: the real height is not known until the row has laid
/// out, and by then the position has been chosen. It only has to be right enough to
/// pick *above* or *below*, and it is one row of `space::of(6)` controls inside a
/// frame whose margin is `space::UNIT` each side.
const BAR_HEIGHT: f32 = CONTROL + space::UNIT * 3.0;

/// The height of every control on the bar, and of the bar's own rhythm.
///
/// **32pt against the 24 it was.** The user put Miro's bars beside Velm's — for a sticky, a
/// shape, a text box, an image, a link and a pen stroke — and the difference that reads first
/// is not which controls are there, it is that Miro's bar is roughly half as tall again and
/// breathes. A 24pt control in a 32pt bar is a dense strip of icons; Miro's is a row of
/// buttons you can hit. This is one number because every control on the bar is sized from it,
/// which is what stops the swatch, the stepper and the icon buttons drifting apart.
const CONTROL: f32 = space::of(8);

/// The gap between the bar and the thing it belongs to.
const GAP: f32 = space::of(3);

/// Draws the bar above `anchor`.
///
/// `anchor` is the selection's bounding box **in screen space** — the app converts it,
/// because only the app has the camera.
///
/// `room` is where the bar may live: the canvas, less the gutter the floating tool
/// palette occupies. Two placements come out of it. A selection at the very top of the
/// window gets its bar *underneath* rather than behind the menu bar, and one against
/// the left edge has its bar pushed right rather than under the tool column — measured,
/// a selection at the board's left edge put the fill swatch behind the palette.
#[expect(clippy::too_many_arguments, reason = "one call per frame; every argument is read")]
pub(crate) fn show(
    ctx: &Context,
    palette: Palette,
    state: &mut ContextBarState,
    model: &PanelModel,
    cmd_ctx: &CommandContext,
    anchor: Rect,
    room: Rect,
    font_families: &[String],
    events: &mut EventSink,
) -> Output {
    let items = controls(model);
    if items.is_empty() || !anchor.is_positive() {
        return Output::default();
    }

    // Above by preference, below when there is no room. The `pivot` is what centres
    // the bar on the selection without knowing its width: egui places the pivot corner
    // at `fixed_pos` and lays the rest out around it.
    let above = anchor.top() - GAP - BAR_HEIGHT >= room.top();
    let (pivot, at) = if above {
        (Align2::CENTER_BOTTOM, pos2(anchor.center().x, anchor.top() - GAP))
    } else {
        (Align2::CENTER_TOP, pos2(anchor.center().x, anchor.bottom() + GAP))
    };

    let mut more_at = None;
    let rect = egui::Area::new(Id::new("velm-context-bar"))
        .fixed_pos(at)
        .pivot(pivot)
        .order(Order::Middle)
        // A selection at the window's edge would otherwise hang its bar off the side,
        // where the last control cannot be reached.
        .constrain_to(room)
        .show(ctx, |ui| {
            let inner = floating_frame(palette).show(ui, |ui| {
                ui.horizontal(|ui| {
                    // **Tight between controls, with the air in the dividers instead.** Miro's
                    // bar looks roomy because its groups are separated by rules, not because
                    // every button is held apart from its neighbour — *"use less padding
                    // proportional to miro's"*. Spacing the buttons as well as ruling the
                    // groups spends the width twice and makes the bar long enough to reach
                    // past the item it belongs to.
                    ui.spacing_mut().item_spacing = vec2(space::UNIT * 0.5, 0.0);
                    for control in &items {
                        if let Some(pos) =
                            draw(ui, palette, state, *control, model, cmd_ctx, font_families, events)
                        {
                            more_at = Some(pos);
                        }
                    }
                });
            });
            paint_glass_edge(ui.painter(), inner.response.rect, palette, Backing::Canvas);
            inner.response.rect
        })
        .inner;

    Output { rect, more_at }
}

/// One control. Answers where to open the context menu, for `⋮` alone.
#[expect(clippy::too_many_arguments, reason = "one dispatch; each control reads a different one")]
fn draw(
    ui: &mut egui::Ui,
    palette: Palette,
    state: &mut ContextBarState,
    control: Control,
    model: &PanelModel,
    cmd_ctx: &CommandContext,
    font_families: &[String],
    events: &mut EventSink,
) -> Option<Pos2> {
    match control {
        Control::Separator => {
            ui.add_space(space::UNIT);
            crate::widgets::hairline_vertical(ui, palette, space::of(5));
            ui.add_space(space::UNIT);
        }

        Control::CardMode => {
            let options = [
                crate::widgets::Segment::text(CardMode::Link, "Row"),
                crate::widgets::Segment::text(CardMode::Card, "Card"),
                crate::widgets::Segment::text(CardMode::Large, "Large"),
            ];
            if let Some(mode) = crate::widgets::segmented(ui, palette, &model.card_mode, &options) {
                events.style(StyleEdit::CardMode(mode));
            }
        }

        Control::OpenPage => {
            if let Some(url) = model.link_url.clone()
                && icon_button(ui, palette, Icon::Import, CONTROL, false)
                    .on_hover_text(format!("Open {url}"))
                    .clicked()
            {
                events.push(UiEvent::OpenLink(url));
            }
        }

        Control::Fill => {
            let shown = match &model.fill {
                Field::Uniform(color) => color.map(to_egui),
                Field::Absent | Field::Mixed => None,
            };
            let response = swatch(ui, palette, shown, SWATCH, false).on_hover_text("Fill");
            if response.clicked() {
                let seed = model.fill.value().copied().flatten().unwrap_or(crate::color::DEFAULT_FILL);
                state.open_picker(Swatch::Fill, seed, true);
            }
            picker(palette, state, Swatch::Fill, &response, events, |c| StyleEdit::Fill(Some(c)));
        }

        Control::Stroke => {
            let shown = model.border_color.value().copied().map(to_egui);
            let response = swatch(ui, palette, shown, SWATCH, false).on_hover_text("Line colour");
            if response.clicked() {
                let seed = model.border_color.or(DEFAULT_STROKE);
                state.open_picker(Swatch::Stroke, seed, true);
            }
            picker(palette, state, Swatch::Stroke, &response, events, StyleEdit::BorderColor);
        }

        Control::StrokeWidth => {
            let mut width = model.border_width.or(1.0);
            let drag = labelled_number(
                ui,
                palette,
                Icon::Thickness,
                egui::DragValue::new(&mut width).speed(0.1).range(0.0..=64.0).max_decimals(1),
            )
            .on_hover_text("Line width");
            if drag.changed() {
                events.style(StyleEdit::BorderWidth(width));
            }
        }

        Control::LineStyle => {
            let options = [
                crate::widgets::Segment::text(LineStyle::Solid, "──"),
                crate::widgets::Segment::text(LineStyle::Dashed, "- -"),
                crate::widgets::Segment::text(LineStyle::Dotted, "···"),
            ];
            if let Some(style) = crate::widgets::segmented(ui, palette, &model.border_style, &options)
            {
                events.style(StyleEdit::BorderStyle(style));
            }
        }

        Control::Routing => {
            let options = [
                crate::widgets::Segment::text(RoutingMode::Straight, "Straight"),
                crate::widgets::Segment::text(RoutingMode::Orthogonal, "Elbow"),
                crate::widgets::Segment::text(RoutingMode::Curved, "Curved"),
            ];
            if let Some(routing) = crate::widgets::segmented(ui, palette, &model.routing, &options) {
                events.style(StyleEdit::Routing(routing));
            }
        }

        Control::EndArrow => {
            let current = model.end_arrow.value().copied().unwrap_or(Arrowhead::None);
            let next = if current == Arrowhead::None {
                Arrowhead::FilledTriangle
            } else {
                Arrowhead::None
            };
            let on = current != Arrowhead::None;
            if icon_button(ui, palette, Icon::ChevronRight, CONTROL, on)
                .on_hover_text(if on { "Remove the arrowhead" } else { "Add an arrowhead" })
                .clicked()
            {
                events.style(StyleEdit::EndArrow(next));
            }
        }

        Control::Opacity => {
            let mut percent = model.opacity.or(1.0) * 100.0;
            let drag = labelled_number(
                ui,
                palette,
                Icon::Opacity,
                egui::DragValue::new(&mut percent).speed(1.0).range(0.0..=100.0).max_decimals(0).suffix("%"),
            )
            .on_hover_text("Opacity");
            if drag.changed() {
                events.style(StyleEdit::Opacity(percent / 100.0));
            }
        }

        Control::TextColor => {
            let shown = model.text_color.value().copied().map(to_egui);
            let response = swatch(ui, palette, shown, SWATCH, false).on_hover_text("Text colour");
            if response.clicked() {
                let seed = model.text_color.or(DEFAULT_INK);
                state.open_picker(Swatch::Text, seed, false);
            }
            picker(palette, state, Swatch::Text, &response, events, StyleEdit::TextColor);
        }

        Control::FontFamily => {
            // The name itself is the control, as it is in Miro's bar and in every word
            // processor — a swatch or an icon would say "typeface" without saying *which*.
            let current = match &model.font_family {
                Field::Uniform(Some(name)) => name.clone(),
                Field::Uniform(None) => "Default".to_owned(),
                Field::Mixed => crate::widgets::MIXED.to_owned(),
                Field::Absent => return None,
            };
            egui::ComboBox::from_id_salt("velm-bar-font-family")
                .selected_text(current)
                // Wide enough for a real family name and no wider: this sits in a floating bar
                // over the board, and every point it takes is board the user cannot see.
                .width(space::of(24))
                .height(space::of(60))
                .show_ui(ui, |ui| {
                    // **`Default` is a real choice, not the absence of one.** It is what an
                    // item carries until somebody names a family, and picking it back is the
                    // only way to undo a font change without `⌘Z`.
                    if ui.selectable_label(false, "Default").clicked() {
                        events.style(StyleEdit::FontFamily(None));
                    }
                    for family in font_families {
                        let selected =
                            model.font_family.value().is_some_and(|f| f.as_deref() == Some(family));
                        if ui.selectable_label(selected, family).clicked() {
                            events.style(StyleEdit::FontFamily(Some(family.clone())));
                        }
                    }
                });
        }
        Control::FontSize => {
            // Auto-fit is `None`, so a sticky that has never been given a size shows
            // the default rather than a blank — and typing one turns auto-fit off,
            // which is the same bargain the panel's checkbox makes explicitly.
            let mut size = model.font_size.value().copied().flatten().unwrap_or(14.0);
            let drag = number(ui, egui::DragValue::new(&mut size).speed(0.5).range(4.0..=400.0).max_decimals(1))
                .on_hover_text(if matches!(model.font_size, Field::Uniform(None)) {
                    "Font size — auto-fitted; set one to pin it"
                } else {
                    "Font size"
                });
            if drag.changed() {
                events.style(StyleEdit::FontSize(Some(size)));
            }
        }

        Control::Bold => {
            let bold = model.font_weight.value() == Some(&FontWeight::Bold);
            // `Button::selectable`, exactly as `widgets::segmented` draws its textual
            // segments — so the selected fill and its contrast come from the palette
            // rather than from a colour named here, which `tests/tokens.rs` forbids.
            let response = ui.add(
                egui::Button::selectable(bold, egui::RichText::new("B").strong())
                    .frame(true)
                    .min_size(Vec2::splat(CONTROL)),
            );
            if response.on_hover_text("Bold").clicked() {
                events.style(StyleEdit::FontWeight(if bold {
                    FontWeight::Regular
                } else {
                    FontWeight::Bold
                }));
            }
        }

        Control::TextAlign => {
            let options = [
                crate::widgets::Segment::icon(Align::Left, Icon::TextAlignLeft, "Left"),
                crate::widgets::Segment::icon(Align::Center, Icon::TextAlignCenter, "Centre"),
                crate::widgets::Segment::icon(Align::Right, Icon::TextAlignRight, "Right"),
            ];
            if let Some(align) = crate::widgets::segmented(ui, palette, &model.align, &options) {
                events.style(StyleEdit::Align(align));
            }
        }

        Control::Lock => {
            // The button says what pressing it will do, which is why it reads the
            // selection rather than showing one fixed padlock. A mixed selection —
            // some locked, some not — offers *Lock*, because that is the state one
            // press can actually reach for all of them.
            let locked = cmd_ctx.all_locked;
            let icon = if locked { Icon::Unlock } else { Icon::Lock };
            if icon_button(ui, palette, icon, CONTROL, locked)
                .on_hover_text(if locked { "Unlock" } else { "Lock" })
                .clicked()
            {
                events.style(StyleEdit::Locked(!locked));
            }
        }

        Control::More => {
            let response = icon_button(ui, palette, Icon::More, CONTROL, false)
                .on_hover_text("More — everything else this selection can do");
            if response.clicked() {
                // Under the button's own left edge, so the menu reads as hanging from
                // it rather than landing wherever the pointer happened to be.
                return Some(pos2(response.rect.left(), response.rect.bottom() + space::UNIT));
            }
        }
    }
    None
}

/// Every colour control in the bar is this size — a wide-ish chip rather than a
/// square, so the colour itself is readable at a glance.
const SWATCH: Vec2 = vec2(space::of(9), CONTROL);

/// What a picker opens on when the selection has no agreed line colour.
const DEFAULT_STROKE: Color = Color::rgb(0x33, 0x33, 0x33);

/// …and no agreed text colour.
const DEFAULT_INK: Color = Color::rgb(0x1A, 0x1D, 0x24);

/// A numeric field, in the numeric face and narrow enough for a bar.
fn number(ui: &mut egui::Ui, drag: egui::DragValue<'_>) -> egui::Response {
    crate::theme::tabular(ui, |ui| {
        ui.add_sized(vec2(space::of(14), CONTROL), drag)
    })
}

/// A number with an icon in front of it saying which number it is.
///
/// *"for the pen small menu please indicate what each number does with an icon."* A pen
/// stroke's bar showed `4.0` beside `100%` with nothing to tell them apart — a hover tooltip
/// is not an answer, because you have to already suspect which one you want before you can
/// hover it. Miro solves the same problem with a *slider* for width, which is self-describing
/// at the cost of far more room than a floating bar has; an icon says the same thing in the
/// width of the icon.
///
/// The icon is painted rather than laid out as a widget so it cannot take the click: the
/// number beside it is a `DragValue`, and a person aiming at the pair should always land on
/// the part that does something.
fn labelled_number(
    ui: &mut egui::Ui,
    palette: Palette,
    icon: Icon,
    drag: egui::DragValue<'_>,
) -> egui::Response {
    let glyph = space::of(4);
    let (box_, _) = ui.allocate_exact_size(Vec2::splat(glyph), egui::Sense::hover());
    icon.paint(&ui.painter().clone(), box_, palette.muted, crate::widgets::ICON_STROKE);
    number(ui, drag)
}

/// The colour picker, hung under whichever swatch opened it.
///
/// **Opaque**, not glass. It opens from the bar, and the bar is already glass over the
/// canvas; `docs/05-design-language.md` §3a forbids stacking the material, so the
/// popover treats the bar as its backing exactly as the panel's picker does.
fn picker(
    palette: Palette,
    state: &mut ContextBarState,
    which: Swatch,
    anchor: &egui::Response,
    events: &mut EventSink,
    edit: impl Fn(Color) -> StyleEdit,
) {
    let mut open = state.open == Some(which);
    if !open {
        return;
    }
    let Some(picker) = state.picker.as_mut() else {
        state.open = None;
        return;
    };

    egui::containers::Popup::from_response(anchor)
        .open_bool(&mut open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::BOTTOM_START)
        .gap(space::of(2))
        .frame(floating_frame_over(palette, Backing::Panel))
        .show(|ui| {
            ui.set_min_width(space::of(56));
            if let Some(color) = picker.ui(ui, palette) {
                events.style(edit(color));
            }
        });

    if !open {
        state.open = None;
        state.picker = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{
        Border, ConnectorSummary, ItemFacet, LinkSummary, SelectionItem, TextSummary,
    };
    use vellum_connect::AnchorSide;
    use vellum_doc::{ItemId, Placement};

    fn id(n: i32) -> ItemId {
        format!("{n}@1").parse().expect("a valid id")
    }

    /// Every item the app describes carries an opacity — `vellum_app::inspect` fills
    /// it in unconditionally — so every fixture here does too. The first version of
    /// these tests left it out, and `an_ink_stroke_gets_exactly_the_four_controls`
    /// passed against a stroke the app would have drawn five controls for. A fixture
    /// that is tidier than production is a test that agrees with nothing.
    fn bare(n: i32, facet: ItemFacet) -> SelectionItem {
        SelectionItem {
            opacity: Some(1.0),
            ..SelectionItem::new(id(n), facet, Placement::new(0.0, 0.0, 100.0, 100.0))
        }
    }

    fn text_summary() -> TextSummary {
        TextSummary {
            content: String::new(),
            family: None,
            size: Some(14.0),
            weight: FontWeight::Regular,
            color: DEFAULT_INK,
            align: Align::Left,
            vertical_align: crate::selection::VerticalAlign::Middle,
            line_height: 1.2,
        }
    }

    fn sticky() -> SelectionItem {
        SelectionItem {
            fill: Some(Some(Color::rgb(0xFF, 0xE0, 0x66))),
            text: Some(text_summary()),
            ..bare(1, ItemFacet::Sticky)
        }
    }

    /// An image: an opacity and nothing else. The one kind the opacity control is for.
    fn image() -> SelectionItem {
        bare(1, ItemFacet::Image)
    }

    /// Ink: a stroke, and the opacity every item carries — which must **not** reach
    /// the bar, because the stroke swatch's own picker has an alpha slider.
    fn ink() -> SelectionItem {
        SelectionItem {
            border: Some(Border { color: DEFAULT_STROKE, width: 4.0, style: LineStyle::Solid }),
            ..bare(1, ItemFacet::Ink)
        }
    }

    fn connector() -> SelectionItem {
        SelectionItem {
            border: Some(Border { color: DEFAULT_STROKE, width: 2.0, style: LineStyle::Solid }),
            connector: Some(ConnectorSummary {
                routing: RoutingMode::Orthogonal,
                start_arrow: Arrowhead::None,
                end_arrow: Arrowhead::FilledTriangle,
                start_anchor: AnchorSide::Right,
                end_anchor: AnchorSide::Left,
            }),
            ..bare(1, ItemFacet::Connector)
        }
    }

    fn card(url: Option<&str>) -> SelectionItem {
        SelectionItem {
            link: Some(LinkSummary {
                url: url.map(str::to_owned),
                provider: None,
                mode: CardMode::Card,
                has_image: false,
            }),
            ..bare(1, ItemFacet::Link)
        }
    }

    fn of(selection: &[SelectionItem]) -> Vec<Control> {
        controls(&PanelModel::derive(selection))
    }

    /// The typeface is on the bar for anything with words, and on nothing else.
    ///
    /// Miro shows it — *"Noto Sans"* — on both the shape bar and the text bar in the user's
    /// own reference screenshots, and in Velm it was reachable only from the docked panel,
    /// which is off by default. The negative half matters as much: a **link card** has words
    /// drawn on it and no typography to change, because its title is fetched metadata rather
    /// than something the panel restyles, so offering a font picker there would be a control
    /// that silently does nothing.
    #[test]
    fn the_typeface_is_offered_wherever_there_are_words_to_set() {
        let words = of(&[sticky()]);
        assert!(words.contains(&Control::FontFamily), "{words:?}");
        let (family, size) = (
            words.iter().position(|c| *c == Control::FontFamily),
            words.iter().position(|c| *c == Control::FontSize),
        );
        assert!(family < size, "the name reads before the number, as it does in Miro");
        for wordless in [of(&[ink()]), of(&[image()]), of(&[card(Some("https://x.test/"))])] {
            assert!(
                !wordless.contains(&Control::FontFamily),
                "nothing here has typography to change: {wordless:?}"
            );
        }
    }

    /// A pen stroke's bar: a colour, a width, an **opacity**, a lock and a `⋮`.
    ///
    /// No fill — ink has none — and no dash, which is a connector's.
    ///
    /// # Why this is five now and was four
    ///
    /// It was four to match a screenshot the user sent of Miro's pen bar. They have since
    /// asked for the fifth outright — *"for the pen strokes you can make it so that there is
    /// also opacity etc"* — after seeing that Miro reaches opacity through the flyout behind
    /// its stroke swatch rather than doing without it. Parity with Miro's *count* was never
    /// the goal; parity with what the hand can reach was.
    ///
    /// **This test passed for the wrong reason once**, and the lesson survives the change:
    /// its fixture had no opacity while the app gives every item one, so it asserted four
    /// against a stroke the app drew five for. A fixture tidier than production agrees with
    /// nothing. `ink()` carries an opacity now for exactly that reason.
    #[test]
    fn an_ink_stroke_gets_a_colour_a_width_and_an_opacity() {
        let bar = of(&[ink()]);
        assert_eq!(
            bar,
            vec![
                Control::Stroke,
                Control::StrokeWidth,
                Control::Opacity,
                Control::Separator,
                Control::Lock,
                Control::More,
            ]
        );
    }

    /// Every selection can be locked and every selection has more to offer than fits,
    /// so these two are the invariant — and they are last, so they never move.
    #[test]
    fn every_selection_ends_with_lock_and_more() {
        for selection in [vec![sticky()], vec![ink()], vec![connector()], vec![card(None)]] {
            let bar = of(&selection);
            assert_eq!(
                &bar[bar.len() - 2..],
                &[Control::Lock, Control::More],
                "bar was {bar:?}"
            );
        }
    }

    /// Nothing selected is no bar at all — not an empty frame floating over the board.
    #[test]
    fn an_empty_selection_draws_nothing() {
        assert!(of(&[]).is_empty());
    }

    /// A sticky has a fill and words; it must not be offered a connector's routing or
    /// a card's display mode.
    #[test]
    fn a_sticky_gets_paint_and_text_and_nothing_else() {
        let bar = of(&[sticky()]);
        assert!(bar.contains(&Control::Fill));
        assert!(bar.contains(&Control::TextColor));
        assert!(bar.contains(&Control::FontSize));
        assert!(bar.contains(&Control::Bold));
        assert!(bar.contains(&Control::TextAlign));
        for absent in [Control::Routing, Control::LineStyle, Control::CardMode, Control::EndArrow] {
            assert!(!bar.contains(&absent), "{absent:?} does not apply to a sticky");
        }
    }

    /// The dash and the routing belong to wire alone. Ink has a stroke too, and giving
    /// it a dash control would offer a property `vellum-ink` cannot honour.
    #[test]
    fn only_a_connector_gets_routing_and_a_dash() {
        let wire = of(&[connector()]);
        assert!(wire.contains(&Control::LineStyle));
        assert!(wire.contains(&Control::Routing));
        assert!(wire.contains(&Control::EndArrow));

        let stroke = of(&[ink()]);
        assert!(!stroke.contains(&Control::LineStyle));
        assert!(!stroke.contains(&Control::Routing));
    }

    /// *Open page* needs an address, exactly as the context menu's row does.
    #[test]
    fn a_card_offers_its_page_only_when_it_has_one() {
        assert!(of(&[card(Some("https://example.com"))]).contains(&Control::OpenPage));
        assert!(!of(&[card(None)]).contains(&Control::OpenPage));
        assert!(of(&[card(None)]).contains(&Control::CardMode), "the mode still applies");
    }

    /// A separator with nothing on one side of it is a stray line. This is the failure
    /// the band arithmetic in `controls` exists to prevent, and it is easy to
    /// reintroduce by adding a band without its guard.
    #[test]
    fn no_separator_ever_leads_or_doubles() {
        for selection in [
            vec![sticky()],
            vec![ink()],
            vec![connector()],
            vec![card(Some("https://example.com"))],
            vec![sticky(), ink()],
            vec![sticky(), connector(), card(None)],
        ] {
            let bar = of(&selection);
            assert_ne!(bar.first(), Some(&Control::Separator), "leading rule in {bar:?}");
            assert_ne!(bar.last(), Some(&Control::Separator), "trailing rule in {bar:?}");
            for pair in bar.windows(2) {
                assert!(
                    pair != [Control::Separator, Control::Separator],
                    "two rules together in {bar:?}"
                );
            }
        }
    }

    /// Opacity is offered to everything **except a fill**, and the exception is the point.
    ///
    /// An image has neither fill nor stroke, so opacity is the only thing on its bar that can
    /// change how it is painted. A stroked item — ink, a connector — gets it too, at the
    /// user's request. A **filled** item does not: a fill swatch's own picker carries an
    /// alpha slider, so a second control would be two widgets for one property, disagreeing
    /// the moment either is used.
    #[test]
    fn opacity_is_offered_to_everything_but_a_fill() {
        for reachable in [of(&[image()]), of(&[ink()]), of(&[connector()])] {
            assert!(
                reachable.contains(&Control::Opacity),
                "there is no swatch here, so the bar is the only way to alpha: {reachable:?}"
            );
        }
        let filled = of(&[sticky()]);
        assert!(
            !filled.contains(&Control::Opacity),
            "the fill swatch's picker already carries alpha: {filled:?}"
        );
    }

    /// A mixed selection folds to whatever the members share, and still gets a bar —
    /// which is the case that would otherwise fall through to nothing at all.
    #[test]
    fn a_mixed_selection_still_gets_a_bar() {
        let bar = of(&[sticky(), ink()]);
        assert!(bar.contains(&Control::Fill), "the sticky's fill survives the fold");
        assert!(bar.contains(&Control::Stroke), "so does the stroke");
        assert!(bar.contains(&Control::More));
    }
}
