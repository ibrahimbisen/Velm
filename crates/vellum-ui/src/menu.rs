//! The menu bar.
//!
//! Rendered entirely from [`Menu::entries`], so the bar cannot drift from the
//! command table: adding a command to a menu's list is the only edit needed to make
//! it appear, keep its shortcut hint and pick up its enablement rule.
//!
//! Two rows are built from state rather than from the table, because neither is a
//! list of commands: **Move to** enumerates the reference spaces, and **Background**
//! offers a colour and a pattern rather than an action. Both are drawn here and emit
//! their own events.
//!
//! # Nothing is inert
//!
//! Every row asks [`Command::availability`] rather than a boolean, and a disabled row
//! carries the reason as its tooltip. A menu entry that can be clicked and does
//! nothing is the specific failure this replaces.

use crate::command::{Availability, Command, CommandContext, Entry, Menu, Submenu, format_shortcut};
use crate::event::{EventSink, GridSettings, LibraryEvent, UiEvent};
use crate::icon::Icon;
use crate::library::Space;
use crate::theme::{Palette, ThemePreference, space};
use vellum_doc::{Background, Pattern};

use egui::{Ui, vec2};
use std::path::Path;

/// What the bar shows about the open board, and what the toggles currently are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuFlags {
    pub minimap_visible: bool,
    pub presenting: bool,
    pub starred: bool,
    pub translucent: bool,
    /// Whether link cards may fetch. Shown ticked in Preferences.
    pub link_previews: bool,
    /// Whether relative snapping is on. Shown ticked in Preferences.
    pub align_objects: bool,
    /// Whether a move lands on the board's grid. Shown ticked in View ▸ Grid.
    pub snap_to_grid: bool,
    /// Whether the docked properties panel is showing. Off by default — the floating
    /// [`crate::context_bar`] is what a selection gets now.
    pub properties_panel: bool,
    /// Whether every selected agent is showing its working. The **resolved** mode, not the
    /// stored one: a node that inherits a raw default is showing raw, and a tick that read
    /// the stored `Option` would say otherwise.
    pub agent_raw: bool,
    /// Whether browser nodes may run a real engine at all. Off by default — feature 13.
    pub browser_nodes: bool,
    /// Whether coding agents on this board get their own worktrees. Off by default —
    /// feature 4.
    pub worktrees: bool,
}

impl MenuFlags {
    const fn is_on(self, command: Command) -> bool {
        match command {
            Command::ToggleMinimap => self.minimap_visible,
            Command::SnapToGrid => self.snap_to_grid,
            Command::TogglePropertiesPanel => self.properties_panel,
            Command::PresentationMode => self.presenting,
            Command::StarBoard => self.starred,
            Command::ToggleTranslucency => self.translucent,
            Command::ToggleLinkPreviews => self.link_previews,
            Command::ToggleAlignObjects => self.align_objects,
            Command::ToggleAgentRaw => self.agent_raw,
            Command::ToggleBrowserNodes => self.browser_nodes,
            Command::ToggleWorktrees => self.worktrees,
            _ => false,
        }
    }
}

/// One provider, as the app found it on this machine.
///
/// **No key, ever.** `docs/07-agent-canvas.md` §8a puts credentials in one file and nowhere
/// else, and this crate is on the far side of that line: it is told *whether* one is stored,
/// never what it is. [`Self::has_key`] is the whole of what a menu row is allowed to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub provider: vellum_agent::Provider,
    /// Whether the provider is reachable — its CLI is installed, or its endpoint answers.
    ///
    /// Probed rather than assumed: `Provider::supports_subscription` says a CLI *exists to
    /// delegate to*, not that this machine has it, and a missing binary must degrade to a
    /// named row rather than to a wrong assumption.
    pub available: bool,
    /// Whether a credential is stored for it. Never *which*.
    pub has_key: bool,
    /// What the app found, in one line — "claude 2.1.4", "not installed",
    /// "localhost:11434". Shown on hover, so a provider that is not working says why.
    pub detail: String,
}

impl ProviderStatus {
    /// Whether this provider runs on a subscription the user already holds.
    ///
    /// Both halves: the provider must have a CLI to delegate to **and** that CLI must
    /// actually be here. Reporting an uninstalled `claude` as *subscription* would be the
    /// one lie the row exists to avoid — the user would take it as configured and discover
    /// otherwise on their first run.
    pub const fn on_subscription(&self) -> bool {
        self.available && self.provider.supports_subscription()
    }

    /// Whether signing in would achieve anything.
    ///
    /// A local model needs no key by definition, and a provider already running on a
    /// subscription needs none either — offering *Sign in* on those is a control that
    /// collects a credential nothing will read.
    pub const fn wants_a_key(&self) -> bool {
        self.provider.needs_api_key() && !self.on_subscription()
    }

    /// The word beside the name: who pays, or why it is not usable.
    pub fn billing(&self) -> &'static str {
        if self.on_subscription() {
            "Subscription"
        } else if !self.provider.needs_api_key() {
            "Your own machine"
        } else if self.has_key {
            "API key · billed per token"
        } else {
            "No key yet"
        }
    }
}

/// What the bar shows to the right of the menus, and what its data-driven rows need.
#[derive(Debug, Clone, Copy, Default)]
pub struct MenuHeader<'a> {
    /// The open board's name. Empty on the library screen.
    pub title: &'a str,
    /// The open board's file, if it has one. `None` disables everything that acts on
    /// a library row — see [`Command::availability`].
    pub path: Option<&'a Path>,
    /// Whether the board has unsaved edits.
    pub dirty: bool,
    /// Which way the theme toggle currently points. Always false: the app is light
    /// only. Kept so re-offering the choice is a change to one menu.
    pub dark_theme: bool,
    /// What the user chose in Preferences ▸ Appearance, for the tick.
    pub preference: ThemePreference,
    /// The open board's canvas colour and pattern, for Board ▸ Background's ticks.
    pub background: Background,
    /// The grid every board wears, for View ▸ Grid's ticks and its two colour controls.
    /// An application setting rather than the board's — see [`UiEvent::GridChanged`].
    pub grid: GridSettings,
    /// The user's folders, for Board ▸ Move to.
    pub spaces: &'a [Space],
    /// Where the transparency slider sits, `0..=255` of tint over the blurred canvas.
    pub glass_opacity: u8,
    /// Which colour the primary accent wears, for Preferences ▸ Accent colour's tick.
    pub accent: crate::theme::Accent,
    /// The mode a **new** agent node inherits, for Preferences ▸ Agent output's tick.
    pub default_display: vellum_agent::DisplayMode,
    /// Which providers this machine can reach, and how each is paid for. Never a key.
    pub providers: &'a [ProviderStatus],
    pub flags: MenuFlags,
}

/// Edge of the pill's square buttons, matching the status cluster and the find bar so
/// every floating control in the app is the same size.
const BUTTON: f32 = space::of(7) - 2.0;

/// How much of the window height the ☰ menu may take before it scrolls.
///
/// Not 100%: a menu that reaches both edges of the screen looks like a panel and leaves
/// nowhere to aim the pointer when dismissing it. 78% clears the tab strip and the pill
/// above and still shows about thirty rows at a typical window size.
const MENU_MAX_SCREEN_FRACTION: f32 = 0.78;

