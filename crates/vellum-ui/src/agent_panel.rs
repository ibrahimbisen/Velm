//! The inspector's rows for an agent node, and for the three kinds that keep it company.
//!
//! `docs/07-agent-canvas.md` §1's module map names this file. What it draws is the *long*
//! form: [`crate::context_bar`] carries the handful of controls a running agent is most
//! likely to want next — run, raw/clean, the provider — and everything else is here, which
//! is the same split the rest of the chrome already makes.
//!
//! # Which rows appear is a pure function
//!
//! [`rows`] answers a `Vec<AgentRow>` with no `egui` in it, so the whole question of what an
//! orchestrator gets that a worker does not is decided once and tested without a window.
//! Two rules shape it:
//!
//! - **Derived from the properties the selection has**, never from [`ItemFacet::Agent`].
//!   A note is not an agent and neither is a browser node; each one's rows come from its own
//!   summary being present.
//! - **The single-node configuration is single-selection.** A rule cascade, a schedule, a
//!   territory and a list of attached files have no shared value across two nodes, and a
//!   control that wrote one into both would lose one of them silently. The four properties
//!   that *do* fold — role kind, provider, display mode, running — are drawn for any number.
//!
//! # Provenance is read, never re-derived
//!
//! The rules block shows *inherited* against *set here* out of
//! [`ResolvedRules::rows`](vellum_agent::ResolvedRules::rows) and
//! [`Layer::label`](vellum_agent::Layer::label) — the record resolution itself produced.
//! `docs/07-agent-canvas.md` §7 requires exactly that, and this repository has twice paid
//! for a second derivation drifting away from the first in the direction nobody was looking.
//!
//! [`ItemFacet::Agent`]: crate::ItemFacet::Agent

use crate::command::{Command, CommandContext};
use crate::event::{AgentEdit, EventSink, UiEvent};
use crate::icon::Icon;
use crate::selection::{Field, PanelModel};
use crate::theme::{Palette, numeric, space};
use crate::widgets::{
    CONTROL_WIDTH, Segment, hairline, icon_button, mixed_placeholder, readout, row, section_header,
    segmented,
};
use egui::Ui;
use vellum_agent::{DisplayMode, NoteScope, Provider, ProviderChoice, RoleKind};
use vellum_doc::ItemId;

/// One row of the agent section, before anything is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRow {
    // ----- an agent node ----------------------------------------------------
    /// The free-text role label — feature 5. What the agent is *for*, in the user's words,
    /// folded into its system context at the agent layer.
    Role,
    /// Worker, orchestrator or meta.
    Kind,
    /// Which provider and model this node runs on — feature 16, per node, with the billing
    /// stated. One board can have one agent on a subscription, one on a local GPU and one
    /// billed per token, and the row is where that becomes visible.
    Provider,
    /// Raw or clean, with a third state that inherits the app-wide default — feature 2.
    Display,
    WorkingDir,
    /// Whether this agent has a git worktree of its own — feature 4, recorded per node
    /// because the worktree is this agent's even though the switch is the project's.
    Worktree,
    /// A one-line summary and the way into the editor — feature 10.
    Schedule,
    /// An orchestrator's region. Only for a role that can spawn.
    Territory,
    /// An orchestrator's hard cap on simultaneous sub-agents. Never unbounded.
    SpawnCap,
    /// Whether connected agents may message this one.
    Messages,
    /// The files, pages and media attached as context.
    Context,
    /// The three-layer cascade, with each field's provenance — feature 11.
    Rules,

    // ----- a note -----------------------------------------------------------
    NotePath,
    NoteScope,
    /// Whether the file is on disk, and whether it and the board have diverged.
    NoteState,

    // ----- a file tree ------------------------------------------------------
    TreeRoot,
    /// Which agent the tree is scoped to — feature 7's hard requirement.
    TreeOwner,
    TreeIgnored,

    // ----- a browser node ---------------------------------------------------
    BrowserUrl,
    /// Whether this page may run an engine, and whether engines are permitted at all.
    BrowserLive,
}

/// Which rows a selection gets. Pure; see the module header.
pub fn rows(model: &PanelModel) -> Vec<AgentRow> {
    let mut out = Vec::new();

    if model.has_agent() {
        // A role is a sentence about one node. Two agents share no label, and a field that
        // wrote one into both would rename them together.
        if model.agent.is_some() {
            out.push(AgentRow::Role);
        }
        // These three fold, so they apply to any number of selected agents.
        out.push(AgentRow::Kind);
        out.push(AgentRow::Provider);
        out.push(AgentRow::Display);

        if let Some(agent) = &model.agent {
            out.push(AgentRow::WorkingDir);
            out.push(AgentRow::Worktree);
            out.push(AgentRow::Schedule);
            // A worker has no territory to spawn into and no cap, because it may not spawn
            // at all. Drawing the rows anyway would be two controls over a value nothing
            // reads — `RoleKind::may_spawn` is the one definition of that.
            if agent.manages() {
                out.push(AgentRow::Territory);
                out.push(AgentRow::SpawnCap);
            }
            out.push(AgentRow::Messages);
            out.push(AgentRow::Context);
            out.push(AgentRow::Rules);
        }
    }

    if model.has_note() {
        if model.note.is_some() {
            out.push(AgentRow::NotePath);
        }
        out.push(AgentRow::NoteScope);
        if model.note.is_some() {
            out.push(AgentRow::NoteState);
        }
    }

    if model.has_file_tree() {
        if model.file_tree.is_some() {
            out.push(AgentRow::TreeRoot);
            out.push(AgentRow::TreeOwner);
        }
        out.push(AgentRow::TreeIgnored);
    }

    if model.has_browser() {
        if model.browser.is_some() {
            out.push(AgentRow::BrowserUrl);
        }
        out.push(AgentRow::BrowserLive);
    }

    out
}

