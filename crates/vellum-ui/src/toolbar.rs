//! The left tool palette and its two flyouts.
//!
//! Floating rather than docked, like Miro's: the canvas runs underneath it, so the
//! board is never letterboxed by chrome, and the palette can be dragged out of the
//! way later without reflowing anything.

use crate::color::to_egui;
use crate::event::{EventSink, UiEvent};
use crate::icon::Icon;
use crate::selection::Field;
use crate::theme::{
    Backing, GlassSurface, Palette, TOOL_BUTTON, floating_frame, floating_frame_over,
    paint_glass_edge, radius, space,
};
use crate::tool::{
    CustomShape, EraserMode, Flyout, PenKind, PenPreset, ShapeColors, ShapeGroup, Tool,
    shape_label,
};
use crate::widgets::{
    ICON_STROKE, Segment, hairline, search_field_for, section_header, segmented, swatch,
    tool_button,
};
use egui::{
    Align, Align2, Color32, Context, CornerRadius, Id, Layout, Order, Painter, PopupCloseBehavior,
    Pos2, Rect, RectAlign, Sense, Stroke, StrokeKind, Ui, Vec2, containers::Popup, pos2, vec2,
};
use vellum_shapes::Shape;

/// Tools, grouped as the palette draws them.
///
/// `docs/04-ui-reference.md` §1 marks Miro's ungrouped 14-entry column as something
/// to improve on and says exactly how: **navigate · create · draw · connect ·
/// insert**. This is that list rather than an approximation of it. The two that were
/// previously wrong are worth naming: `Frame` belongs with *create* — it makes a
/// thing on the board, and grouping it with `Image` said it was an import — and
/// drawing and connecting are separated because a pen stroke and a connector are
/// different acts even though both end in a line.
///
/// `Upload` is the one entry of §1's list that is not here: it needs a file dialog
/// this build has not got, and a button that can only apologise is worse than an
/// absence. `Image` already reports that gap when clicked.
/// The tools on the palette itself. The other six are behind **More** —
/// [`Tool::OCCASIONAL`] — which is drawn as a final group by [`more_row`].
///
/// Was fourteen buttons in five groups, which is a column tall enough to reach both
/// edges of a laptop screen and long enough that finding the sticky note took a moment.
/// *"i just dont use those enough"*: eight everyday tools stay, six fold.
const GROUPS: [&[Tool]; 3] = [
    &[Tool::Select, Tool::Hand],
    &[Tool::Sticky, Tool::Text, Tool::Shape, Tool::Frame],
    &[Tool::Pen, Tool::Eraser],
];

/// The palette's own memory: which flyout is open and what each picker last chose.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolbarState {
    pub open_flyout: Option<Flyout>,
    /// The colour a placed sticky takes. `None` is the palette's default yellow.
    pub sticky: Option<vellum_doc::Color>,
    /// The shape a click on the shape tool will place, shown on the tool button.
    pub shape: Shape,
    /// Which of the three agent roles the agent tool will place.
    ///
    /// On the toolbar rather than decided after placement, for the reason the sticky's
    /// colour is: an orchestrator and a worker are configured differently from the moment
    /// they exist — an orchestrator wants a territory drawn and a cap set — so placing one
    /// and converting it afterwards is a step every single time.
    pub agent_role: vellum_agent::RoleKind,
    pub pen: PenPreset,
    /// What the eraser takes. Miro puts this in the eraser's own flyout, and so does this.
    pub eraser: EraserMode,
    /// The shape picker's search box. `docs/04-ui-reference.md` §3 names search,
    /// categories and *Apply colors* as the parts of Miro's picker worth replicating.
    pub search: String,
    /// One set of colours per category, indexed by [`ShapeGroup::index`].
    pub colors: [ShapeColors; ShapeGroup::ALL.len()],
    /// Which category has its *Apply colors* popover open.
    pub open_colors: Option<ShapeGroup>,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            open_flyout: None,
            sticky: None,
            shape: Shape::Rectangle,
            agent_role: vellum_agent::RoleKind::Worker,
            pen: PenPreset::default(),
            eraser: EraserMode::default(),
            search: String::new(),
            colors: [ShapeColors::default(); ShapeGroup::ALL.len()],
            open_colors: None,
        }
    }
}

impl ToolbarState {
    fn toggle(&mut self, flyout: Flyout) {
        self.open_flyout = if self.open_flyout == Some(flyout) { None } else { Some(flyout) };
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "every one is a distinct per-frame input the chrome owns; a struct here \
              would be a bag with one caller"
)]
pub(crate) fn show(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    active: Tool,
    custom: &[CustomShape],
    cmd_ctx: &crate::command::CommandContext,
    events: &mut EventSink,
    glass: &mut Vec<GlassSurface>,
) -> Rect {
    let ctx = ui.ctx().clone();
    let ctx = &ctx;
    let mut palette_rect = Rect::NOTHING;

    egui::Area::new(Id::new("vellum-toolbar"))
        .anchor(Align2::LEFT_CENTER, vec2(space::of(4), 0.0))
        .order(Order::Middle)
        .show(ctx, |ui| {
            let response = floating_frame(palette).show(ui, |ui| {
                // An `Area` is unconstrained, so anything sized from
                // `available_width` — the group separators — would stretch to
                // egui's default area width and drag the palette's hit region
                // 600 points across the canvas with it.
                ui.set_max_width(TOOL_BUTTON);
                ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                for (index, group) in GROUPS.iter().enumerate() {
                    if index > 0 {
                        ui.add_space(space::UNIT);
                        hairline(ui, palette);
                        ui.add_space(space::UNIT);
                    }
                    for tool in *group {
                        tool_row(ui, palette, state, *tool, active, events);
                    }
                }
                ui.add_space(space::UNIT);
                hairline(ui, palette);
                ui.add_space(space::UNIT);
                more_row(ui, palette, state, active);
            });
            palette_rect = response.response.rect;
            paint_glass_edge(ui.painter(), palette_rect, palette, Backing::Canvas);
        });
    register_glass(glass, palette, palette_rect);
    register_glass(glass, palette, history(ctx, palette, cmd_ctx, events, palette_rect));

    if let Some(flyout) = state.open_flyout {
        let rect = flyout_window(ctx, palette, state, flyout, custom, active, events, palette_rect);
        // The flyout opens *beside* the palette, not on top of it, so both are glass
        // over the canvas and neither is glass over the other. That adjacency is
        // deliberate: `docs/05-design-language.md` §3a forbids stacking the material,
        // and a picker that overlapped its own toolbar would have to be opaque.
        register_glass(glass, palette, rect);
    }

    // Reported so `crate::context_bar` can stay clear of it. The board deliberately
    // runs *underneath* the palette — `ChromeOutput::canvas_rect` does not subtract
    // it — but a floating toolbar is chrome, and chrome under chrome is a control
    // that cannot be clicked. Measured: a selection at the left edge put the bar's
    // fill swatch behind the tool column.
    palette_rect
}

