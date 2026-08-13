//! The right-hand properties panel.
//!
//! Every control reads a [`Field`] from the [`PanelModel`] and writes a
//! [`StyleEdit`], so the mixed-value rule is applied in one place —
//! `crate::selection` — rather than re-decided per control.
//!
//! # The panel is not drawn when nothing is selected
//!
//! It used to stay on screen and say "Nothing selected", and the reasoning recorded
//! here was that a panel which comes and goes changes the canvas rectangle on every
//! click. That reasoning was sound but it answered the wrong question: the user's
//! verdict was *"i dont need this panels i dont even understand the purpose of it"*,
//! and Miro — whose ergonomics `docs/04-ui-reference.md` exists to match — has no
//! docked properties panel at all. It floats a small toolbar above the selection.
//! Spending 250px of board permanently to report that there is nothing to report is
//! the worst trade available on a canvas app.
//!
//! **The flicker concern was real, though, and is not yet fully solved.**
//! `Camera::world_to_screen` centres on `viewport.width / 2.0`, so widening the
//! canvas by `PROPERTIES_WIDTH` shifts every item right by half that. Whoever wires
//! the next step must compensate: on a canvas-width change of `dw`, move the camera
//! centre by `dw / 2 / zoom` so the board stays put under the pointer.
//!
//! The proper end state is a **floating** panel over the canvas rather than a docked
//! one that shrinks it. Then the canvas rectangle never changes, the camera never
//! needs compensating, and the layout matches Miro. That is the next move here.

use crate::color::{ColorPicker, to_egui};
use crate::command::{Command, CommandContext};
use crate::event::{EventSink, StyleEdit, TransformEdit, UiEvent};
use crate::icon::Icon;
use crate::selection::{Field, FontWeight, PanelModel, VerticalAlign};
use crate::theme::{
    Backing, PROPERTIES_WIDTH, Palette, floating_frame_over, numeric, panel_title, space, tabular,
};
use crate::widgets::{
    CONTROL_WIDTH, Segment, hairline, icon_button, mixed_placeholder, row, section_header,
    segmented, swatch,
};
use egui::{PopupCloseBehavior, RectAlign, Stroke, Ui, containers::Popup, vec2};
use vellum_connect::{AnchorSide, Arrowhead, LineStyle, RoutingMode};
use vellum_doc::{Align, CardMode, Color};

/// Which swatch has the colour picker open under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorTarget {
    Fill,
    Border,
    Text,
}

/// The panel's own memory between frames.
#[derive(Debug, Default)]
pub struct PropertiesState {
    open: Option<ColorTarget>,
    picker: Option<ColorPicker>,
    /// What the text field holds, and which item it holds it for.
    ///
    /// The field cannot read straight from the model: the model is rebuilt from the
    /// document, the document is written from the field, and a round trip through a
    /// CRDT is a frame long — so binding them directly makes the caret jump to the
    /// end on every keystroke. This is the buffer that breaks the loop, and the id is
    /// what makes selecting a different item reload it.
    text: Option<(vellum_doc::ItemId, String)>,
    /// Whether the text field had the keyboard on the last frame. See [`words`].
    text_focused: bool,
    /// Set by [`crate::Chrome::focus_text`] when the app wants the cursor put in the
    /// text field — a double click on the canvas, or a note that has just been
    /// placed. Consumed on the next frame.
    focus_text: bool,
    /// The agent section's own free-text buffers — a role label, a working directory, a
    /// model name, a browser address. Same reason [`Self::text`] exists: a field bound
    /// straight to the model sends the caret to the end on every keystroke.
    agent: crate::agent_panel::AgentPanelState,
}

impl PropertiesState {
    /// Puts the cursor in the text field on the next frame.
    pub(crate) fn focus_text(&mut self) {
        self.focus_text = true;
    }

    /// Withdraws a pending focus request.
    ///
    /// The caret now lives on the canvas, so the panel's field must not also claim it:
    /// two carets competing for one keystroke means the one the user is looking at loses
    /// about half the time. Only the *request* is cleared — a field that already has focus
    /// is egui's to hold, and `Chrome::shortcuts`' `egui_wants_keyboard_input` guard is
    /// what keeps the two from both firing.
    pub(crate) fn release_text_focus(&mut self) {
        self.focus_text = false;
    }

    /// Opens the picker on `target`, seeded with the value the swatch was showing.
    fn open_picker(&mut self, target: ColorTarget, seed: Color, alpha: bool) {
        if self.open == Some(target) {
            self.open = None;
            return;
        }
        self.open = Some(target);
        self.picker = Some(ColorPicker::new(seed, alpha));
    }
}