/// Whether the board's name is being edited in place, and what has been typed.
///
/// Lives on `Chrome` rather than in egui's memory because the committed value has to
/// leave as an event, and because "is the user renaming right now" is a state the
/// keyboard handler has to see — `Chrome::shortcuts` must not fire `V` for the pen tool
/// while somebody is typing a board name.
#[derive(Debug, Default)]
pub(crate) struct Rename {
    /// `Some` while the field is open. Seeded with the current name.
    pub(crate) editing: Option<String>,
}

/// Draws the top-left pill and returns the rectangle it took.
///
/// **A floating panel, not a full-width bar**, which is what `docs/04-ui-reference.md` §2
/// has specified from the start — *"floating rounded panels, not a full-width bar"* — and
/// what Miro actually draws. The bar was full-width because that was the quick way to get
/// a menu on screen; the cost was 36 points of canvas taken across the whole window to
/// carry four words and a title.
///
/// Left to right: **☰** (the four menus) · the mark and wordmark · the board name, which
/// is **click-to-rename in place** · **⋮** (everything the right button offers). The two
/// buttons are not interchangeable and the split is Miro's: ☰ is this board's *menus*, ⋮
/// is this board's *actions*.
pub(crate) fn show(
    ui: &mut Ui,
    palette: Palette,
    header: &MenuHeader<'_>,
    cmd_ctx: &CommandContext,
    rename: &mut Rename,
    events: &mut EventSink,
    glass: &mut Vec<crate::theme::GlassSurface>,
) -> Pill {
    let mut pill = egui::Rect::NOTHING;
    let more_at = None;

    egui::Area::new(egui::Id::new("vellum-menu-bar"))
        .anchor(
            egui::Align2::LEFT_TOP,
            egui::vec2(space::of(3), crate::theme::TAB_STRIP_HEIGHT + space::of(2)),
        )
        .order(egui::Order::Middle)
        .show(ui.ctx(), |ui| {
            let inner = crate::theme::floating_frame(palette).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = space::UNIT;

                    // ☰ — the four menus, one popup. They were four top-level buttons,
                    // which is a menu *bar*; Miro has none and `docs/04` §4 files the
                    // whole tree under one opener.
                    let hamburger =
                        crate::widgets::icon_button(ui, palette, Icon::Hamburger, BUTTON, false)
                            .on_hover_text("Menu");
                    // **The list is taller than the window, so it has to scroll.**
                    //
                    // `Command::ALL` is 58 rows at a 28pt pitch — about **1,624 points**,
                    // against a 900pt window. egui slides an oversized popup up to fit,
                    // and the slide is re-measured each pass, so rows moved by a whole
                    // pitch between the pass that laid them out and the pass that
                    // hit-tested a click: the top and bottom of the menu were
                    // unreachable, and a click near the top ran the row *below* the one
                    // under the pointer. Three `interaction.rs` tests reported exactly
                    // that and were twice misread — here and in this file's history — as
                    // stale expectations about row order. They were not; they were
                    // reporting this.
                    //
                    // Capping the height fixes the layout and the clicks together,
                    // because a popup that fits is a popup egui does not move.
                    // `viewport_rect`, which is what egui 0.35 calls the window — the
                    // older `screen_rect` spelling is gone from both `Context` and
                    // `InputState`.
                    let screen = ui.ctx().viewport_rect().height();
                    let ceiling = (screen * MENU_MAX_SCREEN_FRACTION).max(space::of(60));
                    egui::Popup::menu(&hamburger).id(egui::Id::new("vellum-main-menu")).show(
                        |ui| {
                            ui.set_min_width(space::of(58));
                            ui.set_max_width(space::of(90));
                            egui::ScrollArea::vertical().max_height(ceiling).show(ui, |ui| {
                                // **Four named groups, each opening its own submenu** —
                                // Board, Edit, View, Preferences. The same four names the
                                // menu bar carried before the pill replaced it, and the
                                // same tree `docs/04-ui-reference.md` §4 transcribes from
                                // Miro.
                                //
                                // This was **one flat list** for two rounds and the
                                // flattening is now withdrawn. The argument for it was
                                // that nesting puts every command "two clicks and a hover
                                // away"; the measured consequence was 58 rows at a 28pt
                                // pitch — about **1,624 points against a 900pt window**.
                                // Everything past the fold needed a scroll nobody
                                // announced, and Preferences is the *last* group, so the
                                // settings were the part that disappeared. Reported as
                                // *"you didnt add on the … burger menu the options from
                                // the prvious build … like apperiacne"*.
                                //
                                // **A reachability count said the opposite and was
                                // useless.** All 58 commands were reachable from this
                                // popup, measured, the whole time — so by the only test
                                // anyone had written, nothing was missing. What was
                                // missing was the four *names*, which is what a person
                                // navigates by. Four rows fit any window; a group is at
                                // most sixteen and fits too.
                                for menu in Menu::ALL {
                                    menu_group(ui, palette, menu, cmd_ctx, header, events);
                                }
                            });
                        },
                    );

                    // The identity: the mark at its clear space, then the wordmark.
                    // Small enough to be an identity rather than a banner.
                    let size = space::of(6);
                    ui.add_space(crate::mark::clear_space(size));
                    let (rect, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
                    crate::mark::paint(&ui.painter().clone(), rect, palette);
                    ui.add_space(crate::mark::clear_space(size));
                    // Lowercase, like the mark's own geometry and like Miro's. Set in
                    // type rather than shipped as art: `assets/logo` has no wordmark,
                    // and `docs/05` §6 rejected a wordmark-only lockup for the *logo*,
                    // not for the product's name appearing beside the mark.
                    ui.label(
                        egui::RichText::new("velm")
                            .size(crate::theme::text::PANEL_TITLE)
                            .color(palette.text),
                    );

                    crate::widgets::hairline_vertical(ui, palette, space::of(5));

                    title(ui, palette, header, rename, events);

                    // **No ⋮ here.** The pill used to carry both openers — ☰ for the four
                    // menus and ⋮ for the canvas's own list — and the user asked for one:
                    // *"i dont need 2 burger menus."* ☰ is the one that stays, because it is a
                    // strict superset: every command in the application is reachable through
                    // Board · Edit · View · Preferences, while ⋮ held a chosen subset. Nothing
                    // had to be moved across.
                    //
                    // It also removes the path that was misbehaving. Opened from ⋮, the
                    // Background colour submenu vanished as the pointer reached it; from ☰ it
                    // has always worked, which is what identified the opener rather than the
                    // submenu as the fault.
                    //
                    // The **right button** still opens that same canvas list, which is where a
                    // user reaches for it anyway — `docs/04-ui-reference.md` §4 has the right
                    // button carrying everything the selection can do since it was written.
                });
            });
            pill = inner.response.rect;
            crate::theme::paint_glass_edge(ui.painter(), pill, palette, crate::theme::Backing::Canvas);
        });

    if let Some(spec) = palette.glass(crate::theme::Backing::Canvas)
        && pill.is_positive()
    {
        glass.push(crate::theme::GlassSurface {
            rect: pill,
            corner_radius: f32::from(crate::theme::radius::LARGE),
            opacity: spec.opacity,
        });
    }
    Pill { rect: pill, more_at }
}

/// What one pass of the pill leaves behind for the caller.
///
/// The `⋮` position is **returned rather than written through an out-parameter**, which is
/// what it used to be. Two reasons, and the second is the one that made the change worth
/// making: the caller was obliged to declare a `None` before the call and read it after, so
/// the pill's answer looked like the caller's own state; and eight parameters is one past
/// what clippy allows, so this crate could not be verified clean at all.
pub(crate) struct Pill {
    /// The rectangle the pill took, for keeping floating toolbars clear of it.
    pub(crate) rect: egui::Rect,
    /// Where to open the context menu, when `⋮` was clicked this pass.
    pub(crate) more_at: Option<egui::Pos2>,
}