/// Undo and redo, as a detached cluster under the tool palette.
///
/// `docs/04-ui-reference.md` §1 draws them exactly there and §6 asks for **buttons as
/// well as `⌘Z`/`⌘Y`**, with redo greyed out when there is nothing to redo. Until now
/// the only ways to undo were the keyboard and a menu row: someone who reached for
/// the button Miro puts at the lower left found nothing at all.
///
/// Detached rather than a fourth group inside the palette, because these are not
/// tools — nothing here changes what the pointer does — and putting them in the same
/// box would say they were.
fn history(
    ctx: &Context,
    palette: Palette,
    cmd_ctx: &crate::command::CommandContext,
    events: &mut EventSink,
    anchor: Rect,
) -> Rect {
    if !anchor.is_positive() {
        return Rect::NOTHING;
    }
    egui::Area::new(Id::new("vellum-history"))
        .fixed_pos(pos2(anchor.left(), anchor.bottom() + space::of(3)))
        .order(Order::Middle)
        .show(ctx, |ui| {
            let inner = floating_frame(palette).show(ui, |ui| {
                ui.set_max_width(TOOL_BUTTON);
                ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                for command in [crate::command::Command::Undo, crate::command::Command::Redo] {
                    let available = command.availability(cmd_ctx);
                    let Some(icon) = command.icon() else { continue };
                    let response = ui
                        .add_enabled_ui(available.is_enabled(), |ui| {
                            crate::widgets::icon_button(ui, palette, icon, TOOL_BUTTON, false)
                        })
                        .inner;
                    // An icon has no words of its own, so a greyed square with no
                    // explanation is the least informative control there is. egui
                    // shows each of these in only one state, so both are needed.
                    let response = match available.reason() {
                        Some(why) => {
                            response.on_disabled_hover_text(format!("{} — {why}", command.label()))
                        }
                        None => response.on_hover_ui(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(command.label());
                                if let Some(shortcut) = command.shortcut() {
                                    ui.label(
                                        crate::theme::numeric(crate::format_shortcut(
                                            shortcut,
                                            cfg!(target_os = "macos"),
                                        ))
                                        .color(palette.muted),
                                    );
                                }
                            });
                        }),
                    };
                    if response.clicked() {
                        events.command(command);
                    }
                }
            });
            paint_glass_edge(ui.painter(), inner.response.rect, palette, Backing::Canvas);
            inner.response.rect
        })
        .inner
}

/// Notes a floating surface for the renderer to blur the canvas behind.
pub(crate) fn register_glass(glass: &mut Vec<GlassSurface>, palette: Palette, rect: Rect) {
    let Some(spec) = palette.glass(Backing::Canvas) else { return };
    if !rect.is_positive() {
        return;
    }
    glass.push(GlassSurface {
        rect,
        corner_radius: f32::from(radius::LARGE),
        opacity: spec.opacity,
    });
}

fn tool_row(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    tool: Tool,
    active: Tool,
    events: &mut EventSink,
) {
    let flyout = tool.flyout();
    let response =
        tool_button(ui, palette, tool.icon(), TOOL_BUTTON, tool == active, flyout.is_some());

    // Name plus shortcut on every tool, per `docs/04-ui-reference.md` §6. The key is
    // set in the numeric face: it is a glyph standing for a physical key, and it
    // should not read as part of the sentence beside it.
    let response = response.on_hover_ui(|ui| {
        ui.horizontal(|ui| {
            ui.label(tool.label());
            if let Some(key) = tool.shortcut_hint() {
                ui.label(crate::theme::numeric(key).color(palette.muted));
            }
        });
    });

    if response.clicked() {
        // Clicking a tool that owns a flyout both selects the tool and toggles its
        // picker — Miro's behaviour, and the alternative (a separate hit target for
        // the chevron) is a 6px click target.
        if let Some(flyout) = flyout {
            state.toggle(flyout);
        } else {
            state.open_flyout = None;
        }
        if tool != active {
            events.push(UiEvent::ToolChanged(tool));
        }
    }
}

/// The **More** button: the six occasional tools, folded into one row.
///
/// It wears the icon of whichever of them is active, and its own icon when none is —
/// the same idea as the shape button, and the reason the palette does not lie about what
/// is selected. Pick a chart, and the row shows a chart while the tool is armed.
fn more_row(ui: &mut Ui, palette: Palette, state: &mut ToolbarState, active: Tool) {
    let showing = Tool::OCCASIONAL.iter().copied().find(|tool| *tool == active);
    // `Icon::More`, the vertical ellipsis — **not** `Icon::Grid`, which was the first
    // choice and collided head-on: the frame tool is now the crossing-rules artboard
    // mark, and a grid is the same four lines. Two identical marks four rows apart in
    // one column, which a screenshot caught and no test could.
    let icon = showing.map_or(Icon::More, Tool::icon);
    let response = tool_button(ui, palette, icon, TOOL_BUTTON, showing.is_some(), true);

    let response = response.on_hover_ui(|ui| {
        ui.horizontal(|ui| {
            ui.label(showing.map_or("More tools", Tool::label));
            // No shortcut hint: the button is not a tool and has no key of its own.
            // Each tool inside keeps its own, which the picker shows.
            if let Some(key) = showing.and_then(Tool::shortcut_hint) {
                ui.label(crate::theme::numeric(key).color(palette.muted));
            }
        });
    });

    if response.clicked() {
        state.toggle(Flyout::More);
    }
}

