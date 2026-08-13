//! The menu the right button opens, and the one behind the context bar's `⋮`.
//!
//! *"in miro i can right click and i get all of these options … almost all controls
//! appear right above the what i right clicked"*. Two halves answer that: this module
//! and [`crate::context_bar`]. The bar carries the handful of controls the selection
//! is most likely to want; everything else is a row here, which is exactly the split
//! Miro makes.
//!
//! # Two menus, one renderer
//!
//! What is under the pointer decides which list is drawn — [`ContextTarget`] — and
//! nothing else about it changes. Both are drawn by [`crate::menu::entries`]-shaped
//! rows so a command's label, its shortcut hint, its tick and its *reason for being
//! disabled* are the same here as in the menu bar. That is not tidiness: a context
//! menu built from its own literals is a second copy of the command table, and the
//! copy is what goes stale.
//!
//! # Why the rows are data
//!
//! [`rows`] answers a `Vec<Row>` with no `egui` in sight, so the whole question of
//! *which* rows a selection gets is testable without a window — the same separation
//! [`crate::selection`] makes for the panel. The drawing below is then a fold over
//! that list with no decisions left in it.
//!
//! # Closing
//!
//! A context menu that will not go away is worse than none. It closes on: a click
//! outside it, Escape, any command it emits, and a selection that changed underneath
//! it. The last one matters — deleting the selection from this menu must not leave a
//! menu behind pointing at nothing.

use egui::{Context, Id, Order, Pos2, Rect};

use crate::command::{Command, CommandContext, Submenu};
use crate::event::{EventSink, StyleEdit, UiEvent};
use crate::menu::MenuHeader;
use crate::selection::PanelModel;
use crate::theme::{Backing, Palette, floating_frame_over, space};
use vellum_doc::CardMode;

/// What the pointer was over when the right button came up.
///
/// Resolved by the app, not here: only it knows what the scene holds. A right-click
/// that lands on an *unselected* item selects it first and arrives as
/// [`Self::Selection`], which is what every canvas tool does and what stops the menu
/// acting on something the user cannot see they had selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextTarget {
    /// Empty board. The rows are about the canvas and the clipboard.
    #[default]
    Canvas,
    /// The selection. The rows act on it.
    Selection,
}

/// One row, before anything is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Command(Command),
    Separator,
    Sub(Submenu),
    /// A link card's three display forms, as a nested list. Not a [`Submenu`] because
    /// it emits a [`StyleEdit`] rather than a command, and because it is drawn from
    /// the selection's current mode so it can tick the one in force.
    CardMode,
    /// Open the one selected card's page in the browser. Carries no URL — the
    /// renderer reads it from the model, which is the only place it is known.
    OpenPage,
    /// Copy the one selected card's address to the pasteboard. Carries no URL, for the
    /// same reason [`Row::OpenPage`] does not.
    CopyLink,
    /// Show the one selected note's `.md` file. Not a [`Command`] for the same reason
    /// [`Row::OpenPage`] is not: it acts on a value only the model holds.
    RevealNote,
    /// Open the one selected browser node's page in the user's own browser.
    BrowserOpen,
}