/// The eight terminators, in the order the dropdown lists them.
const ARROWHEADS: [Arrowhead; 8] = [
    Arrowhead::None,
    Arrowhead::LineArrow,
    Arrowhead::FilledTriangle,
    Arrowhead::OpenTriangle,
    Arrowhead::Circle,
    Arrowhead::FilledCircle,
    Arrowhead::Diamond,
    Arrowhead::FilledDiamond,
];

const fn arrowhead_label(head: Arrowhead) -> &'static str {
    match head {
        Arrowhead::None => "None",
        Arrowhead::LineArrow => "Line arrow",
        Arrowhead::FilledTriangle => "Filled triangle",
        Arrowhead::OpenTriangle => "Open triangle",
        Arrowhead::Circle => "Circle",
        Arrowhead::FilledCircle => "Filled circle",
        Arrowhead::Diamond => "Diamond",
        Arrowhead::FilledDiamond => "Filled diamond",
    }
}

/// The board's default fill, used to seed a picker opened on a selection that has no
/// agreed colour.
const SEED_FILL: Color = crate::color::DEFAULT_FILL;

pub(crate) fn show(
    ui: &mut Ui,
    palette: Palette,
    state: &mut PropertiesState,
    model: &PanelModel,
    cmd_ctx: &CommandContext,
    font_families: &[String],
    events: &mut EventSink,
) -> egui::Rect {
    // Nothing selected, nothing to inspect — so the panel is not drawn at all, and
    // the canvas gets its width back.
    //
    // Miro has no docked properties panel: it floats a small toolbar above the
    // selected object, which appears where the attention already is and leaves when
    // the selection does. A permanent 250px column that says "Nothing selected" is
    // the worst trade available on a canvas app — it spends board space to report
    // that it has nothing to say. The user put it plainly: *"i dont need this panels
    // i dont even understand the purpose of it"*.
    //
    // Returning an empty rect at the right edge keeps the caller's arithmetic honest:
    // `Chrome` subtracts this from the canvas rect, so an empty rect means the canvas
    // simply extends to the window edge.
    if model.is_empty() {
        let available = ui.max_rect();
        return egui::Rect::from_min_max(available.right_top(), available.right_bottom());
    }

    let frame = egui::Frame::new()
        .fill(palette.surface)
        .inner_margin(egui::Margin::symmetric(space::of(3) as i8, space::of(2) as i8))
        .stroke(Stroke::NONE);

    egui::Panel::right("vellum-properties")
        .frame(frame)
        .exact_size(PROPERTIES_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .show(ui, |ui| {
            // The panel's own left edge, drawn as a hairline in the border colour —
            // one line and a small luminance step, which is all §3 allows a panel to
            // be distinguished by.
            let edge = ui.max_rect();
            ui.painter().vline(
                edge.left() - space::of(3),
                edge.y_range(),
                palette.hairline_stroke(),
            );

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                header(ui, palette, model, events);
                if model.is_empty() {
                    empty_state(ui, palette);
                    return;
                }
                if model.has_appearance() {
                    appearance(ui, palette, state, model, events);
                }
                if model.has_text() {
                    typography(ui, palette, state, model, font_families, events);
                }
                if model.has_connector() {
                    connector(ui, palette, model, events);
                }
                if model.has_link() {
                    link(ui, palette, model, events);
                }
                // The Agent Canvas section — an agent's provider, rules and schedule, or a
                // note's, a file tree's or a browser node's own handful. Drawn from the same
                // model as everything above it, and absent for a board that has none of
                // them: `docs/07-agent-canvas.md` §0's second rule, applied to the chrome.
                if model.has_agent_family() {
                    crate::agent_panel::show(
                        ui,
                        palette,
                        &mut state.agent,
                        model,
                        cmd_ctx,
                        events,
                    );
                }
                arrange(ui, palette, cmd_ctx, events);
                geometry(ui, palette, model, events);
            });
        })
        .response
        .rect
}

fn header(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    ui.horizontal(|ui| {
        ui.label(panel_title(&model.headline).color(palette.text));
        if model.is_empty() {
            return;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let locked = model.all_locked();
            let icon = if locked { Icon::Lock } else { Icon::Unlock };
            if icon_button(ui, palette, icon, space::of(6), locked)
                .on_hover_text(if locked { "Unlock" } else { "Lock" })
                .clicked()
            {
                events.style(StyleEdit::Locked(!locked));
            }
        });
    });
    ui.add_space(space::UNIT);
    hairline(ui, palette);
}