/// The **More** flyout: one row per occasional tool, named and with its key.
///
/// Named rows rather than a grid of icons. The whole reason these six are behind a button
/// is that the user does not reach for them often, and an icon you meet rarely is one you
/// have to decode every time — which is the cost the palette was already paying.
fn more_picker(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    active: Tool,
    events: &mut EventSink,
) {
    section_header(ui, palette, "More tools");
    let width = space::of(48);
    ui.set_min_width(width);

    // **Miro's rows are taller than a toolbar's**, and that is most of why its pop-ups read as
    // menus you pick from rather than as strips of icons — the user put its shapes and frame
    // pickers beside this one and asked for the difference. The same 32pt the context bar
    // moved to, so every list in the application answers a pointer at the same size.
    let row = vec2(width, space::of(8));
    for tool in Tool::OCCASIONAL {
        // The icon sits in a reserved left column and the label follows, so every name
        // starts on the same edge — `menu::row_button`'s tick column, same reasoning.
        let mut button = crate::menu::row_button(tool.label()).min_size(row);
        if tool == active {
            button = button.fill(palette.accent_soft);
        }
        let response = ui.add(button);
        let box_ = Rect::from_center_size(
            egui::pos2(response.rect.left() + space::of(3), response.rect.center().y),
            egui::Vec2::splat(space::of(4)),
        );
        let ink = if tool == active { palette.accent } else { palette.muted };
        tool.icon().paint(&ui.painter().clone(), box_, ink, crate::widgets::ICON_STROKE);

        if let Some(key) = tool.shortcut_hint() {
            let at = Rect::from_center_size(
                egui::pos2(response.rect.right() - space::of(3), response.rect.center().y),
                egui::Vec2::splat(space::of(4)),
            );
            ui.painter().text(
                at.center(),
                egui::Align2::CENTER_CENTER,
                key,
                egui::FontId::monospace(11.0),
                palette.muted,
            );
        }

        if response.clicked() {
            state.open_flyout = None;
            if tool != active {
                events.push(UiEvent::ToolChanged(tool));
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the same shape as `show`, which carries the same note: every one is a \
              distinct per-frame input the chrome owns, and a struct here would be a bag \
              with one caller"
)]
fn flyout_window(
    ctx: &Context,
    palette: Palette,
    state: &mut ToolbarState,
    flyout: Flyout,
    custom: &[CustomShape],
    // For `Flyout::More`, which ticks whichever occasional tool is armed.
    active: Tool,
    events: &mut EventSink,
    anchor: Rect,
) -> Rect {
    let position = pos2(anchor.right() + space::of(2), anchor.top());
    let response = egui::Area::new(Id::new(("vellum-flyout", flyout as u8)))
        .fixed_pos(position)
        .order(Order::Foreground)
        .show(ctx, |ui| {
            let inner = floating_frame(palette).show(ui, |ui| match flyout {
                Flyout::Sticky => sticky_picker(ui, palette, state, events),
                Flyout::Shape => shape_picker(ui, palette, state, custom, events),
                Flyout::Pen => pen_picker(ui, palette, state, events),
                Flyout::Eraser => eraser_picker(ui, palette, state, events),
                Flyout::Agent => agent_picker(ui, palette, state, events),
                Flyout::More => more_picker(ui, palette, state, active, events),
            });
            paint_glass_edge(ui.painter(), inner.response.rect, palette, Backing::Canvas);
        })
        .response;

    // Click-outside closes. Checked against the flyout *and* the palette so that
    // clicking the tool button again toggles rather than closing and reopening — and
    // suspended entirely while the picker's own colour popover is open, since that
    // popover is a separate area and every click in it would otherwise read as a click
    // outside the flyout that owns it.
    let clicked_outside = state.open_colors.is_none()
        && ctx.input(|i| i.pointer.any_click())
        && ctx.pointer_interact_pos().is_some_and(|p| {
            !response.rect.contains(p) && !anchor.contains(p)
        });
    if clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.open_flyout = None;
    }
    response.rect
}

/// Whether a shape's name matches the picker's query.
///
/// Substring on the label, case-insensitively, for the same reason the board search
/// is: the names are short and the person typing knows them. "rect" has to find both
/// the rectangle and the rounded one, and a fuzzy matcher that also surfaces
/// "Predefined process" for it is worse than no search.
fn matches(shape: Shape, query: &str) -> bool {
    let query = query.trim();
    query.is_empty() || shape_label(shape).to_lowercase().contains(&query.to_lowercase())
}

/// The sticky pack, as a grid of the colours a note can be.
///
/// Two columns like Miro's, from the board palette's own sticky row so a note placed here and
/// a note recoloured from the context bar reach for the same swatches — `SWATCHES[0]` is
/// Miro's own pack, verbatim, which is what makes an imported note recolour back to where it
/// started.
///
/// Choosing one **arms the tool** as well as setting the colour: a picker that changed a
/// setting and left the pointer on Select would need a second click to do the thing the first
/// click plainly meant.
fn sticky_picker(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    events: &mut EventSink,
) {
    const COLUMNS: usize = 2;
    let tile = space::of(11);
    ui.set_max_width(COLUMNS as f32 * (tile + space::UNIT) + space::of(2));
    ui.spacing_mut().item_spacing = Vec2::splat(space::UNIT);

    for pair in crate::color::SWATCHES[0].chunks(COLUMNS) {
        ui.horizontal(|ui| {
            for color in pair {
                let selected = state.sticky == Some(*color);
                if crate::widgets::swatch(
                    ui,
                    palette,
                    Some(crate::color::to_egui(*color)),
                    Vec2::splat(tile),
                    selected,
                )
                .on_hover_text(crate::theme::numeric(color.to_hex()))
                .clicked()
                {
                    state.sticky = Some(*color);
                    state.open_flyout = None;
                    events.push(UiEvent::StickyColorChosen(*color));
                    events.push(UiEvent::ToolChanged(Tool::Sticky));
                }
            }
        });
    }
}

fn shape_picker(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    custom: &[CustomShape],
    events: &mut EventSink,
) {
    const COLUMNS: f32 = 8.0;
    let tile = space::of(8);
    ui.set_max_width(COLUMNS * (tile + 2.0));

    search_field_for(
        ui,
        palette,
        Id::new("vellum-shape-search"),
        "Search shapes",
        &mut state.search,
    );

    let mut any = my_shapes(ui, palette, state, custom, events);
    for group in ShapeGroup::ALL {
        let shapes: Vec<Shape> = group.shapes().filter(|s| matches(*s, &state.search)).collect();
        if shapes.is_empty() {
            continue;
        }
        any = true;
        category_header(ui, palette, state, group, events);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(2.0);
            for shape in shapes {
                if shape_tile(ui, palette, shape, shape == state.shape)
                    .on_hover_ui(|ui| shape_tooltip(ui, palette, shape))
                    .clicked()
                {
                    state.shape = shape;
                    state.open_flyout = None;
                    events.push(UiEvent::ShapeChosen(shape));
                    events.push(UiEvent::ToolChanged(Tool::Shape));
                }
            }
        });
    }

    if !any {
        ui.add_space(space::of(2));
        ui.label(egui::RichText::new("No shape by that name").color(palette.faint));
        ui.add_space(space::of(2));
    }
}