/// Which rows a target gets.
///
/// Pure, and the single definition. The order is Miro's: the four clipboard verbs
/// first because they are what a right-click is usually for, then whatever is
/// specific to what was clicked, then structure, then the view.
pub fn rows(target: ContextTarget, model: &PanelModel, cmd_ctx: &CommandContext) -> Vec<Row> {
    let mut out = Vec::new();
    match target {
        ContextTarget::Selection => {
            out.push(Row::Command(Command::Copy));
            out.push(Row::Command(Command::Cut));
            out.push(Row::Command(Command::Duplicate));
            out.push(Row::Command(Command::Delete));

            // The Agent Canvas band. Every row here is an ordinary command, and all five
            // are also in Edit ▸ Agent — which is not tidiness but the rule
            // `every_command_offered_here_is_also_in_the_menu_bar` enforces: a verb that
            // exists only on the right button is a verb a user who does not right-click
            // never finds, and the shortcut sheet is generated from the menu tree.
            if model.has_agent() {
                out.push(Row::Separator);
                // One row, naming what pressing it will do — the same rule the lock row
                // follows. Both are still in Edit ▸ Agent for the mixed case, where
                // neither label is the whole truth.
                out.push(Row::Command(if cmd_ctx.any_agent_running {
                    Command::StopAgent
                } else {
                    Command::RunAgent
                }));
                out.push(Row::Command(Command::ToggleAgentRaw));
                out.push(Row::Command(Command::EditAgentRules));
                out.push(Row::Command(Command::EditAgentSchedule));
            }
            if model.has_note() && model.note_path().is_some() {
                out.push(Row::Separator);
                out.push(Row::RevealNote);
            }
            if model.has_browser() && model.browser_url().is_some() {
                out.push(Row::Separator);
                out.push(Row::BrowserOpen);
            }

            // A card's own rows, in its own band. Only when something selected is one:
            // `card_mode` is `Absent` for every other kind, which is the same test the
            // panel's Link section uses.
            if !model.card_mode.is_absent() {
                out.push(Row::Separator);
                if model.link_url.is_some() {
                    out.push(Row::OpenPage);
                    out.push(Row::CopyLink);
                }
                out.push(Row::CardMode);
                out.push(Row::Command(Command::FetchLinkPreviews));
            }

            out.push(Row::Separator);
            out.push(Row::Sub(Submenu::Arrange));
            // One row, not both. Miro shows *Lock* on an unlocked selection and
            // *Unlock* on a locked one; offering the pair here would mean one of them
            // is always greyed out, which is noise on a menu this short. Both are
            // still in Arrange ▸ for the mixed case, where neither label is the
            // whole truth.
            out.push(Row::Command(if cmd_ctx.any_locked { Command::Unlock } else { Command::Lock }));

            out.push(Row::Separator);
            out.push(Row::Command(Command::ZoomToSelection));
            out.push(Row::Command(Command::TogglePropertiesPanel));
        }
        ContextTarget::Canvas => {
            out.push(Row::Command(Command::Paste));
            out.push(Row::Command(Command::SelectAll));
            out.push(Row::Separator);
            out.push(Row::Command(Command::ZoomToFit));
            out.push(Row::Command(Command::ZoomActualSize));
            out.push(Row::Separator);
            out.push(Row::Command(Command::ToggleMinimap));
            out.push(Row::Separator);
            out.push(Row::Sub(Submenu::Background));
            // The whole grid control, not a show/hide. `Command::ToggleGrid` used to be
            // this row and is gone — see `app::canvas_pattern` for why an off switch that
            // only worked on a board with no grid was the wrong control.
            out.push(Row::Sub(Submenu::Grid));
            out.push(Row::Separator);
            out.push(Row::Command(Command::SetStartView));
        }
    }
    out
}

/// Where the menu is, if it is open at all.
#[derive(Debug, Default)]
pub struct ContextMenu {
    open: Option<Opened>,
}

#[derive(Debug, Clone, Copy)]
struct Opened {
    at: Pos2,
    target: ContextTarget,
    /// True on the frame it opened.
    ///
    /// The click that *opens* the menu is still in egui's input on the frame the menu
    /// first draws, and it landed outside a rectangle that did not exist yet — so
    /// without this the menu opens and closes in the same frame and never appears. It
    /// cost a round of "the right button does nothing".
    fresh: bool,
}

impl ContextMenu {
    /// Opens at a screen position. Re-opening moves it rather than stacking.
    pub const fn open(&mut self, at: Pos2, target: ContextTarget) {
        self.open = Some(Opened { at, target, fresh: true });
    }

    pub const fn close(&mut self) {
        self.open = None;
    }

    pub const fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// What the open menu is about, for a caller that needs to know whether the
    /// selection is live.
    pub const fn target(&self) -> Option<ContextTarget> {
        match self.open {
            Some(open) => Some(open.target),
            None => None,
        }
    }