/// The panel's memory for the three free-text fields.
///
/// The same buffer trick `crate::properties::PropertiesState` uses for the words of an item,
/// and for the same reason: the model is rebuilt from the document, the document is written
/// from the field, and a round trip through a CRDT is one frame long — so binding them
/// directly sends the caret to the end on every keystroke. The id is what makes selecting a
/// different node reload the buffer instead of typing one node's role into another.
#[derive(Debug, Default)]
pub struct AgentPanelState {
    role: Option<(ItemId, String)>,
    working_dir: Option<(ItemId, String)>,
    model_name: Option<(ItemId, String)>,
    browser_url: Option<(ItemId, String)>,
}

impl AgentPanelState {
    /// The live buffer for one field of one item, seeded from the document the first time
    /// that item is selected.
    fn buffer<'a>(
        slot: &'a mut Option<(ItemId, String)>,
        id: ItemId,
        current: &str,
    ) -> &'a mut String {
        let stale = !matches!(slot, Some((held, _)) if *held == id);
        if stale {
            *slot = Some((id, current.to_owned()));
        }
        // Seeded above whenever it did not already belong to this item, so this cannot fail.
        &mut slot.as_mut().expect("the buffer was seeded above").1
    }
}

pub(crate) fn show(
    ui: &mut Ui,
    palette: Palette,
    state: &mut AgentPanelState,
    model: &PanelModel,
    cmd_ctx: &CommandContext,
    events: &mut EventSink,
) {
    let rows = rows(model);
    if rows.is_empty() {
        return;
    }
    section_header(ui, palette, heading(model));
    for entry in rows {
        draw(ui, palette, state, entry, model, cmd_ctx, events);
    }
}

/// What the section is called, which is what the selection actually holds.
///
/// Four kinds share this section because they share a layer, not because they are the same
/// thing — so a note's section is headed *Note* rather than *Agent*, and a selection holding
/// two of them falls back to the layer's own name.
fn heading(model: &PanelModel) -> &'static str {
    match (model.has_agent(), model.has_note(), model.has_file_tree(), model.has_browser()) {
        (true, false, false, false) => "Agent",
        (false, true, false, false) => "Note",
        (false, false, true, false) => "File tree",
        (false, false, false, true) => "Browser",
        _ => "Agent canvas",
    }
}