/// *My Shapes* — the user's own SVGs, and the button that adds one.
///
/// `docs/04-ui-reference.md` §3 puts this at the top of Miro's picker, above the
/// built-in categories, and that order is worth keeping: a shape you uploaded is one
/// you went to some trouble over, so it should not be below forty you did not.
///
/// Returns whether it drew anything, so the picker knows the search matched something.
fn my_shapes(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    custom: &[CustomShape],
    events: &mut EventSink,
) -> bool {
    let query = state.search.trim().to_lowercase();
    let visible: Vec<&CustomShape> = custom
        .iter()
        .filter(|c| query.is_empty() || c.name.to_lowercase().contains(&query))
        .collect();
    // A search that matches nothing here should not leave the upload button stranded
    // above an empty section — but with no query at all the section is the only way to
    // discover that uploading is possible, so it stays.
    if visible.is_empty() && !query.is_empty() {
        return false;
    }

    section_header(ui, palette, "My shapes");
    if visible.is_empty() {
        ui.label(egui::RichText::new("Nothing uploaded yet").color(palette.faint));
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(2.0);
            for shape in visible {
                if custom_tile(ui, palette, shape).clicked() {
                    state.open_flyout = None;
                    events.push(UiEvent::CustomShapeChosen(shape.id));
                    events.push(UiEvent::ToolChanged(Tool::Shape));
                }
            }
        });
    }
    ui.add_space(space::UNIT);
    // The chrome has no SVG parser and no rasteriser — `docs/01-architecture.md` keeps
    // it that way — so this asks the app for a file rather than opening one.
    if ui
        .add(egui::Button::new("Upload SVG…"))
        .on_hover_text("Browse for an SVG to use as a shape")
        .clicked()
    {
        events.push(UiEvent::UploadShape);
    }
    true
}

/// One of the user's shapes: its preview if the app has uploaded one, its outline if
/// not.
fn custom_tile(ui: &mut Ui, palette: Palette, shape: &CustomShape) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(space::of(8) - 2.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let corner = CornerRadius::same(radius::SMALL);
        if response.hovered() {
            ui.painter().rect_filled(rect, corner, palette.hover);
        }
        let inner = rect.shrink(space::of(2) - 1.0);
        match shape.thumbnail {
            Some(thumb) => {
                let (tw, th) = (thumb.size[0] as f32, thumb.size[1] as f32);
                let scale = (inner.width() / tw).min(inner.height() / th);
                let fitted =
                    Rect::from_center_size(inner.center(), vec2(tw * scale, th * scale));
                let mut mesh = egui::Mesh::with_texture(thumb.texture);
                mesh.add_rect_with_uv(
                    fitted,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    // Unmodulated: a multiplier of one, not a colour.
                    crate::theme::UNTINTED,
                );
                ui.painter().add(egui::Shape::mesh(mesh));
            }
            None => Icon::Image.paint(&ui.painter().clone(), inner, palette.faint, ICON_STROKE),
        }
    }
    response.on_hover_text(shape.name.as_str())
}

/// A category heading with Miro's *Apply colors* control on the right.
///
/// The control sets the colours a *newly placed* shape from this category takes; it is
/// not an edit to the selection. The tiles themselves stay in the interface ink rather
/// than previewing the choice, and that is deliberate: a silhouette drawn in a pale
/// fill would be invisible against the panel, and legibility wins — `docs/05` §3a
/// settles the same argument for glass. The swatch beside the heading is where the
/// current choice is shown.
fn category_header(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    group: ShapeGroup,
    events: &mut EventSink,
) {
    ui.add_space(space::of(3));
    ui.horizontal(|ui| {
        ui.label(crate::theme::section_label(group.title(), palette));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let response = colors_button(ui, palette, state.colors[group.index()])
                .on_hover_text("Apply colours to new shapes");
            if response.clicked() {
                state.open_colors =
                    if state.open_colors == Some(group) { None } else { Some(group) };
            }
            colors_popover(ui, palette, state, group, &response, events);
        });
    });
    ui.add_space(space::UNIT);
}