/// What the panel shows with nothing selected.
///
/// Left-aligned and small, with the mark set at the size it is legible at. Not a
/// centred hero: `docs/05-design-language.md` §2 rules that out by name, and a panel
/// that greets you in the middle of itself every time you deselect is a panel you
/// stop reading.
fn empty_state(ui: &mut Ui, palette: Palette) {
    ui.add_space(space::of(4));
    ui.horizontal(|ui| {
        let size = crate::mark::MINIMUM_SIZE;
        let (rect, _) =
            ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
        crate::mark::paint_mono(&ui.painter().clone(), rect, palette.faint);
        ui.add_space(crate::mark::clear_space(size));
        // **Not "Nothing selected" again.** The panel's own headline is already that
        // string — `crate::selection::headline` returns it for an empty selection —
        // and printing it twice, one line under the other, reads as a rendering
        // fault. This line says what to do instead.
        ui.label(
            egui::RichText::new("Pick an object on the board")
                .color(palette.muted),
        );
    });
    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new("Its colour, text, size and position appear here.")
            .color(palette.faint),
    );
}

fn appearance(
    ui: &mut Ui,
    palette: Palette,
    state: &mut PropertiesState,
    model: &PanelModel,
    events: &mut EventSink,
) {
    section_header(ui, palette, "Appearance");

    if !model.fill.is_absent() {
        row(ui, palette, "Fill", |ui| {
            let shown = match &model.fill {
                Field::Uniform(color) => color.map(to_egui),
                _ => None,
            };
            let response = swatch(ui, palette, shown, vec2(28.0, 22.0), false);
            if response.clicked() {
                let seed = model.fill.value().copied().flatten().unwrap_or(SEED_FILL);
                state.open_picker(ColorTarget::Fill, seed, true);
            }
            picker_popup(palette, state, ColorTarget::Fill, &response, events, |color| {
                StyleEdit::Fill(Some(color))
            });

            if model.fill.is_mixed() {
                mixed_placeholder(ui, palette);
            } else if ui
                .add(egui::Button::new("No fill").small())
                .on_hover_text("Remove the fill entirely")
                .clicked()
            {
                events.style(StyleEdit::Fill(None));
            }
        });
    }

    if !model.border_color.is_absent() {
        row(ui, palette, "Border", |ui| {
            let shown = model.border_color.value().copied().map(to_egui);
            let response = swatch(ui, palette, shown, vec2(28.0, 22.0), false);
            if response.clicked() {
                let seed = model.border_color.or(Color::rgb(0x33, 0x33, 0x33));
                state.open_picker(ColorTarget::Border, seed, true);
            }
            picker_popup(palette, state, ColorTarget::Border, &response, events, |color| {
                StyleEdit::BorderColor(color)
            });

            let mut width = model.border_width.or(1.0);
            let drag = tabular(ui, |ui| {
                ui.add_sized(
                    vec2(space::of(14), ui.spacing().interact_size.y),
                    egui::DragValue::new(&mut width)
                        .speed(0.1)
                        .range(0.0..=64.0)
                        .max_decimals(1)
                        .suffix(" px"),
                )
            });
            if drag.changed() {
                events.style(StyleEdit::BorderWidth(width));
            }
            if model.border_width.is_mixed() {
                drag.on_hover_text(crate::widgets::MIXED);
            }

            if icon_button(ui, palette, Icon::Close, space::of(5), false)
                .on_hover_text("No border")
                .clicked()
            {
                events.style(StyleEdit::BorderCleared);
            }
        });

        row(ui, palette, "Line", |ui| {
            let options = [
                Segment::text(LineStyle::Solid, "Solid"),
                Segment::text(LineStyle::Dashed, "Dashed"),
                Segment::text(LineStyle::Dotted, "Dotted"),
            ];
            if let Some(style) = segmented(ui, palette, &model.border_style, &options) {
                events.style(StyleEdit::BorderStyle(style));
            }
        });
    }

    if !model.opacity.is_absent() {
        row(ui, palette, "Opacity", |ui| {
            let mut percent = model.opacity.or(1.0) * 100.0;
            let slider = tabular(ui, |ui| {
                ui.add(
                    egui::Slider::new(&mut percent, 0.0..=100.0)
                        .suffix("%")
                        .fixed_decimals(0)
                        .trailing_fill(true),
                )
            });
            if slider.changed() {
                events.style(StyleEdit::Opacity(percent / 100.0));
            }
            if model.opacity.is_mixed() {
                slider.on_hover_text(crate::widgets::MIXED);
            }
        });
    }
}