/// The board's name: a label that becomes a field when clicked.
///
/// *"i want to be able to rename the board by clicking on the name there"* — and Miro
/// says so out loud with a **"Click to rename"** tooltip, which is worth copying: nothing
/// about a piece of text announces that it is a button.
///
/// The commit path is the one the library row already uses, so renaming from here renames
/// the file, the tab, the window title and the live document — `LibraryEvent::Rename` is
/// handled end to end by the app. A board with no path yet cannot be renamed and is drawn
/// as a plain label.
fn title(
    ui: &mut Ui,
    palette: Palette,
    header: &MenuHeader<'_>,
    rename: &mut Rename,
    events: &mut EventSink,
) {
    if header.title.is_empty() {
        return;
    }
    if header.flags.starred {
        let (rect, _) =
            ui.allocate_exact_size(egui::Vec2::splat(space::of(4)), egui::Sense::hover());
        Icon::StarFilled.paint(
            &ui.painter().clone(),
            rect,
            palette.accent,
            crate::widgets::ICON_STROKE,
        );
    }

    if let Some(buffer) = rename.editing.as_mut() {
        let id = egui::Id::new("vellum-board-rename");
        let field = ui.add(
            egui::TextEdit::singleline(buffer)
                .id(id)
                .desired_width(space::of(40))
                .font(egui::TextStyle::Body),
        );
        // Read the focus edge **before** re-taking focus: `TextEdit` signals Enter by
        // surrendering focus and gives no other signal, and `request_focus` sets the
        // focused widget back synchronously. This is the same ordering `dialog.rs`
        // documents at length, and getting it backwards is why Enter did nothing there
        // for three rounds.
        let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let first_frame = !ui.memory(|m| m.has_focus(id));
        if !submitted && !cancelled {
            field.request_focus();
            if first_frame {
                // Select the seed so the first keystroke replaces it, which is what
                // Finder and every rename field do.
                crate::dialog::select_all(ui.ctx(), id, buffer);
            }
        }

        // Clicking away commits rather than discards. A rename is not a destructive
        // act, and losing a typed name to a stray click is the more annoying failure.
        let dismissed = field.lost_focus() && !submitted;
        if submitted || dismissed {
            let name = buffer.trim().to_owned();
            if let Some(path) = header.path
                && !name.is_empty()
                && name != header.title
            {
                events.push(UiEvent::Library(crate::event::LibraryEvent::Rename {
                    path: path.to_path_buf(),
                    title: name,
                }));
            }
            rename.editing = None;
        } else if cancelled {
            rename.editing = None;
        }
        return;
    }

    let label = ui
        .add(egui::Label::new(egui::RichText::new(header.title).color(palette.text)).sense(
            if header.path.is_some() { egui::Sense::click() } else { egui::Sense::hover() },
        ));
    if header.path.is_some() {
        if label.on_hover_text("Click to rename").clicked() {
            rename.editing = Some(header.title.to_owned());
        }
    } else {
        // An unsaved board has no file to rename. Saying so beats a dead click.
        label.on_hover_text("Save the board before renaming it");
    }

    if header.dirty {
        // A dot rather than the word "Edited": the same information in a tenth of the
        // width, and it does not move the title when it appears.
        ui.label(egui::RichText::new("•").color(palette.accent)).on_hover_text("Unsaved changes");
    }
}



/// Draws a run of rows — a menu's or a submenu's, which are the same thing.
pub(crate) fn entries(
    ui: &mut Ui,
    palette: Palette,
    rows: &[Entry],
    cmd_ctx: &CommandContext,
    header: &MenuHeader<'_>,
    events: &mut EventSink,
) {
    // A menu is as wide as its widest row, and its rows are as wide as the widest
    // thing in them — which, without a ceiling, is the hairline separator, since that
    // takes `available_width` and a popup's available width is most of the window.
    // The floor keeps the shortcut column from crowding short menus.
    ui.set_min_width(space::of(58));
    ui.set_max_width(space::of(90));
    for row in rows {
        match row {
            Entry::Separator => {
                ui.add_space(space::UNIT);
                crate::widgets::hairline(ui, palette);
                ui.add_space(space::UNIT);
            }
            Entry::Item(command) => menu_item(ui, palette, *command, cmd_ctx, header, events),
            Entry::Sub(sub) => submenu(ui, palette, *sub, cmd_ctx, header, events),
        }
    }
}

pub(crate) fn submenu(
    ui: &mut Ui,
    palette: Palette,
    sub: Submenu,
    cmd_ctx: &CommandContext,
    header: &MenuHeader<'_>,
    events: &mut EventSink,
) {
    let available = sub.availability(cmd_ctx);
    let button = row_button(sub.title()).min_size(vec2(row_width(ui), 0.0));

    let response = match available {
        // A submenu that cannot open is drawn as a plain disabled row rather than as
        // a submenu button, so the pointer does not sit on it waiting for a panel
        // that is never coming.
        Availability::Disabled(why) => ui.add_enabled(false, button).on_disabled_hover_text(why),
        Availability::Enabled => {
            egui::containers::menu::SubMenuButton::from_button(button)
                .ui(ui, |ui| match sub {
                    Submenu::MoveToSpace => move_to_space(ui, palette, header, events),
                    Submenu::Background => background(ui, palette, header, events),
                    Submenu::Grid => grid(ui, palette, cmd_ctx, header, events),
                    Submenu::GridColor => grid_color(ui, palette, header, events),
                    Submenu::GridOpacity => grid_opacity(ui, palette, header, events),
                    Submenu::Transparency => transparency(ui, palette, header, events),
                    Submenu::Accent => accent(ui, palette, header, events),
                    Submenu::AgentDisplay => agent_display(ui, palette, header, events),
                    Submenu::Providers => providers(ui, palette, header, events),
                    Submenu::Export | Submenu::Arrange | Submenu::Agent => {
                        entries(ui, palette, sub.entries(), cmd_ctx, header, events);
                    }
                })
                .0
        }
    };

    // The disclosure arrow, drawn rather than set as text: egui's default is the `⏵`
    // dingbat, which comes from the emoji fallback on both platforms Velm ships on
    // — and `docs/05-design-language.md` §2 rules emoji out as iconography anyway.
    let box_ = egui::Rect::from_center_size(
        egui::pos2(response.rect.right() - space::of(2), response.rect.center().y),
        egui::Vec2::splat(space::of(3)),
    );
    let ink = if available.is_enabled() { palette.muted } else { palette.faint };
    Icon::ChevronRight.paint(&ui.painter().clone(), box_, ink, crate::widgets::ICON_STROKE);
}