/// The control itself: the fill, ringed by the outline colour.
fn colors_button(ui: &mut Ui, palette: Palette, colors: ShapeColors) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(space::of(5)), Sense::click());
    if ui.is_rect_visible(rect) {
        let corner = CornerRadius::same(radius::SMALL);
        match colors.fill {
            Some(fill) => {
                ui.painter().rect_filled(rect, corner, to_egui(fill));
            }
            None => {
                ui.painter().rect_filled(rect, corner, palette.well);
                let inset = rect.shrink(3.0);
                ui.painter().line_segment(
                    [inset.left_bottom(), inset.right_top()],
                    Stroke::new(ICON_STROKE, palette.danger),
                );
            }
        }
        // Two rings: the chosen outline colour, then a hairline so a pale outline does
        // not dissolve into the panel behind it.
        ui.painter().rect_stroke(
            rect,
            corner,
            Stroke::new(2.0, to_egui(colors.stroke)),
            StrokeKind::Inside,
        );
        ui.painter().rect_stroke(rect, corner, palette.hairline_stroke(), StrokeKind::Outside);
    }
    response
}

/// The popover behind *Apply colors*: one swatch grid for the fill, one for the
/// outline.
fn colors_popover(
    ui: &mut Ui,
    palette: Palette,
    state: &mut ToolbarState,
    group: ShapeGroup,
    anchor: &egui::Response,
    events: &mut EventSink,
) {
    let mut open = state.open_colors == Some(group);
    if !open {
        return;
    }
    let _ = ui;
    let mut colors = state.colors[group.index()];
    let mut changed = false;

    Popup::from_response(anchor)
        .id(Id::new(("vellum-shape-colors", group.index())))
        .open_bool(&mut open)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(RectAlign::BOTTOM_START)
        .gap(space::UNIT)
        // Opaque: the flyout it opens from is already glass over the canvas, and
        // `docs/05-design-language.md` §3a forbids stacking the material.
        .frame(floating_frame_over(palette, Backing::Panel))
        .show(|ui| {
            ui.set_min_width(space::of(44));
            section_header(ui, palette, "Fill");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = space::UNIT;
                if swatch(ui, palette, None, Vec2::splat(space::of(4) + 2.0), colors.fill.is_none())
                    .on_hover_text("No fill")
                    .clicked()
                {
                    colors.fill = None;
                    changed = true;
                }
            });
            if swatch_grid(ui, palette, colors.fill).inspect(|c| colors.fill = Some(*c)).is_some() {
                changed = true;
            }
            section_header(ui, palette, "Outline");
            if swatch_grid(ui, palette, Some(colors.stroke))
                .inspect(|c| colors.stroke = *c)
                .is_some()
            {
                changed = true;
            }
        });

    if changed {
        state.colors[group.index()] = colors;
        events.push(UiEvent::ShapeColorsChanged { group, colors });
    }
    if !open {
        state.open_colors = None;
    }
}

/// The board palette as a grid, returning whatever was clicked.
fn swatch_grid(
    ui: &mut Ui,
    palette: Palette,
    current: Option<vellum_doc::Color>,
) -> Option<vellum_doc::Color> {
    let mut chosen = None;
    for row in crate::color::SWATCHES {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::UNIT;
            for color in *row {
                if swatch(
                    ui,
                    palette,
                    Some(to_egui(*color)),
                    Vec2::splat(space::of(4) + 2.0),
                    current == Some(*color),
                )
                // Hex in the numeric face: it is a value, not a word.
                .on_hover_text(crate::theme::numeric(color.to_hex()))
                .clicked()
                {
                    chosen = Some(*color);
                }
            }
        });
    }
    chosen
}

fn shape_tooltip(ui: &mut Ui, palette: Palette, shape: Shape) {
    ui.horizontal(|ui| {
        ui.label(shape_label(shape));
        if let Some(key) = crate::tool::SHAPE_SHORTCUTS
            .iter()
            .find(|(_, s)| *s == shape)
            .map(|(key, _)| key.name())
        {
            ui.label(crate::theme::numeric(key).color(palette.muted));
        }
    });
}

fn shape_tile(ui: &mut Ui, palette: Palette, shape: Shape, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(space::of(8) - 2.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let corner = CornerRadius::same(radius::SMALL);
        if selected {
            ui.painter().rect_filled(rect, corner, palette.accent_soft);
        } else if response.hovered() {
            ui.painter().rect_filled(rect, corner, palette.hover);
        }
        let color = if selected { palette.on_accent_soft } else { palette.text };
        paint_shape(
            &ui.painter().clone(),
            rect.shrink(space::of(2) - 1.0),
            shape,
            Stroke::new(ICON_STROKE, color),
        );
    }
    response
}

/// Draws a catalogue shape's silhouette inside `rect`.
///
/// Stroked rather than filled: several catalogue shapes are concave — the star, the
/// cloud, every arrow — and `epaint`'s polygon fill assumes convexity, so filling
/// them would draw a different shape from the one the board will place.
fn paint_shape(painter: &Painter, rect: Rect, shape: Shape, stroke: Stroke) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let outline = shape.outline(rect.width() / rect.height());
    let at = |p: vellum_shapes::Point| -> Pos2 {
        pos2(rect.min.x + p.x * rect.width(), rect.min.y + p.y * rect.height())
    };

    for contour in outline.flattened() {
        let points: Vec<Pos2> = contour.into_iter().map(at).collect();
        if points.len() >= 2 {
            painter.add(egui::Shape::closed_line(points, stroke));
        }
    }
    for detail in &outline.details {
        let points: Vec<Pos2> = detail.flatten(0.004).into_iter().map(at).collect();
        if points.len() >= 2 {
            painter.add(egui::Shape::line(points, stroke));
        }
    }
}