fn draw(
    ui: &mut Ui,
    palette: Palette,
    state: &mut AgentPanelState,
    entry: AgentRow,
    model: &PanelModel,
    cmd_ctx: &CommandContext,
    events: &mut EventSink,
) {
    match entry {
        AgentRow::Role => {
            let (Some(agent), Some(id)) = (&model.agent, model.single_id) else { return };
            let buffer = AgentPanelState::buffer(&mut state.role, id, &agent.role);
            row(ui, palette, "Role", |ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(buffer)
                        .desired_width(CONTROL_WIDTH)
                        .hint_text("Reviewer, Builder, Researcher…"),
                );
                // On losing focus rather than on every keystroke — click away or press
                // Enter and the whole label is one undo step, which is what a name is.
                // `TextEdit` signals Enter by surrendering focus, so one test covers both.
                if field.lost_focus() {
                    events.agent(AgentEdit::Role(buffer.trim().to_owned()));
                }
            });
        }

        AgentRow::Kind => {
            row(ui, palette, "Kind", |ui| {
                let options = [
                    Segment::text(RoleKind::Worker, "Worker"),
                    Segment::text(RoleKind::Orchestrator, "Manager"),
                    Segment::text(RoleKind::Meta, "Meta"),
                ];
                if let Some(kind) = segmented(ui, palette, &model.agent_role_kind, &options) {
                    events.agent(AgentEdit::Kind(kind));
                }
            });
        }

        AgentRow::Provider => provider(ui, palette, state, model, events),

        AgentRow::Display => display(ui, palette, model, events),

        AgentRow::WorkingDir => {
            let (Some(agent), Some(id)) = (&model.agent, model.single_id) else { return };
            let inherited = agent.project_dir.clone().unwrap_or_else(|| "the board's folder".into());
            let current = agent.working_dir.clone().unwrap_or_default();
            let buffer = AgentPanelState::buffer(&mut state.working_dir, id, &current);
            row(ui, palette, "Folder", |ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(buffer)
                        .desired_width(CONTROL_WIDTH)
                        .hint_text(inherited.as_str()),
                );
                if field.lost_focus() {
                    let typed = buffer.trim();
                    events.agent(AgentEdit::WorkingDir(
                        (!typed.is_empty()).then(|| typed.to_owned()),
                    ));
                }
            });
            caption(ui, palette, &format!("Empty runs in {inherited}."));
        }

        AgentRow::Worktree => {
            let Some(agent) = &model.agent else { return };
            let on = agent.worktree.is_on();
            row(ui, palette, "Worktree", |ui| {
                if ui
                    .add(egui::Button::selectable(on, if on { "On" } else { "Off" }).frame(true))
                    .on_hover_text(if on {
                        "Turning this off leaves the worktree in place — it is removed \
                         deliberately, and never while it has uncommitted work in it."
                    } else {
                        "Give this agent its own git worktree, on its own branch."
                    })
                    .clicked()
                {
                    events.agent(AgentEdit::Worktree(!on));
                }
            });
            caption(ui, palette, &agent.worktree.label());
        }

        AgentRow::Schedule => {
            let Some(agent) = &model.agent else { return };
            let summary = agent
                .schedule
                .as_ref()
                .map_or_else(|| "Not scheduled".to_owned(), vellum_agent::Schedule::summary);
            row(ui, palette, "Schedule", |ui| {
                readout(ui, palette, summary);
            });
            command_row(ui, palette, Command::EditAgentSchedule, cmd_ctx, events);
        }

        AgentRow::Territory => {
            let Some(agent) = &model.agent else { return };
            row(ui, palette, "Territory", |ui| match &agent.territory {
                Some(region) if !region.is_empty() => {
                    readout(
                        ui,
                        palette,
                        format!("{:.0} × {:.0}", region.width, region.height),
                    );
                    if icon_button(ui, palette, Icon::Close, space::of(5), false)
                        .on_hover_text("Clear the region")
                        .clicked()
                    {
                        events.agent(AgentEdit::Territory(None));
                    }
                }
                // Reported rather than offered. A rectangle is drawn on the board, not
                // typed into a panel — four number fields for a region you can see is the
                // control nobody uses — and the gesture that draws one belongs to the app.
                _ => {
                    ui.add_enabled(false, egui::Button::new("Not set").frame(true))
                        .on_disabled_hover_text(
                            "Drag a region on the board while this orchestrator is selected. \
                             It may only spawn inside it.",
                        );
                }
            });
        }

        AgentRow::SpawnCap => {
            let Some(agent) = &model.agent else { return };
            let mut cap = agent.spawn_cap;
            row(ui, palette, "Spawn cap", |ui| {
                let drag = crate::theme::tabular(ui, |ui| {
                    ui.add(egui::DragValue::new(&mut cap).speed(1.0).range(1..=64))
                });
                if drag
                    .on_hover_text(
                        "The most sub-agents this one may have running at once. Enforced in \
                         Velm rather than in its instructions — a limit that lives only in a \
                         prompt is not a limit.",
                    )
                    .changed()
                {
                    events.agent(AgentEdit::SpawnCap(cap));
                }
            });
        }

        AgentRow::Messages => {
            let Some(agent) = &model.agent else { return };
            let on = agent.accepts_messages;
            row(ui, palette, "Messages", |ui| {
                if ui
                    .add(
                        egui::Button::selectable(on, if on { "Accepted" } else { "Muted" })
                            .frame(true),
                    )
                    .on_hover_text(
                        "Whether agents connected to this one may message it. Muting keeps \
                         the connector, which is what documents the relationship.",
                    )
                    .clicked()
                {
                    events.agent(AgentEdit::AcceptsMessages(!on));
                }
            });
        }

        AgentRow::Context => context(ui, palette, model, events),

        AgentRow::Rules => rules(ui, palette, model, cmd_ctx, events),

        AgentRow::NotePath => {
            let Some(note) = &model.note else { return };
            row(ui, palette, "File", |ui| {
                readout(ui, palette, note.path.clone()).on_hover_text(note.path.clone());
            });
            if note.on_disk
                && ui
                    .add(egui::Button::new("Show the file").frame(true))
                    .on_hover_text("A note is a real `.md` file; open it in any editor.")
                    .clicked()
            {
                events.push(UiEvent::RevealPath(note.path.clone().into()));
            }
        }

        AgentRow::NoteScope => {
            // **Only one direction is reachable from a panel, and the other is disabled
            // rather than dead.** Making a note private needs an *owner*, and nothing here
            // can pick one: which agent a note belongs to is which agent it is connected to,
            // and a connector is a board gesture. A button that flipped the label and
            // emitted nothing would be the exact failure `CLAUDE.md`'s font-family lesson
            // is about — a control that reports success and does nothing.
            let private = matches!(model.note_private, Field::Uniform(true));
            row(ui, palette, "Scope", |ui| {
                if model.note_private.is_mixed() {
                    mixed_placeholder(ui, palette);
                    return;
                }
                if private {
                    let owner = model.note.as_ref().and_then(|n| n.owner.clone());
                    if ui
                        .add(egui::Button::selectable(true, "Private").frame(true))
                        .on_hover_text(owner.map_or_else(
                            || "One agent only. Click to share it with every agent.".to_owned(),
                            |who| format!("{who} only. Click to share it with every agent."),
                        ))
                        .clicked()
                    {
                        events.agent(AgentEdit::NoteScope(NoteScope::Shared));
                    }
                } else {
                    ui.add_enabled(
                        false,
                        egui::Button::selectable(false, "Shared").frame(true),
                    )
                    .on_disabled_hover_text(
                        "Every agent on this board may read and write it. Connect the note to \
                         one agent to make it that agent's own.",
                    );
                }
            });
        }

        AgentRow::NoteState => {
            let Some(note) = &model.note else { return };
            let state = if note.conflicted {
                "Changed here and on disk — both were kept"
            } else if note.on_disk {
                "In step with the file"
            } else {
                "Not written yet"
            };
            row(ui, palette, "State", |ui| {
                ui.label(
                    egui::RichText::new(state)
                        .color(if note.conflicted { palette.warning } else { palette.muted }),
                );
            });
            if note.links > 0 {
                caption(ui, palette, &format!("Links to {} other note(s).", note.links));
            }
        }

        AgentRow::TreeRoot => {
            let Some(tree) = &model.file_tree else { return };
            row(ui, palette, "Root", |ui| {
                let shown =
                    if tree.root.is_empty() { "The board's folder" } else { tree.root.as_str() };
                readout(ui, palette, shown.to_owned());
            });
        }

        AgentRow::TreeOwner => {
            let Some(tree) = &model.file_tree else { return };
            row(ui, palette, "Scoped to", |ui| match &tree.agent {
                Some(agent) => {
                    readout(ui, palette, agent.clone());
                }
                None => {
                    ui.label(egui::RichText::new("Nobody in particular").color(palette.muted));
                }
            });
        }

        AgentRow::TreeIgnored => {
            let on = matches!(model.tree_show_ignored, Field::Uniform(true));
            row(ui, palette, "Ignored files", |ui| {
                if model.tree_show_ignored.is_mixed() {
                    mixed_placeholder(ui, palette);
                    return;
                }
                if ui
                    .add(
                        egui::Button::selectable(on, if on { "Shown" } else { "Hidden" })
                            .frame(true),
                    )
                    .on_hover_text(
                        "Files git ignores. Hidden by default: a build directory with 40,000 \
                         entries in it is not a project structure.",
                    )
                    .clicked()
                {
                    events.agent(AgentEdit::ShowIgnored(!on));
                }
            });
        }

        AgentRow::BrowserUrl => {
            let (Some(browser), Some(id)) = (&model.browser, model.single_id) else { return };
            let buffer = AgentPanelState::buffer(&mut state.browser_url, id, &browser.url);
            row(ui, palette, "Address", |ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(buffer)
                        .desired_width(CONTROL_WIDTH)
                        .hint_text("https://"),
                );
                if field.lost_focus() && !buffer.trim().is_empty() {
                    events.agent(AgentEdit::BrowserUrl(buffer.trim().to_owned()));
                }
            });
            if !browser.title.is_empty() {
                caption(ui, palette, &browser.title);
            }
        }

        AgentRow::BrowserLive => {
            let allowed = model.browser.as_ref().is_none_or(|b| b.allowed);
            let live = matches!(model.browser_live, Field::Uniform(true));
            row(ui, palette, "Page", |ui| {
                if model.browser_live.is_mixed() {
                    mixed_placeholder(ui, palette);
                    return;
                }
                // Two yeses, and the outer one is the app's. With browser nodes off, this
                // says so and names where the switch is rather than doing nothing when
                // pressed — `docs/07-agent-canvas.md` §0's third rule.
                let button =
                    egui::Button::selectable(live, if live { "Loaded" } else { "Not loaded" })
                        .frame(true);
                let response = ui.add_enabled(allowed, button);
                if allowed {
                    if response.clicked() {
                        events.agent(AgentEdit::BrowserLive(!live));
                    }
                } else {
                    response.on_disabled_hover_text(
                        "Browser nodes are off. Turn them on in Preferences ▸ Browser nodes; \
                         the row there says what one costs.",
                    );
                }
            });
            if let Some(url) = model.browser_url()
                && ui
                    .add(egui::Button::new("Open in my browser").frame(true))
                    .on_hover_text(url.to_owned())
                    .clicked()
            {
                events.push(UiEvent::OpenLink(url.to_owned()));
            }
        }
    }
}