/// One of the four top-level groups — Board, Edit, View, Preferences — as a submenu row.
///
/// The `Menu` counterpart of [`submenu`], which does the same for a [`Submenu`]. Kept
/// separate rather than generalised over both: the two enums answer different questions
/// (a `Menu` is always available and always has entries; a `Submenu` can be disabled with
/// a reason and three of the six draw their own bespoke contents), and one function taking
/// a trait object to serve both would be longer than the two.
///
/// No disabled state, no reason string, and deliberately so. A top-level group is a
/// heading, not an action: Edit is never *unavailable*, its rows are — and each of those
/// already carries its own reason, which is what `docs/04-ui-reference.md` §4's "anything
/// not implemented is disabled with a tooltip naming what is missing" asks for.
/// Opens the `⋮` popup, for `--screenshot`.
///
/// A diagnostic, like `--open-dialog`: a menu exists only between the click that opens it and
/// the click that dismisses it, so the surface `docs/04-ui-reference.md` §4 specifies in the
/// most detail was one nothing could photograph. The popup re-arms itself every pass it draws,
/// so once opened it survives to the screenshot frame.
///
/// # It opens the popup, not a named group inside it — and that is a limit, not an oversight
///
/// Expanding *Board* or *View* was tried and withdrawn. egui keys a submenu on
/// `ui.next_auto_id()`, which only exists inside `menu_group`, and `SubMenuButton::ui`
/// re-derives `MenuState::open_item` from the hover it just resolved — so a write before the
/// call is clobbered within the frame and a write after is clobbered by the next one.
/// `Memory::set_everything_is_visible` would force it, and is worse than not having it: it
/// opens every popup **and every tooltip** in the window, so the photograph would show an
/// interface that never occurs. A flag whose name promises a group it cannot deliver is worse
/// than a flag that says what it does.
pub fn force_open_menu(ctx: &egui::Context) {
    egui::Popup::open_id(ctx, egui::Id::new("vellum-main-menu"));
}

fn menu_group(
    ui: &mut Ui,
    palette: Palette,
    menu: Menu,
    cmd_ctx: &CommandContext,
    header: &MenuHeader<'_>,
    events: &mut EventSink,
) {
    let button = row_button(menu.title()).min_size(vec2(row_width(ui), 0.0));
    // **A diagnostic hook, and it has to be here rather than in the app.** `--menu NAME` asks
    // for one of these four groups open so `--screenshot` can photograph it; egui keys a
    // submenu's open state on `ui.next_auto_id()`, which only exists at this call site, so
    // there is no way to reach it from outside this function. Read *before* the button is
    // built, because building it advances the auto id.
    //
    // The blunt alternative, `Memory::set_everything_is_visible`, is worse than no hook: it
    // opens every popup **and every tooltip** in the window at once, so the photograph would
    // show an interface that never occurs.
    let response = egui::containers::menu::SubMenuButton::from_button(button)
        .ui(ui, |ui| {
            ui.set_min_width(space::of(58));
            ui.set_max_width(space::of(90));
            entries(ui, palette, menu.entries(), cmd_ctx, header, events);
        })
        .0;

    // The same drawn chevron `submenu` uses, for the same reason: egui's default is the
    // `⏵` dingbat out of the emoji fallback, and `docs/05` §2 rules emoji out as
    // iconography. A group that looked different from a nested submenu would also say
    // the two behave differently, which they do not.
    let box_ = egui::Rect::from_center_size(
        egui::pos2(response.rect.right() - space::of(2), response.rect.center().y),
        egui::Vec2::splat(space::of(3)),
    );
    Icon::ChevronRight.paint(
        &ui.painter().clone(),
        box_,
        palette.muted,
        crate::widgets::ICON_STROKE,
    );
}

/// Board ▸ Background — the canvas's own colour and pattern.
///
/// *"i want to be able to select backgrounds for the baords"*. Miro puts this under
/// Board ▸ Background colour and `docs/04-ui-reference.md` §4 flags it as cheap and
/// worth having. It used to be a single command that could only report that the
/// document had nowhere to put the answer.
///
/// The pattern rows come first because that is the choice most boards make and never
/// revisit, and the swatches under them are `crate::color::SWATCHES` — the same grid
/// every other colour control in the app offers, so a board tinted to match a sticky
/// really does match it. *Default* is the first swatch and is an absence rather than
/// a colour: a board that never chose one keeps following the palette.
/// View ▸ Grid — none, lines or dots, named as Miro names them.
///
/// *"add these grid options and make the dotted default."* Velm already had all four patterns
/// and `Dots` already *was* the default; what it did not have was Miro's shape — a short list
/// of named choices with the current one marked, under View, separate from the board's colour.
/// They had been sharing one submenu, which put "what the board is" and "what is drawn over it"
/// in the same list.
///
/// `Crosses` is offered too, which Miro has no equivalent for. It costs a row and it is already
/// implemented; dropping a working option to match a competitor's list exactly would be
/// copying for its own sake.
pub(crate) fn grid(
    ui: &mut Ui,
    palette: Palette,
    cmd_ctx: &CommandContext,
    header: &MenuHeader<'_>,
    events: &mut EventSink,
) {
    ui.set_min_width(space::of(50));
    let current = header.grid;
    for pattern in Pattern::ALL {
        let response = ui.add(row_button(pattern.label()).min_size(vec2(row_width(ui), 0.0)));
        if current.pattern == pattern {
            tick(ui, palette, &response);
        }
        if response.clicked() {
            events.push(UiEvent::GridChanged(GridSettings { pattern, ..current }));
        }
    }
    // The rule between the patterns and everything below is drawn here rather than led
    // with in `entries()`: a menu that *opens* with a separator is a rule against the top
    // of its own frame, which is what `no_menu_starts_or_ends_with_a_separator` exists to
    // stop, and these entries are a tail rather than a menu.
    ui.add_space(space::UNIT);
    ui.separator();
    ui.add_space(space::UNIT);
    // Snap to grid, then the two controls over the grid's own ink. Drawn from
    // `Submenu::Grid::entries()` rather than written out here, so the rows the
    // menu-coverage test counts are the rows a user actually sees — the four patterns
    // above are the only part of this submenu built from runtime state.
    entries(ui, palette, crate::command::Submenu::Grid.entries(), cmd_ctx, header, events);
}

/// View ▸ Grid ▸ Grid colour — what the dots, crosses or rules are drawn in.
///
/// *"give me the option to select the color and the transparency of each grid system"*.
/// This half writes the three channels; [`grid_opacity`] writes the alpha of the same
/// [`Background::grid_color`], which is one document field precisely so the two cannot
/// disagree about a pixel.
///
/// **The alpha is carried across, not reset.** Picking a new colour after setting the grid
/// to a quarter opacity has to leave it at a quarter opacity, or every trip through this
/// list undoes the slider next door.
fn grid_color(ui: &mut Ui, palette: Palette, header: &MenuHeader<'_>, events: &mut EventSink) {
    ui.set_min_width(space::of(48));
    let current = header.grid;
    let alpha = current.color.map_or(u8::MAX, |c| c.a);
    for choice in GRID_COLOURS {
        let wanted = choice.color.map(|c| vellum_doc::Color::rgba(c.r, c.g, c.b, alpha));
        let selected = match wanted {
            None => current.color.is_none(),
            // On the channels alone. The row is "which colour", and comparing the alpha
            // too would leave every row unticked the moment the slider was touched.
            Some(want) => {
                current.color.is_some_and(|c| (c.r, c.g, c.b) == (want.r, want.g, want.b))
            }
        };
        let response = ui.add(row_button(choice.name).min_size(vec2(row_width(ui), 0.0)));
        swatch(ui, palette, &response, choice.color, selected);
        let response = match choice.color {
            Some(color) => response.on_hover_text(crate::theme::numeric(color.to_hex())),
            None => response.on_hover_text("Follows the app's own grid colour"),
        };
        if response.clicked() {
            events.push(UiEvent::GridChanged(GridSettings { color: wanted, ..current }));
        }
    }
}