    /// Draws it, and answers the rectangle it took.
    ///
    /// **Opaque, not glass**, and that is a correction rather than an oversight. It was
    /// [`Backing::Canvas`] and the user photographed the result: a menu opened over the
    /// reference board had a colourful blur bleeding through its top-right corner and read
    /// as broken. `docs/05-design-language.md` §3a settles this case in one line —
    /// *legibility wins over the material, every time* — and this is the surface with the
    /// least claim to the material anyway. Glass is worth paying for on the tool palette,
    /// where the board carrying on underneath is genuinely useful; a context menu is a dense
    /// list of words you are reading for half a second, and what is behind it is exactly
    /// what you have stopped looking at.
    ///
    /// It also removes an inconsistency nobody would have been able to name: the menu bar's
    /// own dropdowns are already opaque (egui lays them out, so their rectangles never reach
    /// [`Chrome::glass_surfaces`](crate::Chrome::glass_surfaces) to be blurred behind), so
    /// the same command list was translucent from the right button and opaque from the menu
    /// bar.
    pub(crate) fn show(
        &mut self,
        ctx: &Context,
        palette: Palette,
        model: &PanelModel,
        cmd_ctx: &CommandContext,
        header: &MenuHeader<'_>,
        events: &mut EventSink,
    ) -> Option<Rect> {
        let open = self.open?;

        // A menu about a selection that has gone — deleted from this very menu, or
        // cleared by something else — is pointing at nothing.
        if open.target == ContextTarget::Selection && model.count == 0 {
            self.open = None;
            return None;
        }

        let before = events.as_slice().len();
        let area = egui::Area::new(Id::new("velm-context-menu"))
            .fixed_pos(open.at)
            .order(Order::Foreground)
            // Keeps a menu opened near the bottom-right corner on screen. Without it
            // the rows run off the window and the last of them cannot be clicked.
            .constrain(true)
            .show(ctx, |ui| {
                let inner = floating_frame_over(palette, Backing::Panel).show(ui, |ui| {
                    // Narrower than the menu bar's: these lists are short and a
                    // context menu that is as wide as a File menu reads as a panel.
                    ui.set_min_width(space::of(46));
                    ui.set_max_width(space::of(76));
                    for row in rows(open.target, model, cmd_ctx) {
                        draw(ui, palette, row, model, cmd_ctx, header, events);
                    }
                });
                // No specular catch: that line is what separates a *material* from a plain
                // blur, and there is neither here. The frame's own hairline does the edge.
                inner.response.rect
            });
        let rect = area.inner;

        // Anything the menu emitted was the user finishing with it.
        if events.as_slice().len() != before {
            self.open = None;
            return Some(rect);
        }

        // `interact_pos` rather than the hover position: a click that lands on another
        // area still reports where the *press* was, which is what "outside" means here.
        let clicked_outside = ctx.input(|i| i.pointer.any_click())
            && ctx.pointer_interact_pos().is_some_and(|p| !rect.contains(p));
        if (clicked_outside && !open.fresh) || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.open = None;
        } else if let Some(open) = self.open.as_mut() {
            open.fresh = false;
        }
        Some(rect)
    }
}

fn draw(
    ui: &mut egui::Ui,
    palette: Palette,
    row: Row,
    model: &PanelModel,
    cmd_ctx: &CommandContext,
    header: &MenuHeader<'_>,
    events: &mut EventSink,
) {
    match row {
        Row::Separator => {
            ui.add_space(space::UNIT);
            crate::widgets::hairline(ui, palette);
            ui.add_space(space::UNIT);
        }
        Row::Command(command) => {
            crate::menu::menu_item(ui, palette, command, cmd_ctx, header, events);
        }
        Row::Sub(sub) => crate::menu::submenu(ui, palette, sub, cmd_ctx, header, events),
        Row::CardMode => card_mode(ui, palette, model, events),
        Row::OpenPage => {
            let Some(url) = model.link_url.clone() else { return };
            let row = crate::menu::row_button("Open page").min_size(egui::vec2(crate::menu::row_width(ui), 0.0));
            if ui.add(row).on_hover_text(url.clone()).clicked() {
                events.push(UiEvent::OpenLink(url));
            }
        }
        Row::CopyLink => {
            let Some(url) = model.link_url.clone() else { return };
            let row =
                crate::menu::row_button("Copy link").min_size(egui::vec2(crate::menu::row_width(ui), 0.0));
            if ui.add(row).on_hover_text(url.clone()).clicked() {
                events.push(UiEvent::CopyLink(url));
            }
        }
        Row::RevealNote => {
            let Some(path) = model.note_path().map(str::to_owned) else { return };
            let row = crate::menu::row_button("Show the file")
                .min_size(egui::vec2(crate::menu::row_width(ui), 0.0));
            if ui.add(row).on_hover_text(path.clone()).clicked() {
                events.push(UiEvent::RevealPath(path.into()));
            }
        }
        Row::BrowserOpen => {
            let Some(url) = model.browser_url().map(str::to_owned) else { return };
            let row = crate::menu::row_button("Open in my browser")
                .min_size(egui::vec2(crate::menu::row_width(ui), 0.0));
            if ui.add(row).on_hover_text(url.clone()).clicked() {
                events.push(UiEvent::OpenLink(url));
            }
        }
    }
}