/// Which provider and model this node runs on, and **who pays** — feature 16.
///
/// The billing word is [`ProviderChoice::summary`]'s, never composed here: it is the single
/// most useful thing to know before a long run, and a second derivation of it is how a node
/// on a subscription comes to be labelled as billed per token.
fn provider(
    ui: &mut Ui,
    palette: Palette,
    state: &mut AgentPanelState,
    model: &PanelModel,
    events: &mut EventSink,
) {
    let chosen = match &model.agent_provider {
        Field::Absent => return,
        Field::Mixed => None,
        Field::Uniform(choice) => Some(choice.clone()),
    };

    row(ui, palette, "Provider", |ui| {
        let selected = match &chosen {
            None => crate::widgets::MIXED.to_owned(),
            Some(None) => "Inherit".to_owned(),
            Some(Some(choice)) => choice.provider.label().to_owned(),
        };
        egui::ComboBox::from_id_salt("velm-agent-provider")
            .selected_text(selected)
            .width(CONTROL_WIDTH)
            .show_ui(ui, |ui| {
                // **Inherit is a real choice, not the absence of one.** A node that inherits
                // follows when the board's default moves; a node that names the same
                // provider does not. Collapsing them is what would make feature 16's
                // per-node promise quietly untrue.
                let inheriting = matches!(&chosen, Some(None));
                if ui.selectable_label(inheriting, "Inherit the board's default").clicked() {
                    events.agent(AgentEdit::Provider(None));
                }
                for provider in Provider::ALL {
                    let on = matches!(&chosen, Some(Some(c)) if c.provider == provider);
                    if ui.selectable_label(on, provider.label()).clicked() {
                        // The model name is kept when only the provider moved, because the
                        // two are usually changed together and losing a typed model id on
                        // every provider click would be a control that punishes exploring.
                        let model_name = match &chosen {
                            Some(Some(c)) => c.model.clone(),
                            _ => None,
                        };
                        let mut choice = ProviderChoice::new(provider);
                        choice.model = model_name;
                        events.agent(AgentEdit::Provider(Some(choice)));
                    }
                }
            });
    });

    // The billing line, and the whole reason this row is worth having.
    if let Some(agent) = &model.agent {
        caption(ui, palette, &agent.provider_summary());
    }

    // The model name, for one node. `None` is "whatever the provider or the delegated CLI
    // considers current", which is deliberately not a pinned id — a pinned default is wrong
    // within months and would override what the CLI already knows.
    let (Some(agent), Some(id)) = (&model.agent, model.single_id) else { return };
    let effective = agent.effective_provider();
    let current = effective.model.clone().unwrap_or_default();
    let buffer = AgentPanelState::buffer(&mut state.model_name, id, &current);
    row(ui, palette, "Model", |ui| {
        let field = ui.add(
            egui::TextEdit::singleline(buffer)
                .desired_width(CONTROL_WIDTH)
                .hint_text("Whatever is current"),
        );
        if field.lost_focus() {
            let typed = buffer.trim();
            let mut choice = effective.clone();
            choice.model = (!typed.is_empty()).then(|| typed.to_owned());
            events.agent(AgentEdit::Provider(Some(choice)));
        }
    });
}