/// View ▸ Grid ▸ Grid opacity — how see-through the pattern is.
///
/// The same field as [`grid_color`], written on its alpha channel. Modelled on
/// [`transparency`] and emitting on **release** for the same reason: this reaches
/// `Board::set_background`, which commits to the CRDT, and a held slider reports a change
/// every frame.
///
/// **The floor is zero and that is deliberate**, unlike the glass slider's legibility floor.
/// A grid nobody can see is exactly the *No grid* row three above, so a user who drags this
/// to nothing has said something the menu already offers a word for — it is not a state they
/// can be trapped in, and refusing it would be refusing a setting that costs nothing.
fn grid_opacity(ui: &mut Ui, palette: Palette, header: &MenuHeader<'_>, events: &mut EventSink) {
    ui.set_min_width(space::of(56));
    let current = header.grid;
    // A board that has not chosen an ink is showing the theme's grid at full strength, so
    // that is what the slider must open on — starting at 0 would say the grid on screen is
    // invisible while the user is looking at it.
    let existing = current.color.unwrap_or(vellum_doc::Color::rgb(0, 0, 0));
    let mut percent = f32::from(current.color.map_or(u8::MAX, |c| c.a)) / 2.55;

    let slider = ui.add(
        egui::Slider::new(&mut percent, 0.0..=100.0)
            .suffix("%")
            .fixed_decimals(0)
            .trailing_fill(true)
            .text(""),
    );
    if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the slider's own range is 0..=100, so this lands inside a u8"
        )]
        let alpha = (percent * 2.55).round().clamp(0.0, 255.0) as u8;
        events.push(UiEvent::GridChanged(GridSettings {
            // Writing the alpha **commits the colour too**: a grid following the theme has
            // no colour to put an alpha on, so the theme's own near-black is what a
            // half-transparent grid is half of. `None` here would throw the setting away.
            color: Some(vellum_doc::Color::rgba(existing.r, existing.g, existing.b, alpha)),
            ..current
        }));
    }

    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new("Applies to every board.")
            .color(palette.faint)
            .size(crate::theme::text::LABEL),
    );
}

/// The circle in a colour row's tick column, and the ring that marks the chosen one.
///
/// Shared by the board-colour list and the grid-colour list, which drew the same three
/// primitives from two copies until the second one existed. `None` shows the canvas, since
/// that is what "follows the app" looks like on the surface behind it.
fn swatch(
    ui: &Ui,
    palette: Palette,
    response: &egui::Response,
    color: Option<vellum_doc::Color>,
    selected: bool,
) {
    let box_ = egui::Rect::from_center_size(
        egui::pos2(response.rect.left() + space::of(3), response.rect.center().y),
        egui::Vec2::splat(space::of(4)),
    );
    let shown = color.map_or(palette.canvas, crate::color::to_egui);
    ui.painter().circle(
        box_.center(),
        box_.width() * 0.5,
        shown,
        egui::Stroke::new(1.0, if selected { palette.accent } else { palette.border }),
    );
    if selected {
        ui.painter().circle_stroke(
            box_.center(),
            box_.width() * 0.5 + 2.0,
            egui::Stroke::new(1.5, palette.accent),
        );
    }
}

/// The colours a board's grid can be drawn in.
///
/// Shorter than [`BOARD_COLOURS`] and deliberately so: a canvas is a large area where a
/// pale tint reads as a mood, and a grid is a field of one-pixel marks where anything pale
/// is simply not there. So this is a ramp of neutrals — the useful axis for a datum field —
/// plus the three hues that stay legible at that size against a near-white board.
///
/// **`None` leads**, as the board list's default does, and it is what every board on disk
/// already has: it follows the app's own grid ink, which is where feedback 27's tuned
/// near-black lives.
pub(crate) const GRID_COLOURS: &[BoardColour] = &[
    BoardColour { name: "Default", color: None },
    BoardColour { name: "Black", color: Some(vellum_doc::Color::rgb(0x00, 0x00, 0x00)) },
    BoardColour { name: "Dark grey", color: Some(vellum_doc::Color::rgb(0x42, 0x42, 0x42)) },
    BoardColour { name: "Grey", color: Some(vellum_doc::Color::rgb(0x9E, 0x9E, 0x9E)) },
    BoardColour { name: "Light grey", color: Some(vellum_doc::Color::rgb(0xC8, 0xCE, 0xD2)) },
    BoardColour { name: "Blue", color: Some(vellum_doc::Color::rgb(0x2D, 0x6B, 0xD4)) },
    BoardColour { name: "Green", color: Some(vellum_doc::Color::rgb(0x2E, 0x7D, 0x4F)) },
    BoardColour { name: "Red", color: Some(vellum_doc::Color::rgb(0xC0, 0x39, 0x2B)) },
];

pub(crate) fn background(ui: &mut Ui, palette: Palette, header: &MenuHeader<'_>, events: &mut EventSink) {
    // **Not wider than the menu that opened it.** At `space::of(64)` this submenu was wider
    // than the `⋮` popup it hangs off, and egui re-solves a popup's position against the
    // space it has every pass — so it was repositioned on each one, never settled, and read
    // as the menu vanishing the moment the pointer reached Background colour.
    ui.set_min_width(space::of(58));
    let current = header.background;

    // **Named, not a grid of anonymous chips.** *"where i see the name of the colors, and the
    // default color just says default."* A swatch grid asks you to recognise a colour you have
    // not seen applied yet; Miro's list says *Light Gray (Default)* and *Light Blue*, so the
    // choice can be made by reading rather than by trying each one. The hex is still on the
    // hover, for anyone who wants it.
    for choice in BOARD_COLOURS {
        let selected = match choice.color {
            None => current.color.is_none(),
            Some(color) => current.color == Some(color),
        };
        let response = ui.add(row_button(choice.name).min_size(vec2(row_width(ui), 0.0)));
        // The swatch sits in the tick column, left of the name — it *is* the row's icon, and
        // a colour list whose colours are off to one side reads as a list of words. The ring
        // outside the chosen one is how Miro marks it; a tick would sit where the swatch is.
        swatch(ui, palette, &response, choice.color, selected);
        let response = match choice.color {
            // Hex in the numeric face: it is a value, not a word.
            Some(color) => response.on_hover_text(crate::theme::numeric(color.to_hex())),
            None => response.on_hover_text("Follows the app's own colour"),
        };
        if response.clicked() {
            events.push(UiEvent::BackgroundChanged(Background { color: choice.color, ..current }));
        }
    }
}

/// One row of the board-colour list.
pub(crate) struct BoardColour {
    pub(crate) name: &'static str,
    /// `None` is *follow the app's palette*, which is what makes a board take a new default
    /// when the palette moves instead of freezing today's value into the document.
    pub(crate) color: Option<vellum_doc::Color>,
}