/// *Show ▸* — a card's three forms, ticking the one in force.
///
/// A submenu rather than three top-level rows: it is one property with three values,
/// and three sibling rows would read as three separate actions.
fn card_mode(ui: &mut egui::Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    let button = crate::menu::row_button("Show")
        .min_size(egui::vec2(crate::menu::row_width(ui), 0.0));
    let response = egui::containers::menu::SubMenuButton::from_button(button)
        .ui(ui, |ui| {
            ui.set_min_width(space::of(30));
            for (mode, label) in
                [(CardMode::Link, "Row"), (CardMode::Card, "Card"), (CardMode::Large, "Large")]
            {
                let row = ui.add(
                    crate::menu::row_button(label)
                        .min_size(egui::vec2(crate::menu::row_width(ui), 0.0)),
                );
                if model.card_mode.value() == Some(&mode) {
                    crate::menu::tick(ui, palette, &row);
                }
                if row.clicked() {
                    events.style(StyleEdit::CardMode(mode));
                }
            }
        })
        .0;

    // The same drawn chevron `crate::menu::submenu` uses, for the same reason: egui's
    // default is the `⏵` dingbat, which resolves through the emoji fallback.
    let box_ = egui::Rect::from_center_size(
        egui::pos2(response.rect.right() - space::of(2), response.rect.center().y),
        egui::Vec2::splat(space::of(3)),
    );
    crate::icon::Icon::ChevronRight.paint(
        &ui.painter().clone(),
        box_,
        palette.muted,
        crate::widgets::ICON_STROKE,
    );
}