/// Raw, clean, or inherit — and *inherit* is a third state rather than a way of spelling one
/// of the other two.
///
/// Feature 2's second half is that new agents inherit a preferred mode, which only means
/// anything if a node can *stay* inheriting: an inheriting node moves when the preference
/// moves and a node that named Clean does not. So the control has three segments, and the
/// line under it says what inheriting currently resolves to — otherwise the user has to open
/// Preferences to find out what they are looking at.
fn display(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    row(ui, palette, "Output", |ui| {
        let options = [
            Segment::text(None, "Inherit"),
            Segment::text(Some(DisplayMode::Clean), "Clean"),
            Segment::text(Some(DisplayMode::Raw), "Raw"),
        ];
        if let Some(choice) = segmented(ui, palette, &model.agent_display, &options) {
            events.agent(AgentEdit::Display(choice));
        }
    });

    let Some(agent) = &model.agent else { return };
    let line = match agent.display {
        None => format!(
            "Inheriting {} from Preferences ▸ Agent output.",
            agent.inherited_display.label().to_lowercase()
        ),
        Some(DisplayMode::Raw) => {
            "Every tool call, command and reasoning step.".to_owned()
        }
        Some(DisplayMode::Clean) => "The answer, and nothing else.".to_owned(),
    };
    caption(ui, palette, &line);
}