/// The colours a board can be, named, in Miro's own order.
///
/// **`None` leads, labelled as the default**, because it is both the value a new board has and
/// the only one that keeps following the palette. Every other entry freezes a colour into the
/// document — which is correct when it is chosen deliberately and wrong as a starting point.
///
/// The greys are Miro's; the tints are the board palette's own sticky row lightened, so a
/// board tinted `Light Yellow` and a note coloured from the same family agree.
pub(crate) const BOARD_COLOURS: &[BoardColour] = &[
    BoardColour { name: "White", color: Some(vellum_doc::Color::rgb(0xFF, 0xFF, 0xFF)) },
    BoardColour { name: "Light grey (default)", color: None },
    BoardColour { name: "Grey", color: Some(vellum_doc::Color::rgb(0x9E, 0x9E, 0x9E)) },
    BoardColour { name: "Dark grey", color: Some(vellum_doc::Color::rgb(0x42, 0x42, 0x42)) },
    BoardColour { name: "Black", color: Some(vellum_doc::Color::rgb(0x11, 0x11, 0x11)) },
    BoardColour { name: "Light blue", color: Some(vellum_doc::Color::rgb(0xDF, 0xEE, 0xF7)) },
    BoardColour { name: "Light purple", color: Some(vellum_doc::Color::rgb(0xE6, 0xE2, 0xF7)) },
    BoardColour { name: "Light violet", color: Some(vellum_doc::Color::rgb(0xF0, 0xE4, 0xF4)) },
    BoardColour { name: "Light pink", color: Some(vellum_doc::Color::rgb(0xF9, 0xE4, 0xEC)) },
    BoardColour { name: "Light yellow", color: Some(vellum_doc::Color::rgb(0xFA, 0xF4, 0xD8)) },
    BoardColour { name: "Light green", color: Some(vellum_doc::Color::rgb(0xE4, 0xF3, 0xE2)) },
];

/// Preferences ▸ Transparency — how much of the board comes through the chrome.
///
/// *"make it slightly more translucent please or give me a setting inside preferences
/// so that i can change the how transparent it is"* — both, so the default moved and
/// this is here to move it further.
///
/// Shown as *transparency* and stored as *opacity*, which run opposite ways: the
/// number under the slider is what the user is asking for, not the field it lands in.
/// The range is deliberately not 0–100. `docs/05-design-language.md` §3a puts
/// legibility above the material, so it stops where text over a busy board stops
/// reading — see [`Palette::MIN_GLASS_OPACITY`] — and at the other end where the blur
/// has become imperceptible and *Translucent chrome* off is the honest setting, since
/// that also skips the blur pass instead of paying for one nobody can see.
fn transparency(ui: &mut Ui, palette: Palette, header: &MenuHeader<'_>, events: &mut EventSink) {
    ui.set_min_width(space::of(56));

    // Inverted, so dragging right makes the panel more see-through. A control labelled
    // "transparency" that gets more opaque as it grows is a control read backwards.
    let to_percent =
        |opacity: u8| f32::from(u8::MAX - opacity) / f32::from(u8::MAX - Palette::MIN_GLASS_OPACITY);
    // Clamped rather than trusted: `MenuHeader` derives `Default`, so a caller that has
    // not filled this in hands over 0, which is below the floor and would put the knob
    // off the end of its own track.
    let current = header.glass_opacity.clamp(Palette::MIN_GLASS_OPACITY, u8::MAX);
    let mut percent = to_percent(current) * 100.0;
    let ceiling = to_percent(Palette::MAX_GLASS_OPACITY) * 100.0;

    let slider = ui.add(
        egui::Slider::new(&mut percent, ceiling..=100.0)
            .suffix("%")
            .fixed_decimals(0)
            .trailing_fill(true)
            .text(""),
    );

    // On release, not on change. `Library::persist` is a synchronous `fs::write` and a
    // held slider reports a change every frame — about a hundred writes a second for as
    // long as it is dragged. The chrome still repaints live: the app is only told once.
    if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
        let span = f32::from(u8::MAX - Palette::MIN_GLASS_OPACITY);
        let opacity = f32::from(u8::MAX) - (percent / 100.0) * span;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped into u8's range on the line before"
        )]
        let opacity = opacity.clamp(f32::from(Palette::MIN_GLASS_OPACITY), f32::from(u8::MAX)) as u8;
        events.push(UiEvent::GlassOpacityChanged(opacity));
    }

    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new("Applies to the toolbar, menus and panels.")
            .color(palette.faint)
            .size(crate::theme::text::LABEL),
    );
}

/// Preferences ▸ Accent colour — which colour selection and the active tool wear.
///
/// *"in the settings i want you to have an option to the previous red and also blue if i
/// want to change it in the future"*.
///
/// Each row carries its own **swatch**, drawn in the colour it names, because "Teal" and
/// "Blue" are two words for a reader who has not seen either — and the swatch is the whole
/// content of the choice. It is drawn rather than described: `crate::theme::Accent::swatch`
/// is the same value [`Palette::with_accent`] installs, so the dot and the interface cannot
/// come to disagree about what "Blue" means.
fn accent(ui: &mut Ui, palette: Palette, header: &MenuHeader<'_>, events: &mut EventSink) {
    ui.set_min_width(space::of(36));
    for choice in crate::theme::Accent::ALL {
        let response = ui.add(row_button(choice.label()).min_size(vec2(row_width(ui), 0.0)));
        if choice == header.accent {
            tick(ui, palette, &response);
        }
        // Painted after the row so it lands on top of the row's own hover fill, and inside
        // the row's own rect so it travels with it.
        ui.painter().circle_filled(
            egui::pos2(response.rect.right() - space::of(3), response.rect.center().y),
            crate::theme::text::LABEL * 0.42,
            choice.swatch(),
        );
        if response.clicked() {
            events.push(UiEvent::AccentChanged(choice));
        }
    }

    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new("Selection, the active tool and primary buttons.")
            .color(palette.faint)
            .size(crate::theme::text::LABEL),
    );
}

/// Preferences ▸ Agent output — the mode a **new** agent node inherits.
///
/// Feature 2's second half, and the reason `AgentModel::display` is an `Option`: this row
/// moves every node that never chose for itself, and leaves the ones that did. The line
/// underneath says so, because "default" on its own does not distinguish a setting that
/// applies from here on from one that applies to everything.
fn agent_display(ui: &mut Ui, palette: Palette, header: &MenuHeader<'_>, events: &mut EventSink) {
    ui.set_min_width(space::of(40));
    for mode in vellum_agent::DisplayMode::ALL {
        let response = ui.add(row_button(mode.label()).min_size(vec2(row_width(ui), 0.0)));
        if mode == header.default_display {
            tick(ui, palette, &response);
        }
        if response.clicked() {
            events.push(UiEvent::DefaultDisplayModeChanged(mode));
        }
    }

    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new(
            "New agents, and every agent still set to inherit. A node with its own mode \
             keeps it.",
        )
        .color(palette.faint)
        .size(crate::theme::text::LABEL),
    );
}