/// The pen's tool, width and colour.
///
/// # Kept labelled, deliberately, where Miro's is a bare icon column
///
/// The user sent Miro's pen flyout beside this one. Miro's is a vertical strip of icons —
/// pen, marker, highlighter, eraser, lasso — with three colour dots under it, and converting
/// to that shape would mean giving up the width presets, the named tools and the full palette
/// in exchange for a picker that fits in less space than this one needs anyway.
///
/// So what was taken from the comparison is the *legibility* rather than the layout: bigger
/// swatches, matching the sizing everything else moved to when the user said Miro's menus were
/// "a lot more visible and easier to use". If the icon column is wanted for its own sake it is
/// a small change from here, and it is the user's call rather than one to make by inference
/// from a screenshot.
fn pen_picker(ui: &mut Ui, palette: Palette, state: &mut ToolbarState, events: &mut EventSink) {
    ui.set_max_width(space::of(56));
    let mut changed = false;

    section_header(ui, palette, "Tool");
    let kinds: Vec<Segment<PenKind>> =
        PenKind::ALL.iter().map(|k| Segment::text(*k, k.label())).collect();
    if let Some(kind) = segmented(ui, palette, &Field::Uniform(state.pen.kind), &kinds) {
        state.pen = state.pen.with_kind(kind);
        changed = true;
    }

    section_header(ui, palette, "Width");
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::UNIT;
        for width in PenPreset::WIDTHS {
            let selected = (state.pen.width - width).abs() < f32::EPSILON;
            if width_tile(ui, palette, width, selected).clicked() {
                state.pen.width = width;
                changed = true;
            }
        }
    });

    section_header(ui, palette, "Colour");
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(space::UNIT);
        for color in crate::color::SWATCHES.iter().flat_map(|row| row.iter()) {
            let selected = state.pen.color == *color;
            if swatch(
                ui,
                palette,
                Some(to_egui(*color)),
                // The same swatch the sticky pack and the colour picker use. It was 18pt —
                // small enough that the colours were hard to tell apart at a glance, which is
                // the whole job of a swatch grid.
                Vec2::splat(space::of(6)),
                selected,
            )
            // Hex in the numeric face: it is a value, not a word.
            .on_hover_text(crate::theme::numeric(color.to_hex()))
            .clicked()
            {
                state.pen.color = *color;
                changed = true;
            }
        }
    });

    if changed {
        events.push(UiEvent::PenChanged(state.pen));
    }
}

/// The eraser's flyout: what it takes, and how wide it is.
///
/// This is the mode Miro puts here and this app only had as a bare ⇧ modifier — see
/// [`EraserMode`]. The hint under the toggle is not decoration: an eraser that can delete a
/// whole frame needs to say which of the two things it is about to do *before* the sweep,
/// and it also names ⇧ in place, so the modifier is discoverable without the shortcut sheet.
///
/// The width steps are shared with the pen deliberately. The eraser's radius is read from
/// `pen().width` on the app side — one number, one control — so a wide pen has always meant
/// a wide eraser and this makes that visible rather than surprising.
fn eraser_picker(ui: &mut Ui, palette: Palette, state: &mut ToolbarState, events: &mut EventSink) {
    ui.set_max_width(space::of(49));

    section_header(ui, palette, "Erase");
    let modes: Vec<Segment<EraserMode>> =
        EraserMode::ALL.iter().map(|m| Segment::text(*m, m.label())).collect();
    if let Some(mode) = segmented(ui, palette, &Field::Uniform(state.eraser), &modes) {
        state.eraser = mode;
        events.push(UiEvent::EraserChanged(mode));
    }
    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new(state.eraser.hint())
            .size(11.0)
            .color(palette.muted),
    );

    section_header(ui, palette, "Size");
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::UNIT;
        for width in PenPreset::WIDTHS {
            let selected = (state.pen.width - width).abs() < f32::EPSILON;
            if width_tile(ui, palette, width, selected).clicked() {
                state.pen.width = width;
                events.push(UiEvent::PenChanged(state.pen));
            }
        }
    });
}

/// Which of the three roles the agent tool places: a worker, an orchestrator, or the meta
/// agent.
///
/// A flyout rather than a conversion after the fact, for the sticky picker's reason: the
/// three are configured differently from the moment they exist — an orchestrator is born
/// owning a region of the board and a spawn cap — so placing a worker and converting it is an
/// extra step every single time.
///
/// Each row carries a line saying what the role *is*. Three nouns alone would not do it:
/// "Orchestrator" and "Meta agent" are the same word to somebody who has not used either,
/// which is the same argument that put a drawn swatch beside each accent colour rather than
/// its name.
fn agent_picker(ui: &mut Ui, palette: Palette, state: &mut ToolbarState, events: &mut EventSink) {
    use vellum_agent::RoleKind;

    ui.set_max_width(space::of(56));
    section_header(ui, palette, "Agent");

    let roles: Vec<Segment<RoleKind>> =
        RoleKind::ALL.iter().map(|role| Segment::text(*role, role.label())).collect();
    if let Some(role) = segmented(ui, palette, &Field::Uniform(state.agent_role), &roles) {
        state.agent_role = role;
        events.push(UiEvent::AgentRoleChosen(role));
        // Choosing arms the tool, exactly as choosing a sticky colour does: a picker that
        // set a setting and left the pointer on Select would need a second click to do the
        // thing the first click plainly meant.
        events.push(UiEvent::ToolChanged(Tool::Agent));
    }

    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new(agent_role_hint(state.agent_role)).size(11.0).color(palette.muted),
    );
}

/// One line saying what a role does, for the picker.
///
/// Written for somebody who has not used one: what it is *for*, and — for the two that carry
/// a power the others do not — what that power is, because a limit the user cannot see is one
/// they cannot reason about.
const fn agent_role_hint(role: vellum_agent::RoleKind) -> &'static str {
    match role {
        vellum_agent::RoleKind::Worker => "Does the work: writes, researches, answers.",
        vellum_agent::RoleKind::Orchestrator => {
            "Manages other agents. Owns a region of the board and a cap on how many it may run."
        }
        vellum_agent::RoleKind::Meta => {
            "Talks to you about the board. The only role that may edit other agents' settings."
        }
    }
}