fn typography(
    ui: &mut Ui,
    palette: Palette,
    state: &mut PropertiesState,
    model: &PanelModel,
    font_families: &[String],
    events: &mut EventSink,
) {
    section_header(ui, palette, "Text");
    words(ui, state, model, events);

    row(ui, palette, "Font", |ui| {
        let current = match &model.font_family {
            Field::Uniform(Some(name)) => name.clone(),
            Field::Uniform(None) => "Default".to_owned(),
            Field::Mixed => crate::widgets::MIXED.to_owned(),
            Field::Absent => return,
        };
        egui::ComboBox::from_id_salt("vellum-font-family")
            .selected_text(current)
            .width(CONTROL_WIDTH - 24.0)
            .show_ui(ui, |ui| {
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
    });

    row(ui, palette, "Size", |ui| {
        // Auto-fit is `None`, matching Miro's `fs: 0, fsa: 1`, and it is a mode
        // rather than a size — hence a checkbox beside the field rather than a
        // magic value inside it.
        let auto = matches!(model.font_size, Field::Uniform(None));
        let mut size = model.font_size.value().copied().flatten().unwrap_or(14.0);
        let drag = ui
            .add_enabled_ui(!auto, |ui| {
                tabular(ui, |ui| {
                    ui.add_sized(
                        vec2(space::of(15), ui.spacing().interact_size.y),
                        egui::DragValue::new(&mut size)
                            .speed(0.5)
                            .range(4.0..=400.0)
                            .max_decimals(1),
                    )
                })
            })
            .inner;
        if drag.changed() {
            events.style(StyleEdit::FontSize(Some(size)));
        }
        let mut auto_now = auto;
        if ui
            .checkbox(&mut auto_now, "Auto")
            .on_hover_text("Shrink the text to fit its box")
            .changed()
        {
            events.style(StyleEdit::FontSize(if auto_now { None } else { Some(size) }));
        }
    });

    row(ui, palette, "Weight", |ui| {
        let options: Vec<Segment<FontWeight>> =
            FontWeight::ALL.iter().map(|w| Segment::text(*w, w.label())).collect();
        if let Some(weight) = segmented(ui, palette, &model.font_weight, &options) {
            events.style(StyleEdit::FontWeight(weight));
        }
    });

    row(ui, palette, "Colour", |ui| {
        let shown = model.text_color.value().copied().map(to_egui);
        let response = swatch(ui, palette, shown, vec2(28.0, 22.0), false);
        if response.clicked() {
            let seed = model.text_color.or(Color::rgb(0x1A, 0x1D, 0x24));
            state.open_picker(ColorTarget::Text, seed, false);
        }
        picker_popup(palette, state, ColorTarget::Text, &response, events, |color| {
            StyleEdit::TextColor(color)
        });
        if model.text_color.is_mixed() {
            mixed_placeholder(ui, palette);
        }
    });

    row(ui, palette, "Align", |ui| {
        let options = [
            Segment::icon(Align::Left, Icon::TextAlignLeft, "Left"),
            Segment::icon(Align::Center, Icon::TextAlignCenter, "Centre"),
            Segment::icon(Align::Right, Icon::TextAlignRight, "Right"),
        ];
        if let Some(align) = segmented(ui, palette, &model.align, &options) {
            events.style(StyleEdit::Align(align));
        }
    });

    row(ui, palette, "Vertical", |ui| {
        let options = [
            Segment::icon(VerticalAlign::Top, Icon::AlignTop, VerticalAlign::Top.label()),
            Segment::icon(
                VerticalAlign::Middle,
                Icon::AlignMiddleVertical,
                VerticalAlign::Middle.label(),
            ),
            Segment::icon(VerticalAlign::Bottom, Icon::AlignBottom, VerticalAlign::Bottom.label()),
        ];
        if let Some(align) = segmented(ui, palette, &model.vertical_align, &options) {
            events.style(StyleEdit::VerticalAlign(align));
        }
    });

    row(ui, palette, "Line height", |ui| {
        let mut height = model.line_height.or(1.2);
        let drag = tabular(ui, |ui| {
            ui.add(
                egui::DragValue::new(&mut height)
                    .speed(0.01)
                    .range(0.6..=4.0)
                    .fixed_decimals(2)
                    .suffix("×"),
            )
        });
        if drag.changed() {
            events.style(StyleEdit::LineHeight(height));
        }
        if model.line_height.is_mixed() {
            drag.on_hover_text(crate::widgets::MIXED);
        }
    });
}

/// The words themselves.
///
/// The one control in this panel that is not a *property*: it is the content. It is
/// here because it is the only place a sticky's text can be typed at all — there is
/// no caret on the canvas yet (`docs/features/README.md` §3 puts rich-text editing at
/// P4) and a note that can never say anything is not a note.
///
/// Single selection only. `PanelModel::text_content` explains why.
fn words(
    ui: &mut Ui,
    state: &mut PropertiesState,
    model: &PanelModel,
    events: &mut EventSink,
) {
    let (Some(id), Some(current)) = (model.single_id, model.text_content.as_ref()) else {
        state.text = None;
        return;
    };

    // Reload when the selection changed, or when the document moved on underneath —
    // an undo, a find, another edit path — while nobody is typing into the field.
    // Reloading *during* typing is what makes a caret jump to the end on every
    // keystroke: the document is one frame behind the buffer by construction.
    let reload = match &state.text {
        Some((held, buffer)) => *held != id || (!state.text_focused && buffer != current),
        None => true,
    };
    if reload {
        state.text = Some((id, current.clone()));
    }
    let Some((_, buffer)) = state.text.as_mut() else { return };

    let response = ui.add(
        egui::TextEdit::multiline(buffer)
            .id_salt("vellum-item-text")
            .desired_width(f32::INFINITY)
            .desired_rows(3)
            .hint_text("Type here"),
    );
    if std::mem::take(&mut state.focus_text) {
        response.request_focus();
    }
    if response.changed() {
        events.push(UiEvent::TextEdited(buffer.clone()));
    }
    state.text_focused = response.has_focus();
    ui.add_space(space::UNIT);
}

fn connector(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    section_header(ui, palette, "Connector");

    row(ui, palette, "Route", |ui| {
        let options = [
            Segment::text(RoutingMode::Straight, "Straight"),
            Segment::text(RoutingMode::Curved, "Curved"),
            Segment::text(RoutingMode::Orthogonal, "Elbow"),
        ];
        if let Some(routing) = segmented(ui, palette, &model.routing, &options) {
            events.style(StyleEdit::Routing(routing));
        }
    });

    arrow_row(ui, palette, "Start", &model.start_arrow, events, StyleEdit::StartArrow);
    arrow_row(ui, palette, "End", &model.end_arrow, events, StyleEdit::EndArrow);

    anchor_row(ui, palette, "From", &model.start_anchor, events, StyleEdit::StartAnchor);
    anchor_row(ui, palette, "To", &model.end_anchor, events, StyleEdit::EndAnchor);
}

/// The anchor picker: which point of the attached item this end is tied to.
///
/// The only way to *choose* an attachment. Drawing a connector computes the edge facing
/// the other end, which is right almost always and cannot be argued with — there was no
/// way to insist on the top edge of a box that happens to be to the left, and no way to
/// reach the centre attachment at all, which imported connectors use and a drag
/// deliberately never produces.
///
/// An unattached end shows *Unattached* and offers nothing: an anchor on a free end is a
/// fraction of the connector's own box, so picking "Left" for one would move the endpoint
/// somewhere nobody asked for. Same for the *Custom* fractions Miro's own exports carry —
/// shown so the panel does not claim the anchor is an edge when it is not, and not
/// offered because it cannot be re-chosen once left.
fn anchor_row(
    ui: &mut Ui,
    palette: Palette,
    label: &str,
    field: &Field<AnchorSide>,
    events: &mut EventSink,
    edit: fn(AnchorSide) -> StyleEdit,
) {
    row(ui, palette, label, |ui| {
        let current = match field {
            Field::Uniform(side) => side.label(),
            Field::Mixed => crate::widgets::MIXED,
            Field::Absent => return,
        };
        let enabled = field.value().is_none_or(|side| side.is_pickable());
        let combo = egui::ComboBox::from_id_salt(("vellum-anchor", label))
            .selected_text(current)
            .width(CONTROL_WIDTH - 24.0);
        ui.add_enabled_ui(enabled, |ui| {
            combo.show_ui(ui, |ui| {
                for side in AnchorSide::PICKABLE {
                    let selected = field.value().is_some_and(|s| *s == side);
                    if ui.selectable_label(selected, side.label()).clicked() {
                        events.style(edit(side));
                    }
                }
            });
        });
        if !enabled {
            ui.label("").on_hover_text(match field.value() {
                Some(AnchorSide::Free) => "This end is pinned to the canvas, not to an item",
                _ => "An imported anchor that is not one of the five named points",
            });
        }
    });
}

/// A link card's own controls: how much of it to draw, and where it points.
///
/// The mode is the one property a card really has — Miro puts the same switch in the card's
/// floating toolbar — and *Open* is what a card is for. Before this, a selected card reported
/// itself as an `Image` and the panel offered it a picture's controls, so neither was reachable.
fn link(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    section_header(ui, palette, "Link");

    row(ui, palette, "Show", |ui| {
        let options = [
            Segment::text(CardMode::Link, "Row"),
            Segment::text(CardMode::Card, "Card"),
            Segment::text(CardMode::Large, "Large"),
        ];
        if let Some(mode) = segmented(ui, palette, &model.card_mode, &options) {
            events.style(StyleEdit::CardMode(mode));
        }
    });

    // Said rather than left to be discovered: *Large* is the only mode that draws the preview
    // image, so choosing it before anything has been fetched looks like a mode that does
    // nothing. `link_has_image` is what distinguishes "no image yet" from "no image at all".
    if matches!(model.card_mode.value(), Some(CardMode::Large))
        && model.link_has_image.value() == Some(&false)
    {
        row(ui, palette, "", |ui| {
            ui.label(
                egui::RichText::new("No preview image fetched yet")
                    .size(crate::theme::text::LABEL)
                    .color(palette.muted),
            );
        });
    }

    // Single selection only. *Open* acts on one page, and forty tabs is not what anybody means
    // by clicking it once.
    row(ui, palette, "Page", |ui| {
        match model.link_url.clone() {
            Some(url) => {
                if ui
                    .add(egui::Button::new("Open").small())
                    .on_hover_text(url.clone())
                    .clicked()
                {
                    events.push(UiEvent::OpenLink(url));
                }
            }
            None => {
                ui.add_enabled(false, egui::Button::new("Open").small()).on_disabled_hover_text(
                    if model.count > 1 {
                        "Select one card to open its page"
                    } else {
                        "This card has no address recorded"
                    },
                );
            }
        }
    });
}

fn arrow_row(
    ui: &mut Ui,
    palette: Palette,
    label: &str,
    field: &Field<Arrowhead>,
    events: &mut EventSink,
    edit: fn(Arrowhead) -> StyleEdit,
) {
    row(ui, palette, label, |ui| {
        let current = match field {
            Field::Uniform(head) => arrowhead_label(*head),
            Field::Mixed => crate::widgets::MIXED,
            Field::Absent => return,
        };
        egui::ComboBox::from_id_salt(("vellum-arrowhead", label))
            .selected_text(current)
            .width(CONTROL_WIDTH - 24.0)
            .show_ui(ui, |ui| {
                for head in ARROWHEADS {
                    let selected = field.value().is_some_and(|h| *h == head);
                    if ui.selectable_label(selected, arrowhead_label(head)).clicked() {
                        events.style(edit(head));
                    }
                }
            });
    });
}

fn arrange(ui: &mut Ui, palette: Palette, cmd_ctx: &CommandContext, events: &mut EventSink) {
    section_header(ui, palette, "Arrange");

    for (label, size, commands) in [
        (
            "Order",
            26.0,
            &[
                Command::SendToBack,
                Command::SendBackward,
                Command::BringForward,
                Command::BringToFront,
            ][..],
        ),
        ("Group", 26.0, &[Command::Group, Command::Ungroup]),
        (
            "Align",
            24.0,
            &[
                Command::AlignLeft,
                Command::AlignCenterHorizontal,
                Command::AlignRight,
                Command::AlignTop,
                Command::AlignMiddleVertical,
                Command::AlignBottom,
            ],
        ),
        (
            "Distribute",
            26.0,
            &[Command::DistributeHorizontally, Command::DistributeVertically],
        ),
    ] {
        row(ui, palette, label, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for command in commands {
                command_button(ui, palette, *command, cmd_ctx, size, events);
            }
        });
    }
}

/// A command as an icon button, which says why when it cannot act.
///
/// The reason matters more here than in a menu: an icon has no words of its own, so a
/// greyed square with no explanation is the least informative control in the chrome.
/// egui shows `on_hover_text` only on enabled widgets and `on_disabled_hover_text`
/// only on disabled ones, so both are needed to have a tooltip in either state.
fn command_button(
    ui: &mut Ui,
    palette: Palette,
    command: Command,
    cmd_ctx: &CommandContext,
    size: f32,
    events: &mut EventSink,
) {
    let Some(icon) = command.icon() else { return };
    let available = command.availability(cmd_ctx);
    let response = ui
        .add_enabled_ui(available.is_enabled(), |ui| {
            icon_button(ui, palette, icon, size, false)
        })
        .inner;
    let response = match available.reason() {
        Some(why) => response.on_disabled_hover_text(format!("{} — {why}", command.label())),
        None => response.on_hover_text(command.label()),
    };
    if response.clicked() {
        events.command(command);
    }
}

fn geometry(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    let Some(bounds) = model.bounds else { return };
    section_header(ui, palette, "Position and size");

    // The four fields this design language cares most about. A board coordinate on the
    // reference import runs to five digits, it changes every frame while an object is
    // dragged, and in a proportional face the field would breathe in and out as the
    // digits change. Monospace pins it.
    let field = |ui: &mut Ui, label: &str, mut value: f64, enabled: bool| -> Option<f64> {
        let mut committed = None;
        ui.horizontal(|ui| {
            ui.label(numeric(label).color(palette.muted));
            let drag = tabular(ui, |ui| {
                ui.add_enabled(
                    enabled,
                    egui::DragValue::new(&mut value)
                        .speed(1.0)
                        .max_decimals(1)
                        .min_decimals(0),
                )
            });
            if drag.changed() {
                committed = Some(value);
            }
        });
        committed
    };

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::of(2);
        ui.vertical(|ui| {
            if let Some(x) = field(ui, "X", bounds.x, true) {
                events.push(UiEvent::Transform(TransformEdit::X(x)));
            }
            if let Some(w) = field(ui, "W", bounds.width, model.size_editable) {
                events.push(UiEvent::Transform(TransformEdit::Width(w)));
            }
        });
        ui.vertical(|ui| {
            if let Some(y) = field(ui, "Y", bounds.y, true) {
                events.push(UiEvent::Transform(TransformEdit::Y(y)));
            }
            if let Some(h) = field(ui, "H", bounds.height, model.size_editable) {
                events.push(UiEvent::Transform(TransformEdit::Height(h)));
            }
        });
    });

    row(ui, palette, "Rotation", |ui| {
        let mut rotation = model.rotation.or(0.0);
        let drag = tabular(ui, |ui| {
            ui.add(
                egui::DragValue::new(&mut rotation)
                    .speed(0.5)
                    .max_decimals(1)
                    .suffix("°"),
            )
        });
        if drag.changed() {
            events.push(UiEvent::Transform(TransformEdit::Rotation(rotation)));
        }
        if model.rotation.is_mixed() {
            drag.on_hover_text(crate::widgets::MIXED);
        }
    });
    ui.add_space(space::of(3));
}