/// Preferences ▸ Providers — which models this machine can reach, and who pays for each.
///
/// **Nothing here is a key and nothing here shows one.** `docs/07-agent-canvas.md` §8a puts
/// credentials in one file at mode `0600`; this list is told only whether one exists.
///
/// The rule that shapes the rows: a provider already running on a subscription the user
/// holds is **disabled**, with the reason as its tooltip — because signing in would collect
/// a credential nothing would ever read, and a control that succeeds and changes nothing is
/// worse than one that plainly cannot be pressed. That is feature 17 made visible: the whole
/// point of ACP is that `claude`, `codex` and `gemini` already hold the subscription.
fn providers(ui: &mut Ui, palette: Palette, header: &MenuHeader<'_>, events: &mut EventSink) {
    ui.set_min_width(space::of(52));
    ui.set_max_width(space::of(80));

    if header.providers.is_empty() {
        ui.label(
            egui::RichText::new("No providers have been looked for yet.")
                .color(palette.faint)
                .size(crate::theme::text::LABEL),
        );
        return;
    }

    for status in header.providers {
        // The billing word is the second column, in the same right-hand slot a shortcut
        // takes elsewhere in this bar — so the eye reads name, then cost, down the list.
        let button = row_button(status.provider.label())
            .min_size(vec2(row_width(ui), 0.0))
            .shortcut_text(crate::theme::numeric(status.billing()).color(palette.faint));

        if status.wants_a_key() {
            let response = ui.add(button).on_hover_text(if status.has_key {
                format!("{} — sign in again to replace the stored key", status.detail)
            } else {
                format!("{} — sign in to use it", status.detail)
            });
            if response.clicked() {
                events.push(UiEvent::ProviderSignIn(status.provider));
            }
            if status.has_key {
                tick(ui, palette, &response);
            }
        } else {
            // Disabled and explained, never absent: a provider missing from the list looks
            // like one Velm cannot reach at all.
            let why = if status.on_subscription() {
                format!("{} — runs on your subscription, so it needs no key", status.detail)
            } else if !status.provider.needs_api_key() {
                format!("{} — a model on your own machine needs no key", status.detail)
            } else {
                format!("{} — not found on this machine", status.detail)
            };
            let response = ui.add_enabled(false, button).on_disabled_hover_text(why);
            if status.on_subscription() {
                tick(ui, palette, &response);
            }
        }
    }

    if header.providers.iter().any(|status| status.has_key) {
        ui.add_space(space::UNIT);
        crate::widgets::hairline(ui, palette);
        ui.add_space(space::UNIT);
        for status in header.providers.iter().filter(|status| status.has_key) {
            let label = format!("Forget the {} key", status.provider.label());
            if ui.add(row_button(&label).min_size(vec2(row_width(ui), 0.0))).clicked() {
                events.push(UiEvent::ProviderForget(status.provider));
            }
        }
    }

    ui.add_space(space::UNIT);
    ui.label(
        egui::RichText::new(
            "Keys are stored in Velm's own credentials file and are never written to a \
             board, a transcript or a log.",
        )
        .color(palette.faint)
        .size(crate::theme::text::LABEL),
    );
}

/// Board ▸ Move to — the reference folders, with the one the board is already in
/// ticked, and a row that takes it out again.
fn move_to_space(
    ui: &mut Ui,
    palette: Palette,
    header: &MenuHeader<'_>,
    events: &mut EventSink,
) {
    ui.set_min_width(space::of(40));
    ui.set_max_width(space::of(72));
    let Some(path) = header.path else { return };
    let current = header.spaces.iter().find(|s| s.contains(path)).map(|s| s.name.as_str());

    for entry in header.spaces {
        let response = ui.add(row_button(entry.name.as_str()).min_size(vec2(row_width(ui), 0.0)));
        if current == Some(entry.name.as_str()) {
            tick(ui, palette, &response);
        }
        if response.clicked() {
            events.push(UiEvent::Library(LibraryEvent::MoveToSpace {
                path: path.to_path_buf(),
                space: Some(entry.name.clone()),
            }));
        }
    }

    ui.add_space(space::UNIT);
    crate::widgets::hairline(ui, palette);
    ui.add_space(space::UNIT);
    let none = ui
        .add_enabled(current.is_some(), row_button("No folder").min_size(vec2(row_width(ui), 0.0)))
        .on_disabled_hover_text("This board is not in a folder");
    if none.clicked() {
        events.push(UiEvent::Library(LibraryEvent::MoveToSpace {
            path: path.to_path_buf(),
            space: None,
        }));
    }
}

/// A menu row's button: the tick column reserved on the left, a floor under the width
/// so a column of shortcut hints lines up down the menu.
pub(crate) fn row_button(label: &str) -> egui::Button<'_> {
    // A drawn tick rather than a glyph: `docs/05-design-language.md` §2 rules out
    // emoji as iconography, and the check-mark dingbat renders from the emoji
    // fallback on both platforms Velm ships on. Reserving the column even when
    // unticked keeps every label in the menu on the same left edge.
    //
    // The tick column is a plain leading atom, **not** `Button::left_text`, which
    // is what this used to be. `left_text` pushes an `Atom::grow()` in front of the
    // text it is given, so the atom order came out as
    // `[" ", grow, label, grow, shortcut]` — an expanding spacer sitting *between*
    // the tick column and the label. Against a forced 220px width that spacer shoved
    // every label rightward by however much room the label did not need, so each row
    // began at a different x and the menu read as ragged. A tuple pushes both atoms
    // plainly, leaving `[" ", label, grow, shortcut]`: indent, label hard left,
    // shortcut hard right.
    // The leading atom is a sized placeholder, not a space character. A `" "` is only
    // as wide as the font makes it, which was narrower than the tick — so a ticked row
    // drew the check straight through the first letter of its own label. `tick` centres
    // a `space::of(3)` box on `left + space::of(2)`, reaching `space::of(3.5)`, so the
    // column reserves `space::of(4)` and the label clears it.
    let tick_column =
        egui::Atom::custom(egui::Id::new("vellum-menu-tick"), vec2(space::of(4), 0.0));
    egui::Button::new((tick_column, label)).min_size(vec2(ROW_MIN_WIDTH, 0.0))
}

/// The narrowest a menu row may be. A floor, not the width — see [`row_width`].
const ROW_MIN_WIDTH: f32 = space::of(55);

/// How wide a row must be to fill the menu it is in.
///
/// **A row that is narrower than its own menu leaves a dead strip down the right of every
/// line**, and that strip does not merely fail to act — the click lands on the frame, so it
/// does not close the menu either, and the menu appears frozen. It happened because
/// `ui.separator()` expands to the available width while `Button` sizes to its content:
/// the separators were 304pt, the rows 220pt, and the 84pt between them was inert on every
/// row of a menu whose whole job is to be clicked.
///
/// Taken from the `Ui` rather than fixed, so it follows whatever `set_min_width` /
/// `set_max_width` that particular menu chose — the bar's dropdowns and the right-button
/// menu are deliberately different widths.
pub(crate) fn row_width(ui: &Ui) -> f32 {
    ui.available_width().max(ROW_MIN_WIDTH)
}

pub(crate) fn tick(ui: &Ui, palette: Palette, response: &egui::Response) {
    let box_ = egui::Rect::from_center_size(
        egui::pos2(response.rect.left() + space::of(2), response.rect.center().y),
        egui::Vec2::splat(space::of(3)),
    );
    Icon::Check.paint(&ui.painter().clone(), box_, palette.accent, crate::widgets::ICON_STROKE);
}