/// The files, pages and media this agent has been given.
fn context(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    let Some(agent) = &model.agent else { return };
    if agent.context.is_empty() {
        row(ui, palette, "Context", |ui| {
            ui.label(egui::RichText::new("Nothing attached").color(palette.muted));
        });
        caption(
            ui,
            palette,
            "Connect a note or a file tree to this agent, or drop a file on it.",
        );
        return;
    }

    row(ui, palette, "Context", |ui| {
        readout(ui, palette, format!("{} attached", agent.context.len()));
    });
    for (index, source) in agent.context.iter().enumerate() {
        ui.horizontal(|ui| {
            let label =
                if source.label.is_empty() { source.source.as_str() } else { source.label.as_str() };
            ui.add(
                egui::Label::new(egui::RichText::new(label).color(palette.text))
                    .selectable(false)
                    .wrap_mode(egui::TextWrapMode::Truncate),
            )
            .on_hover_text(source.source.clone());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, palette, Icon::Close, space::of(4), false)
                    .on_hover_text("Detach")
                    .clicked()
                {
                    events.agent(AgentEdit::DropContext(index));
                }
                if !source.kind.is_empty() {
                    ui.label(numeric(source.kind.clone()).color(palette.faint));
                }
            });
        });
    }
}

/// The three-layer cascade, per field, with **where each value came from** — feature 11.
///
/// Every row is read out of [`ResolvedRules::rows`](vellum_agent::ResolvedRules::rows), which
/// is the record resolution itself produced. Comparing the three layers here instead would be
/// a second derivation of provenance, and the two would eventually disagree about what the
/// agent was actually told — which is the failure `docs/07-agent-canvas.md` §7 names outright.
///
/// The layer's own [`Layer::label`](vellum_agent::Layer::label) supplies the wording, so
/// *"Inherited from this project"* is one string in one place rather than a sentence this
/// panel invents.
fn rules(
    ui: &mut Ui,
    palette: Palette,
    model: &PanelModel,
    cmd_ctx: &CommandContext,
    events: &mut EventSink,
) {
    let Some(agent) = &model.agent else { return };
    ui.add_space(space::UNIT);
    hairline(ui, palette);
    section_header(ui, palette, "Rules");

    if !agent.rules.inherits {
        caption(
            ui,
            palette,
            "This agent ignores your global and project rules entirely.",
        );
    }

    for setting in agent.rules.rows() {
        row(ui, palette, setting.field.label(), |ui| match &setting.value {
            Some(value) => {
                ui.add(
                    egui::Label::new(egui::RichText::new(value.as_str()).color(palette.text))
                        .selectable(false)
                        .wrap_mode(egui::TextWrapMode::Truncate),
                );
            }
            None => {
                ui.label(egui::RichText::new("Not set").color(palette.faint));
            }
        });
        // *Set here* wears the accent and an inherited value does not, so the eye finds the
        // overrides without reading a word — and the words are still there for the case
        // where "inherited" is not enough and you need to know *from where*.
        let colour = if setting.from == vellum_agent::Layer::Agent { palette.accent } else { palette.faint };
        ui.horizontal(|ui| {
            ui.add_space(crate::widgets::LABEL_WIDTH);
            ui.label(
                egui::RichText::new(setting.from.label())
                    .color(colour)
                    .size(crate::theme::text::LABEL),
            );
        });
    }

    if !agent.own_rules.text.trim().is_empty() {
        ui.add_space(space::UNIT);
        caption(ui, palette, "This agent also has instructions of its own.");
    }

    command_row(ui, palette, Command::EditAgentRules, cmd_ctx, events);
}

/// A full-width button that runs a command, greyed with its reason when it cannot.
fn command_row(
    ui: &mut Ui,
    palette: Palette,
    command: Command,
    cmd_ctx: &CommandContext,
    events: &mut EventSink,
) {
    let available = command.availability(cmd_ctx);
    let button = egui::Button::new(egui::RichText::new(command.label()).color(palette.text))
        .frame(true)
        .min_size(egui::vec2(CONTROL_WIDTH, 0.0));
    let response = ui.add_enabled(available.is_enabled(), button);
    match available.reason() {
        Some(why) => {
            response.on_disabled_hover_text(why);
        }
        None => {
            if response.clicked() {
                events.command(command);
            }
        }
    }
}