/// Every command a context menu offers must be reachable from the menu bar too.
///
/// Not a style rule — a discoverability one. A verb that exists *only* on the right
/// button is a verb a user who does not right-click never finds, and the shortcut
/// sheet is generated from the menu tree.
#[cfg(test)]
fn is_in_the_menu_bar(command: Command) -> bool {
    use crate::command::Entry;
    fn walk(entries: &[Entry], wanted: Command) -> bool {
        entries.iter().any(|entry| match entry {
            Entry::Item(c) => *c == wanted,
            Entry::Sub(sub) => walk(sub.entries(), wanted),
            Entry::Separator => false,
        })
    }
    crate::command::Menu::ALL.iter().any(|menu| walk(menu.entries(), command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{ItemFacet, LinkSummary, SelectionItem};
    use std::str::FromStr;
    use vellum_doc::{ItemId, Placement};

    fn id(n: i32) -> ItemId {
        ItemId::from_str(&format!("{n}@1")).expect("a valid id")
    }

    fn sticky(n: i32) -> SelectionItem {
        SelectionItem::new(id(n), ItemFacet::Sticky, Placement::new(0.0, 0.0, 100.0, 100.0))
    }

    fn card(n: i32, url: Option<&str>) -> SelectionItem {
        SelectionItem {
            link: Some(LinkSummary {
                mode: CardMode::Card,
                url: url.map(str::to_owned),
                provider: None,
                has_image: false,
            }),
            ..SelectionItem::new(id(n), ItemFacet::Link, Placement::new(0.0, 0.0, 100.0, 100.0))
        }
    }

    fn agent(n: i32, running: bool) -> SelectionItem {
        let own = vellum_agent::AgentRules::default();
        SelectionItem {
            agent: Some(crate::AgentSummary {
                role: "Reviewer".to_owned(),
                role_kind: vellum_agent::RoleKind::Worker,
                provider: None,
                inherited_provider: vellum_agent::ProviderChoice::new(
                    vellum_agent::Provider::Claude,
                ),
                display: None,
                inherited_display: vellum_agent::DisplayMode::Clean,
                working_dir: None,
                project_dir: None,
                worktree: crate::WorktreeState::Off,
                schedule: None,
                territory: None,
                spawn_cap: 0,
                context: Vec::new(),
                running,
                rules: vellum_agent::rules::resolve(
                    &vellum_agent::RuleFile::default(),
                    &vellum_agent::RuleFile::default(),
                    &own,
                    "Reviewer",
                ),
                own_rules: own,
                connected: Vec::new(),
                accepts_messages: true,
                voice: false,
            }),
            ..SelectionItem::new(id(n), ItemFacet::Agent, Placement::new(0.0, 0.0, 400.0, 300.0))
        }
    }

    fn ctx_for(selection: &[SelectionItem]) -> CommandContext {
        let locked = selection.iter().filter(|i| i.locked).count();
        let agents = selection.iter().filter_map(|i| i.agent.as_ref());
        let running = agents.clone().filter(|a| a.running).count();
        let agents_selected = agents.count();
        CommandContext {
            board_open: true,
            selected: selection.len(),
            any_locked: locked > 0,
            all_locked: locked > 0 && locked == selection.len(),
            agents_selected,
            any_agent_running: running > 0,
            all_agents_running: agents_selected > 0 && running == agents_selected,
            ..CommandContext::default()
        }
    }

    fn commands(rows: &[Row]) -> Vec<Command> {
        rows.iter().filter_map(|r| match r {
            Row::Command(c) => Some(*c),
            _ => None,
        })
        .collect()
    }

    /// The clipboard verbs are the reason a right-click exists. If they ever fall off
    /// this list the menu has stopped being worth opening.
    #[test]
    fn a_selection_gets_the_clipboard_verbs_first() {
        let selection = [sticky(1)];
        let model = PanelModel::derive(&selection);
        let rows = rows(ContextTarget::Selection, &model, &ctx_for(&selection));
        assert_eq!(
            &commands(&rows)[..4],
            &[Command::Copy, Command::Cut, Command::Duplicate, Command::Delete]
        );
    }

    /// A sticky is not a card, so none of a card's rows may appear on one. The whole
    /// point of deriving the list is that this cannot be got wrong by hand.
    #[test]
    fn only_a_card_gets_the_card_rows() {
        let plain = [sticky(1)];
        let model = PanelModel::derive(&plain);
        let plain_rows = rows(ContextTarget::Selection, &model, &ctx_for(&plain));
        assert!(!plain_rows.contains(&Row::CardMode));
        assert!(!plain_rows.contains(&Row::OpenPage));
        assert!(!plain_rows.contains(&Row::CopyLink));
        assert!(!commands(&plain_rows).contains(&Command::FetchLinkPreviews));

        let linked = [card(1, Some("https://example.com"))];
        let model = PanelModel::derive(&linked);
        let card_rows = rows(ContextTarget::Selection, &model, &ctx_for(&linked));
        assert!(card_rows.contains(&Row::CardMode));
        assert!(card_rows.contains(&Row::OpenPage));
        assert!(card_rows.contains(&Row::CopyLink));
        assert!(commands(&card_rows).contains(&Command::FetchLinkPreviews));
    }

    /// *Open page* and *Copy link* both act on one address. Two cards have two, and a card
    /// that arrived without one has none — neither is a thing either row can do, so
    /// neither is offered.
    #[test]
    fn open_page_needs_exactly_one_card_with_an_address() {
        for selection in [
            vec![card(1, None)],
            vec![card(1, Some("https://example.com")), card(2, Some("https://example.org"))],
        ] {
            let model = PanelModel::derive(&selection);
            let rows = rows(ContextTarget::Selection, &model, &ctx_for(&selection));
            assert!(!rows.contains(&Row::OpenPage), "offered Open with {} cards", selection.len());
            assert!(!rows.contains(&Row::CopyLink), "offered Copy with {} cards", selection.len());
            assert!(rows.contains(&Row::CardMode), "the mode rows still apply");
        }
    }

    /// One row, and it says what clicking it will do.
    #[test]
    fn the_lock_row_names_the_direction_it_will_move() {
        let unlocked = [sticky(1)];
        let model = PanelModel::derive(&unlocked);
        let open = commands(&rows(ContextTarget::Selection, &model, &ctx_for(&unlocked)));
        assert!(open.contains(&Command::Lock));
        assert!(!open.contains(&Command::Unlock));

        let locked = [SelectionItem { locked: true, ..sticky(1) }];
        let model = PanelModel::derive(&locked);
        let shut = commands(&rows(ContextTarget::Selection, &model, &ctx_for(&locked)));
        assert!(shut.contains(&Command::Unlock));
        assert!(!shut.contains(&Command::Lock));
    }

    /// The empty-canvas menu must not offer verbs that need a selection: every one of
    /// them would draw greyed out, which is a menu of things you cannot do.
    ///
    /// The clipboard is held as full, because *Paste* is the one row here whose
    /// enablement is about something other than the board — an empty clipboard greys
    /// it out and that is correct. Testing it against an empty one would be asserting
    /// that Paste does not belong on a canvas menu, which is the opposite of true.
    #[test]
    fn the_canvas_menu_offers_nothing_that_needs_a_selection() {
        let model = PanelModel::empty();
        let ctx = CommandContext {
            board_open: true,
            clipboard_has_content: true,
            ..CommandContext::default()
        };
        for command in commands(&rows(ContextTarget::Canvas, &model, &ctx)) {
            assert!(
                command.is_enabled(&ctx),
                "{:?} is offered on an empty canvas but cannot act there",
                command
            );
        }
    }

    /// An agent's right-click rows, and the one that names the direction it will move.
    ///
    /// The band appears because the selection *has* agent properties — a sticky must never
    /// be offered *Run*, which is the failure a kind-match introduces the first time
    /// somebody adds a node kind and forgets an arm.
    #[test]
    fn an_agent_gets_the_agent_verbs_and_a_sticky_does_not() {
        let idle = [agent(1, false)];
        let model = PanelModel::derive(&idle);
        let offered = commands(&rows(ContextTarget::Selection, &model, &ctx_for(&idle)));
        assert!(offered.contains(&Command::RunAgent));
        assert!(!offered.contains(&Command::StopAgent), "it is not running");
        assert!(offered.contains(&Command::ToggleAgentRaw));
        assert!(offered.contains(&Command::EditAgentRules));
        assert!(offered.contains(&Command::EditAgentSchedule));

        let live = [agent(1, true)];
        let model = PanelModel::derive(&live);
        let offered = commands(&rows(ContextTarget::Selection, &model, &ctx_for(&live)));
        assert!(offered.contains(&Command::StopAgent));
        assert!(!offered.contains(&Command::RunAgent), "one row, naming what it will do");

        let plain = [sticky(1)];
        let model = PanelModel::derive(&plain);
        let offered = commands(&rows(ContextTarget::Selection, &model, &ctx_for(&plain)));
        for agentish in [
            Command::RunAgent,
            Command::StopAgent,
            Command::ToggleAgentRaw,
            Command::EditAgentRules,
            Command::EditAgentSchedule,
        ] {
            assert!(!offered.contains(&agentish), "{agentish:?} was offered on a sticky");
        }
    }

    /// See [`is_in_the_menu_bar`].
    #[test]
    fn every_command_offered_here_is_also_in_the_menu_bar() {
        let selection = [card(1, Some("https://example.com"))];
        let model = PanelModel::derive(&selection);
        // Both agent states, because the running one offers a *different* command and a
        // fixture with only the idle one would leave `StopAgent` unchecked.
        let idle = [agent(2, false)];
        let live = [agent(3, true)];
        let both = [
            rows(ContextTarget::Selection, &model, &ctx_for(&selection)),
            rows(ContextTarget::Canvas, &PanelModel::empty(), &ctx_for(&[])),
            rows(ContextTarget::Selection, &PanelModel::derive(&idle), &ctx_for(&idle)),
            rows(ContextTarget::Selection, &PanelModel::derive(&live), &ctx_for(&live)),
        ];
        for command in both.iter().flatten().filter_map(|r| match r {
            Row::Command(c) => Some(*c),
            _ => None,
        }) {
            assert!(
                is_in_the_menu_bar(command),
                "{command:?} is only reachable from the right button"
            );
        }
    }
}