pub(crate) fn menu_item(
    ui: &mut Ui,
    palette: Palette,
    command: Command,
    cmd_ctx: &CommandContext,
    header: &MenuHeader<'_>,
    events: &mut EventSink,
) {
    let ticked = command.is_toggle() && header.flags.is_on(command);
    let mut button = row_button(command.label()).min_size(vec2(row_width(ui), 0.0));
    if let Some(shortcut) = command.shortcut() {
        // The shortcut hint is a key, not a word — set in the numeric face so a
        // column of them lines up down the menu.
        button = button.shortcut_text(
            crate::theme::numeric(format_shortcut(shortcut, cfg!(target_os = "macos")))
                .color(palette.faint),
        );
    }

    let available = command.availability(cmd_ctx);
    let mut response = ui.add_enabled(available.is_enabled(), button);
    if let Some(why) = available.reason() {
        response = response.on_disabled_hover_text(why);
    }
    // …and the other kind of explanation: why a row that *can* be clicked is set the way it
    // is. Two preferences carry one, both of them off by default for a cost the switch
    // cannot show — see `Command::note`. On an enabled row only, so it never competes with
    // the disabled reason above.
    if let Some(note) = command.note()
        && available.is_enabled()
    {
        response = response.on_hover_text(note);
    }
    if ticked {
        tick(ui, palette, &response);
    }
    if response.clicked() {
        events.command(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandContext;
    use egui::Context;

    fn run(cmd_ctx: CommandContext, header: &MenuHeader<'_>) -> Vec<UiEvent> {
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut events = EventSink::default();
        let mut rename = Rename::default();
        let mut glass = Vec::new();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = show(
                ui,
                Palette::LIGHT,
                header,
                &cmd_ctx,
                &mut rename,
                &mut events,
                &mut glass,
            );
        });
        events.take()
    }

    #[test]
    fn the_bar_draws_with_and_without_a_board_and_emits_nothing_untouched() {
        assert!(run(CommandContext::default(), &MenuHeader::default()).is_empty());

        let spaces = [Space::new("Cars", ["/boards/site-plan.vellum".into()]).pinned()];
        let header = MenuHeader {
            grid: GridSettings::default(),
            title: "Engine bay",
            path: Some(Path::new("/boards/site-plan.vellum")),
            dirty: true,
            dark_theme: false,
            preference: ThemePreference::System,
            background: Background::default(),
            spaces: &spaces,
            glass_opacity: Palette::LIGHT.glass_opacity,
            accent: crate::theme::Accent::default(),
            default_display: vellum_agent::DisplayMode::Clean,
            providers: &[],
            flags: MenuFlags {
                link_previews: false,
                align_objects: true,
                snap_to_grid: true,
                minimap_visible: true,
                presenting: false,
                starred: true,
                translucent: true,
                properties_panel: false,
                agent_raw: false,
                browser_nodes: false,
                worktrees: false,
            },
        };
        assert!(
            run(
                CommandContext {
                    board_open: true,
                    board_saved: true,
                    selected: 3,
                    spaces: 1,
                    ..CommandContext::default()
                },
                &header,
            )
            .is_empty()
        );
    }

    /// *"i want to be able to select backgrounds for the baords"*. Board ▸ Background
    /// replaced a command that could only report that the document had nowhere to put
    /// the answer, so the submenu has to be reachable and its rows have to act.
    #[test]
    fn the_background_submenu_offers_every_pattern_and_the_palette() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let header = MenuHeader {
            grid: GridSettings::default(),
            title: "Engine bay",
            background: Background { color: None, pattern: Pattern::Dots },
            ..MenuHeader::default()
        };
        let cmd_ctx = CommandContext { board_open: true, ..CommandContext::default() };

        assert!(Submenu::Background.availability(&cmd_ctx).is_enabled());
        assert_eq!(Submenu::Background.parent(), Menu::Board);
        assert!(
            Menu::Board.entries().contains(&Entry::Sub(Submenu::Background)),
            "the submenu is not on the Board menu"
        );

        let mut events = EventSink::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            background(ui, Palette::LIGHT, &header, &mut events);
        });
        assert!(events.take().is_empty(), "a passive frame chose a background");

        // Every pattern is offered, and every one of them is a real document value.
        for pattern in Pattern::ALL {
            assert!(!pattern.label().is_empty());
            assert_eq!(Pattern::from_tag(pattern.tag()), Some(pattern));
        }
        // …and the swatch grid is the same one every other colour control offers, so
        // a board tinted to match a sticky really does match it.
        assert!(!crate::color::SWATCHES.is_empty());
    }

    /// The provider list says who pays, and never says what the key is.
    ///
    /// Two rules, and the first is feature 17's whole point: a provider running through a
    /// CLI the user is already signed in to needs **no key**, so offering *Sign in* on it
    /// would collect a credential nothing would ever read. The second is
    /// `docs/07-agent-canvas.md` §8a — this crate is told *whether* a key exists and is
    /// never told what it is, so there is nothing here that could be shown by accident.
    #[test]
    fn the_provider_rows_say_who_pays_and_never_show_a_key() {
        let installed = ProviderStatus {
            provider: vellum_agent::Provider::Claude,
            available: true,
            has_key: false,
            detail: "claude 2.1.4".to_owned(),
        };
        assert!(installed.on_subscription());
        assert!(!installed.wants_a_key(), "a held subscription needs no key");
        assert_eq!(installed.billing(), "Subscription");

        // …and the same provider with its CLI missing does need one. `supports_subscription`
        // says a CLI *exists to delegate to*, not that this machine has it — reporting an
        // uninstalled `claude` as configured is the one lie this row exists to avoid.
        let missing = ProviderStatus { available: false, ..installed.clone() };
        assert!(!missing.on_subscription());
        assert!(missing.wants_a_key());
        assert_eq!(missing.billing(), "No key yet");

        let local = ProviderStatus {
            provider: vellum_agent::Provider::Local,
            available: true,
            has_key: false,
            detail: "localhost:11434".to_owned(),
        };
        assert!(!local.wants_a_key(), "a model on your own GPU needs no key");
        assert_eq!(local.billing(), "Your own machine");

        let kimi = ProviderStatus {
            provider: vellum_agent::Provider::Kimi,
            available: true,
            has_key: true,
            detail: "api.moonshot.ai".to_owned(),
        };
        assert!(kimi.wants_a_key());
        assert_eq!(kimi.billing(), "API key · billed per token");

        // The whole list draws, and a passive frame chooses nothing.
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let all = [installed, missing, local, kimi];
        let header = MenuHeader { providers: &all, ..MenuHeader::default() };
        let mut events = EventSink::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            providers(ui, Palette::LIGHT, &header, &mut events);
        });
        assert!(events.take().is_empty(), "a passive frame signed in to something");

        // Nothing in this module can name a key, because nothing in this module is given
        // one. Asserted on the type rather than on the drawing: a field that does not exist
        // cannot be printed by a future edit either.
        let empty = MenuHeader::default();
        assert!(empty.providers.is_empty(), "the default roster is empty, not invented");
    }

    /// The app-wide default display mode — feature 2's second half. The rows tick what is
    /// current and say, in as many words, that a node with its own mode keeps it.
    #[test]
    fn the_default_output_mode_offers_both_and_ticks_the_current_one() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let header = MenuHeader {
            default_display: vellum_agent::DisplayMode::Raw,
            ..MenuHeader::default()
        };
        let mut events = EventSink::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            agent_display(ui, Palette::LIGHT, &header, &mut events);
        });
        assert!(events.take().is_empty(), "a passive frame changed the default");
        assert_eq!(vellum_agent::DisplayMode::ALL.len(), 2);
        assert_eq!(Submenu::AgentDisplay.parent(), Menu::Preferences);
    }

    #[test]
    fn only_the_toggle_commands_ever_show_a_tick() {
        let flags = MenuFlags {
            link_previews: true,
            align_objects: true,
            snap_to_grid: true,
            minimap_visible: true,
            presenting: true,
            starred: true,
            translucent: true,
            properties_panel: true,
            agent_raw: true,
            browser_nodes: true,
            worktrees: true,
        };
        for command in Command::ALL {
            assert_eq!(
                flags.is_on(*command),
                command.is_toggle(),
                "{command:?} disagrees with its toggle flag"
            );
        }
    }
}