/// The colour picker, shown under whichever swatch opened it.
fn picker_popup(
    palette: Palette,
    state: &mut PropertiesState,
    target: ColorTarget,
    anchor: &egui::Response,
    events: &mut EventSink,
    edit: impl Fn(Color) -> StyleEdit,
) {
    let mut open = state.open == Some(target);
    if !open {
        return;
    }
    let Some(picker) = state.picker.as_mut() else {
        state.open = None;
        return;
    };

    Popup::from_response(anchor)
        .open_bool(&mut open)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(RectAlign::LEFT_START)
        .gap(space::of(2))
        // Opaque. The picker opens from a docked panel, and glass over a panel is
        // glass stacked on glass, which `docs/05-design-language.md` §3a rules out.
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
        Border, ConnectorSummary, ItemFacet, SelectionItem, TextSummary,
    };
    use crate::color::from_egui;
    use crate::theme::Theme;
    use egui::Context;
    use vellum_doc::{ItemId, Placement};

    fn id(n: i32) -> ItemId {
        format!("{n}@1").parse().unwrap()
    }

    fn sticky(n: i32, fill: Color) -> SelectionItem {
        SelectionItem {
            fill: Some(Some(fill)),
            opacity: Some(1.0),
            border: Some(Border {
                color: Color::rgb(0, 0, 0),
                width: 1.0,
                style: LineStyle::Solid,
            }),
            text: Some(TextSummary {
                content: "radiator fan".to_owned(),
                family: Some("Noto Sans".to_owned()),
                size: Some(14.0),
                weight: FontWeight::Regular,
                color: Color::rgb(0, 0, 0),
                align: Align::Center,
                vertical_align: VerticalAlign::Middle,
                line_height: 1.2,
            }),
            ..SelectionItem::new(id(n), ItemFacet::Sticky, Placement::new(0.0, 0.0, 200.0, 200.0))
        }
    }

    fn wire(n: i32) -> SelectionItem {
        SelectionItem {
            border: Some(Border {
                color: Color::rgb(0x33, 0x33, 0x33),
                width: 2.0,
                style: LineStyle::Solid,
            }),
            connector: Some(ConnectorSummary {
                routing: RoutingMode::Straight,
                start_arrow: Arrowhead::None,
                end_arrow: Arrowhead::FilledTriangle,
                start_anchor: AnchorSide::Right,
                end_anchor: AnchorSide::Left,
            }),
            ..SelectionItem::new(
                id(n),
                ItemFacet::Connector,
                Placement::new(0.0, 0.0, 100.0, 10.0),
            )
        }
    }

    fn run(selection: &[SelectionItem], state: &mut PropertiesState) -> Vec<UiEvent> {
        let ctx = Context::default();
        crate::theme::apply(&ctx, Theme::Light);
        let model = PanelModel::derive(selection);
        let cmd_ctx = CommandContext {
            board_open: true,
            selected: selection.len(),
            ..CommandContext::default()
        };
        let families = vec!["Noto Sans".to_owned(), "Inter".to_owned()];
        let mut events = EventSink::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = show(ui, Palette::LIGHT, state, &model, &cmd_ctx, &families, &mut events);
        });
        events.take()
    }

    #[test]
    fn the_panel_renders_for_every_selection_shape_without_emitting_anything() {
        let mut state = PropertiesState::default();
        assert!(run(&[], &mut state).is_empty(), "empty selection");
        assert!(run(&[sticky(1, SEED_FILL)], &mut state).is_empty(), "one sticky");
        assert!(
            run(&[sticky(1, SEED_FILL), sticky(2, Color::rgb(0xFF, 0x9E, 0x9E))], &mut state)
                .is_empty(),
            "mixed fills"
        );
        assert!(run(&[wire(3)], &mut state).is_empty(), "one connector");
        assert!(run(&[sticky(1, SEED_FILL), wire(3)], &mut state).is_empty(), "mixed kinds");
    }

    /// The picker has to survive being opened on a mixed selection, where there is
    /// no colour to seed it from.
    #[test]
    fn opening_the_picker_on_a_mixed_fill_seeds_it_with_the_board_default() {
        let mut state = PropertiesState::default();
        state.open_picker(ColorTarget::Fill, SEED_FILL, true);
        assert_eq!(state.open, Some(ColorTarget::Fill));
        assert_eq!(state.picker.as_ref().map(ColorPicker::color), Some(SEED_FILL));

        let events =
            run(&[sticky(1, SEED_FILL), sticky(2, Color::rgb(0xFF, 0x9E, 0x9E))], &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn clicking_the_same_swatch_twice_closes_the_picker() {
        let mut state = PropertiesState::default();
        state.open_picker(ColorTarget::Fill, SEED_FILL, true);
        state.open_picker(ColorTarget::Fill, SEED_FILL, true);
        assert_eq!(state.open, None);
    }

    #[test]
    fn opening_a_second_swatch_moves_the_picker_rather_than_stacking_it() {
        let mut state = PropertiesState::default();
        state.open_picker(ColorTarget::Fill, SEED_FILL, true);
        state.open_picker(ColorTarget::Text, Color::rgb(0, 0, 0), false);
        assert_eq!(state.open, Some(ColorTarget::Text));
        assert_eq!(state.picker.as_ref().map(ColorPicker::color), Some(Color::rgb(0, 0, 0)));
    }

    #[test]
    fn arrowhead_labels_cover_every_variant_the_dropdown_offers() {
        for head in ARROWHEADS {
            assert!(!arrowhead_label(head).is_empty());
        }
        let mut labels: Vec<_> = ARROWHEADS.iter().map(|h| arrowhead_label(*h)).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), ARROWHEADS.len(), "two heads share a label");
    }

    #[test]
    fn opaque_colours_survive_the_round_trip_through_egui() {
        let color = Color::rgb(9, 8, 7);
        assert_eq!(from_egui(to_egui(color)), color);
    }
}