/// A width swatch drawn as a dot of that width, clamped so the widest still fits.
fn width_tile(ui: &mut Ui, palette: Palette, width: f32, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(space::of(8) - 2.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let corner = CornerRadius::same(radius::SMALL);
        if selected {
            ui.painter().rect_filled(rect, corner, palette.accent_soft);
            ui.painter().rect_stroke(
                rect,
                corner,
                Stroke::new(palette.hairline_width(), palette.accent),
                StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(rect, corner, palette.hover);
        }
        let color: Color32 = if selected { palette.on_accent_soft } else { palette.text };
        ui.painter().circle_filled(rect.center(), (width * 0.4).clamp(1.5, 10.0), color);
    }
    response.on_hover_text(crate::theme::numeric(format!("{width} px")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Context;
    use crate::theme::Theme;

    fn run(
        state: &mut ToolbarState,
        active: Tool,
        input: egui::RawInput,
        ctx: &Context,
    ) -> Vec<UiEvent> {
        run_in(state, active, input, ctx, Palette::LIGHT).0
    }

    fn run_in(
        state: &mut ToolbarState,
        active: Tool,
        input: egui::RawInput,
        ctx: &Context,
        palette: Palette,
    ) -> (Vec<UiEvent>, Vec<GlassSurface>) {
        run_with(state, active, input, ctx, palette, &[])
    }

    fn run_with(
        state: &mut ToolbarState,
        active: Tool,
        input: egui::RawInput,
        ctx: &Context,
        palette: Palette,
        custom: &[CustomShape],
    ) -> (Vec<UiEvent>, Vec<GlassSurface>) {
        let mut events = EventSink::default();
        let mut glass = Vec::new();
        let _ = ctx.run_ui(input, |ui| {
            let cmd_ctx = crate::command::CommandContext {
                board_open: true,
                can_undo: true,
                ..crate::command::CommandContext::default()
            };
            show(ui, palette, state, active, custom, &cmd_ctx, &mut events, &mut glass);
        });
        (events.take(), glass)
    }

    fn context() -> Context {
        let ctx = Context::default();
        crate::theme::apply(&ctx, Theme::Light);
        ctx
    }

    #[test]
    fn the_palette_draws_every_tool_and_emits_nothing_untouched() {
        let ctx = context();
        let mut state = ToolbarState::default();
        assert!(run(&mut state, Tool::Select, egui::RawInput::default(), &ctx).is_empty());
        assert_eq!(state.open_flyout, None);
    }

    #[test]
    fn every_flyout_renders_without_panicking() {
        let ctx = context();
        for flyout in [Flyout::Shape, Flyout::Pen, Flyout::Eraser] {
            let mut state = ToolbarState { open_flyout: Some(flyout), ..ToolbarState::default() };
            let events = run(&mut state, Tool::Shape, egui::RawInput::default(), &ctx);
            assert!(events.is_empty(), "{flyout:?} emitted {events:?} on a passive frame");
        }
    }

    /// The eraser's mode has to be reachable from the palette, which is the whole point of
    /// giving it a flyout: it existed only as a bare ⇧ modifier, so a gesture that deletes
    /// whole frames was discoverable only by reading the shortcut sheet.
    ///
    /// Both halves are asserted — that the tool *owns* a flyout, and that both modes render
    /// with their hint. A `Flyout` variant with no `Tool::flyout` arm pointing at it is a
    /// picker nothing can open.
    #[test]
    fn the_eraser_owns_a_flyout_and_both_modes_draw() {
        assert_eq!(Tool::Eraser.flyout(), Some(Flyout::Eraser));

        let ctx = context();
        for mode in EraserMode::ALL {
            let mut state = ToolbarState {
                open_flyout: Some(Flyout::Eraser),
                eraser: mode,
                ..ToolbarState::default()
            };
            let events = run(&mut state, Tool::Eraser, egui::RawInput::default(), &ctx);
            assert!(events.is_empty(), "{mode:?} emitted {events:?} on a passive frame");
            assert_eq!(state.eraser, mode, "a passive frame must not change the mode");
            // The hint names ⇧, so the modifier is documented where it is used rather than
            // only in the shortcut sheet.
            assert!(mode.hint().contains('⇧'), "{}", mode.hint());
        }
    }

    /// The picker's search, `docs/04-ui-reference.md` §3. Filtering must survive a
    /// query that matches nothing rather than drawing an empty flyout with no
    /// explanation.
    #[test]
    fn the_shape_picker_filters_by_name_and_says_so_when_nothing_matches() {
        assert!(matches(Shape::Rectangle, ""));
        assert!(matches(Shape::Rectangle, "  "));
        assert!(matches(Shape::Rectangle, "RECT"));
        assert!(matches(Shape::rounded_rectangle(), "rect"), "a partial word finds both");
        assert!(!matches(Shape::Rectangle, "cylinder"));

        let ctx = context();
        for query in ["", "rect", "process", "zzz"] {
            let mut state = ToolbarState {
                open_flyout: Some(Flyout::Shape),
                search: query.to_owned(),
                ..ToolbarState::default()
            };
            let events = run(&mut state, Tool::Shape, egui::RawInput::default(), &ctx);
            assert!(events.is_empty(), "searching {query:?} emitted {events:?}");
        }
    }

    /// The palette floats over the canvas, so the renderer has to be told where to
    /// blur. An unreported panel is a panel the material silently does not apply to.
    #[test]
    fn the_floating_clusters_report_themselves_as_glass() {
        let ctx = context();
        let mut state = ToolbarState { open_flyout: Some(Flyout::Shape), ..Default::default() };
        let (_, glass) =
            run_in(&mut state, Tool::Shape, egui::RawInput::default(), &ctx, Palette::LIGHT);
        assert_eq!(
            glass.len(),
            3,
            "the palette, the undo cluster and the flyout: {glass:?}"
        );
        for surface in &glass {
            assert!(surface.rect.is_positive());
            assert_eq!(surface.corner_radius, f32::from(radius::LARGE));
            assert!(surface.opacity < u8::MAX);
        }
        // …and no two of them overlap, because glass never stacks: the flyout opens
        // beside the palette rather than on top of it, and the undo cluster sits
        // clear below.
        for (index, surface) in glass.iter().enumerate() {
            for other in &glass[index + 1..] {
                assert!(
                    !surface.rect.intersects(other.rect),
                    "two glass surfaces are stacked: {glass:?}"
                );
            }
        }
    }

    /// `docs/04-ui-reference.md` §6: **buttons as well as the keyboard**, with redo
    /// greyed out when there is nothing to redo. The cluster is what a person reaches
    /// for at the lower left, and it was not there at all.
    #[test]
    fn the_undo_cluster_acts_and_greys_out_what_cannot_act() {
        let ctx = context();
        let mut state = ToolbarState::default();
        let mut events = EventSink::default();
        let mut glass = Vec::new();
        let cmd_ctx = crate::command::CommandContext {
            board_open: true,
            can_undo: true,
            can_redo: false,
            ..crate::command::CommandContext::default()
        };
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            show(
                ui,
                Palette::LIGHT,
                &mut state,
                Tool::Select,
                &[],
                &cmd_ctx,
                &mut events,
                &mut glass,
            );
        });
        assert!(events.take().is_empty(), "a passive frame emitted something");
        assert!(crate::command::Command::Undo.icon().is_some(), "undo has no glyph to draw");
        assert!(crate::command::Command::Redo.icon().is_some(), "redo has no glyph to draw");
        assert!(crate::command::Command::Undo.availability(&cmd_ctx).is_enabled());
        assert!(!crate::command::Command::Redo.availability(&cmd_ctx).is_enabled());
        assert!(
            crate::command::Command::Redo.availability(&cmd_ctx).reason().is_some(),
            "a greyed button with no explanation is the least informative control there is"
        );
    }

    /// Under Reduce Transparency there is nothing to blur, so nothing is reported —
    /// otherwise the renderer spends its blur budget on surfaces drawn fully opaque.
    #[test]
    fn reduce_transparency_withdraws_the_glass_regions_entirely() {
        let ctx = context();
        let opaque = Palette::LIGHT.opaque();
        let mut state = ToolbarState { open_flyout: Some(Flyout::Pen), ..Default::default() };
        let (_, glass) = run_in(&mut state, Tool::Pen, egui::RawInput::default(), &ctx, opaque);
        assert!(glass.is_empty(), "{glass:?}");
    }

    #[test]
    fn a_flyout_toggles_rather_than_reopening() {
        let mut state = ToolbarState::default();
        state.toggle(Flyout::Shape);
        assert_eq!(state.open_flyout, Some(Flyout::Shape));
        state.toggle(Flyout::Shape);
        assert_eq!(state.open_flyout, None);
        state.toggle(Flyout::Shape);
        state.toggle(Flyout::Pen);
        assert_eq!(state.open_flyout, Some(Flyout::Pen), "one picker at a time");
    }

    /// Every catalogue shape has to survive being asked for its silhouette at the
    /// picker's aspect ratio; a shape that panics there takes the whole flyout down.
    #[test]
    fn every_catalogue_shape_paints_a_tile() {
        let ctx = context();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            for shape in vellum_shapes::CATALOGUE {
                paint_shape(
                    &ui.painter().clone(),
                    Rect::from_min_size(Pos2::ZERO, Vec2::splat(16.0)),
                    *shape,
                    Stroke::new(ICON_STROKE, Palette::LIGHT.text),
                );
            }
        });
    }

    #[test]
    fn a_degenerate_tile_paints_nothing_rather_than_dividing_by_zero() {
        let ctx = context();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint_shape(
                &ui.painter().clone(),
                Rect::from_min_size(Pos2::ZERO, Vec2::ZERO),
                Shape::Ellipse,
                Stroke::new(ICON_STROKE, Palette::LIGHT.text),
            );
        });
    }

    /// Every tool is reachable from the palette exactly once — on it, or behind **More**.
    ///
    /// The `OCCASIONAL` half is the point. Six tools moved off the palette on the user's
    /// own request, and the failure mode of that change is a tool that is in *neither*
    /// list: still in `Tool::ALL`, still with a keyboard shortcut, and with no button
    /// anywhere. Nothing would panic and no other test would notice — the palette would
    /// simply be quietly missing a tool, which is the shape of defect this file's history
    /// is full of. Checking the union rather than `GROUPS` alone is what makes that
    /// impossible.
    #[test]
    fn every_tool_is_on_the_palette_or_behind_more_exactly_once() {
        let mut listed: Vec<Tool> = GROUPS.iter().flat_map(|g| g.iter().copied()).collect();
        listed.extend(Tool::OCCASIONAL);
        assert_eq!(listed.len(), Tool::ALL.len(), "a tool is listed twice or not at all");
        for tool in Tool::ALL {
            let position = listed
                .iter()
                .position(|t| *t == tool)
                .unwrap_or_else(|| panic!("{tool:?} is on neither the palette nor More"));
            listed.remove(position);
        }
        assert!(listed.is_empty());
    }

    /// The folded six are the user's list, in the user's order — **still the first six, and
    /// still in that order**, with the Agent Canvas's three added after them.
    ///
    /// Pinned because it is a *preference*, not a derivation — nothing about a chart makes
    /// it occasional, and the next reader has no way to tell this list was chosen rather
    /// than computed. *"put table charts kanabn and mindmap image and the connector into a
    /// smaller menu in this bar i just dont use those enough"*.
    ///
    /// The assertion is written as a **prefix** check rather than a whole-array one so that
    /// the user's six keep their identity as the user's six. Rewriting it to compare all
    /// nine would have quietly turned a recorded preference into a list anybody may edit,
    /// which is exactly the distinction the original test existed to preserve.
    #[test]
    fn more_holds_exactly_the_six_the_user_named_then_the_agent_canvas_three() {
        let (theirs, ours) = Tool::OCCASIONAL.split_at(6);
        assert_eq!(
            theirs,
            [Tool::Table, Tool::Chart, Tool::Kanban, Tool::MindMap, Tool::Image, Tool::Connector]
        );
        // A note, a file tree and a browser are things you place occasionally *around* the
        // agents. The agent tool itself is deliberately not here: a headline feature folded
        // behind a More button is one nobody discovers.
        assert_eq!(ours, [Tool::Note, Tool::FileTree, Tool::Browser]);
        assert!(!Tool::Agent.is_occasional(), "the agent tool was folded behind More");
        for tool in Tool::OCCASIONAL {
            assert!(tool.is_occasional(), "{tool:?}");
        }
        for group in GROUPS {
            for tool in group {
                assert!(!tool.is_occasional(), "{tool:?} is both on the palette and behind More");
            }
        }
    }
}