/// A muted line under a row, saying what the control above it resolves to.
fn caption(ui: &mut Ui, palette: Palette, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(crate::widgets::LABEL_WIDTH);
        ui.add(
            egui::Label::new(
                egui::RichText::new(text).color(palette.faint).size(crate::theme::text::LABEL),
            )
            .selectable(false),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{
        AgentSummary, BrowserSummary, FileTreeSummary, ItemFacet, NoteSummary, SelectionItem,
        WorktreeState,
    };
    use vellum_agent::{AgentRules, ContextSource, RuleFile, Schedule};
    use vellum_doc::Placement;

    fn id(n: i32) -> ItemId {
        format!("{n}@1").parse().expect("a valid id")
    }

    fn resolved(agent: &AgentRules, project: &str) -> vellum_agent::ResolvedRules {
        vellum_agent::rules::resolve(
            &RuleFile::parse("---\ntone: warm\nlanguage: English\n---\nHouse style."),
            &RuleFile::parse(project),
            agent,
            "Reviewer",
        )
    }

    fn agent_item(n: i32, kind: RoleKind) -> SelectionItem {
        let own = AgentRules::default();
        SelectionItem {
            agent: Some(AgentSummary {
                role: "Reviewer".to_owned(),
                role_kind: kind,
                provider: None,
                inherited_provider: ProviderChoice::new(Provider::Claude),
                display: None,
                inherited_display: DisplayMode::Clean,
                working_dir: None,
                project_dir: Some("/tmp/project".to_owned()),
                worktree: WorktreeState::Off,
                schedule: None,
                territory: None,
                spawn_cap: 5,
                context: Vec::new(),
                running: false,
                rules: resolved(&own, "---\ntone: blunt\n---\nProject style."),
                own_rules: own,
                connected: Vec::new(),
                accepts_messages: true,
                voice: false,
            }),
            ..SelectionItem::new(id(n), ItemFacet::Agent, Placement::new(0.0, 0.0, 400.0, 300.0))
        }
    }

    fn of(selection: &[SelectionItem]) -> Vec<AgentRow> {
        rows(&PanelModel::derive(selection))
    }

    /// A worker has nothing to spawn into, so it gets neither a territory nor a cap.
    /// Drawing them anyway would be two controls over a value nothing reads — and the cap
    /// in particular would read as a limit that is being enforced when it is not.
    #[test]
    fn only_a_managing_role_gets_a_territory_and_a_cap() {
        let worker = of(&[agent_item(1, RoleKind::Worker)]);
        assert!(!worker.contains(&AgentRow::Territory));
        assert!(!worker.contains(&AgentRow::SpawnCap));
        assert!(worker.contains(&AgentRow::Provider), "it still has a provider");

        for managing in [RoleKind::Orchestrator, RoleKind::Meta] {
            let rows = of(&[agent_item(1, managing)]);
            assert!(rows.contains(&AgentRow::Territory), "{managing:?}");
            assert!(rows.contains(&AgentRow::SpawnCap), "{managing:?}");
        }
    }

    /// The four folding properties survive a multi-selection; the single-node configuration
    /// withdraws. Two agents have two rule cascades and two schedules, and a control that
    /// wrote one into both would lose one of them without saying so.
    #[test]
    fn two_agents_keep_the_folding_rows_and_lose_the_rest() {
        let many = of(&[agent_item(1, RoleKind::Orchestrator), agent_item(2, RoleKind::Orchestrator)]);
        assert_eq!(many, vec![AgentRow::Kind, AgentRow::Provider, AgentRow::Display]);

        let one = of(&[agent_item(1, RoleKind::Worker)]);
        assert!(one.contains(&AgentRow::Role));
        assert!(one.contains(&AgentRow::Rules));
        assert!(one.contains(&AgentRow::Schedule));
        assert!(one.contains(&AgentRow::Context));
    }

    /// The rows come from the properties the selection has. A note is not an agent, and a
    /// selection of notes must not be offered a provider — which is the failure a `match` on
    /// the facet produces the first time somebody adds a kind and forgets an arm.
    #[test]
    fn each_kind_gets_only_its_own_rows() {
        let mut note =
            SelectionItem::new(id(1), ItemFacet::Note, Placement::new(0.0, 0.0, 100.0, 100.0));
        note.note = Some(NoteSummary {
            path: "notes/plan.md".to_owned(),
            scope: NoteScope::Shared,
            owner: None,
            conflicted: false,
            links: 0,
            on_disk: true,
        });
        let rows = of(std::slice::from_ref(&note));
        assert!(rows.contains(&AgentRow::NoteScope) && rows.contains(&AgentRow::NotePath));
        assert!(!rows.contains(&AgentRow::Provider), "{rows:?}");

        let mut tree =
            SelectionItem::new(id(2), ItemFacet::FileTree, Placement::new(0.0, 0.0, 1.0, 1.0));
        tree.file_tree = Some(FileTreeSummary {
            root: String::new(),
            agent: None,
            show_ignored: false,
        });
        let rows = of(std::slice::from_ref(&tree));
        assert!(rows.contains(&AgentRow::TreeIgnored));
        assert!(!rows.contains(&AgentRow::NoteScope));

        let mut browser =
            SelectionItem::new(id(3), ItemFacet::Browser, Placement::new(0.0, 0.0, 1.0, 1.0));
        browser.browser = Some(BrowserSummary {
            url: "https://example.test/".to_owned(),
            title: String::new(),
            live: false,
            allowed: false,
        });
        let rows = of(std::slice::from_ref(&browser));
        assert!(rows.contains(&AgentRow::BrowserLive));
        assert!(!rows.contains(&AgentRow::Display), "a browser node has no output mode");

        // …and nothing at all for an ordinary sticky, which is what keeps a board with no
        // agent nodes exactly the interface it was.
        let plain =
            SelectionItem::new(id(4), ItemFacet::Sticky, Placement::new(0.0, 0.0, 1.0, 1.0));
        assert!(of(std::slice::from_ref(&plain)).is_empty());
    }

    /// The section is named after what is in it. A note headed *Agent* is the panel calling
    /// two things one thing.
    #[test]
    fn the_section_is_named_after_what_is_selected() {
        let agent = PanelModel::derive(&[agent_item(1, RoleKind::Worker)]);
        assert_eq!(heading(&agent), "Agent");

        let mut note =
            SelectionItem::new(id(1), ItemFacet::Note, Placement::new(0.0, 0.0, 1.0, 1.0));
        note.note = Some(NoteSummary {
            path: "n.md".to_owned(),
            scope: NoteScope::Shared,
            owner: None,
            conflicted: false,
            links: 0,
            on_disk: false,
        });
        assert_eq!(heading(&PanelModel::derive(&[note.clone()])), "Note");
        assert_eq!(
            heading(&PanelModel::derive(&[note, agent_item(2, RoleKind::Worker)])),
            "Agent canvas"
        );
    }

    /// Provenance is read off the resolution, and the whole point is that a value set on the
    /// node is distinguishable from the same value arriving from a file.
    ///
    /// The fixture puts a tone in the global layer, a *different* tone in the project layer
    /// and then overrides it on the node — so the row must report the node's word and the
    /// node as its source, while a field nobody touched still names the file it came from.
    #[test]
    fn a_rules_row_names_the_layer_that_supplied_it() {
        let own = AgentRules { text: "---\ntone: terse\n---".to_owned(), ..AgentRules::default() };
        let resolved = resolved(&own, "---\ntone: blunt\nlanguage: German\n---");

        let by_field: Vec<(&str, Option<String>, vellum_agent::Layer)> = resolved
            .rows()
            .into_iter()
            .map(|r| (r.field.key(), r.value, r.from))
            .collect();

        let tone = by_field.iter().find(|(key, ..)| *key == "tone").expect("a tone row");
        assert_eq!(tone.1.as_deref(), Some("terse"), "the node's own word wins");
        assert_eq!(tone.2, vellum_agent::Layer::Agent, "and it is reported as set here");
        assert!(!tone.2.is_inherited());

        let language =
            by_field.iter().find(|(key, ..)| *key == "language").expect("a language row");
        assert_eq!(language.1.as_deref(), Some("German"));
        assert_eq!(language.2, vellum_agent::Layer::Project);
        assert!(language.2.is_inherited());
        assert_eq!(language.2.label(), "Inherited from this project");

        // A field nobody set still gets a row, so the panel shows the whole set of settings
        // rather than only the ones somebody happened to fill in.
        let output = by_field.iter().find(|(key, ..)| *key == "output").expect("an output row");
        assert_eq!(output.1, None);
        assert_eq!(output.2, vellum_agent::Layer::Default);
    }

    /// Every row has to survive being drawn, including the states nothing else exercises: a
    /// running orchestrator with a schedule, a territory and attached context; a conflicted
    /// note; a browser node whose engine is not permitted.
    #[test]
    fn every_row_draws_without_panicking() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);

        let mut orchestrator = agent_item(1, RoleKind::Orchestrator);
        {
            let agent = orchestrator.agent.as_mut().expect("an agent");
            agent.running = true;
            agent.schedule = Some(Schedule { enabled: true, ..Schedule::default() });
            agent.territory = Some(vellum_agent::Territory::new(0.0, 0.0, 800.0, 600.0));
            agent.worktree = WorktreeState::At("/tmp/wt/1".to_owned());
            agent.working_dir = Some("crates".to_owned());
            agent.provider = Some(ProviderChoice::new(Provider::Kimi).with_model("kimi-k2"));
            agent.display = Some(DisplayMode::Raw);
            agent.context = vec![ContextSource {
                source: "spec.pdf".to_owned(),
                kind: "pdf".to_owned(),
                extract: None,
                label: "spec.pdf".to_owned(),
            }];
        }

        let mut note =
            SelectionItem::new(id(2), ItemFacet::Note, Placement::new(0.0, 0.0, 1.0, 1.0));
        note.note = Some(NoteSummary {
            path: "notes/plan.md".to_owned(),
            scope: NoteScope::Private { agent: "1@1".to_owned() },
            owner: Some("Reviewer".to_owned()),
            conflicted: true,
            links: 3,
            on_disk: true,
        });

        let mut browser =
            SelectionItem::new(id(3), ItemFacet::Browser, Placement::new(0.0, 0.0, 1.0, 1.0));
        browser.browser = Some(BrowserSummary {
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
            live: false,
            allowed: false,
        });

        for selection in [vec![orchestrator], vec![note], vec![browser]] {
            let model = PanelModel::derive(&selection);
            let cmd_ctx = CommandContext {
                board_open: true,
                selected: 1,
                agents_selected: usize::from(model.has_agent()),
                ..CommandContext::default()
            };
            let mut state = AgentPanelState::default();
            let mut events = EventSink::default();
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                show(ui, Palette::LIGHT, &mut state, &model, &cmd_ctx, &mut events);
            });
        }
    }
}
