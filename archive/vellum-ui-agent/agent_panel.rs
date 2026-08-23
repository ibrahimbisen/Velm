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
    /// Push-to-talk — feature 14. Present whatever the build can do, and disabled with
    /// [`vellum_agent::voice::NOT_BUILT_IN`] when it cannot record: a control that is simply
    /// absent leaves the user with no way to discover that the feature exists and is off.
    Voice,
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
            out.push(AgentRow::Voice);
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
    /// The file name offered for a note that has none yet. Seeded from the note's own
    /// title through [`NoteSummary::proposed_stem`], and left alone afterwards so a user
    /// who typed a name does not watch it change under them as they rename the node.
    ///
    /// [`NoteSummary::proposed_stem`]: crate::NoteSummary::proposed_stem
    note_stem: Option<(ItemId, String)>,
    tree_root: Option<(ItemId, String)>,
    /// Where a local or custom model is listening — feature 16's other half.
    ///
    /// Its own buffer rather than sharing [`Self::model_name`]'s: they are two fields of one
    /// row group and a user types into both, so one buffer would put the endpoint into the
    /// model name the moment focus moved between them.
    endpoint: Option<(ItemId, String)>,
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
                        // **The consequence, not the mechanic.** This said *"Clear the
                        // region"*, which describes the click and hides what it does: a
                        // manager with no region is refused **every** spawn
                        // (`orchestrator.rs`'s `Refusal::NoTerritory`), so this button is not
                        // "unrestrict it" — it is "stop it working until you draw a new one".
                        .on_hover_text(
                            "Clear the region — this manager is then refused every spawn \
                             until a new one is drawn.",
                        )
                        .clicked()
                    {
                        events.agent(AgentEdit::Territory(None));
                    }
                }
                // Reported rather than typed. A rectangle is drawn on the board, not entered
                // as four numbers — a numeric editor for a region you can see is the control
                // nobody uses.
                //
                // **The copy here was correct when written and is now stale, which is the
                // more dangerous of the two failures.** It said a territory could only be
                // made by *"switching it back to Worker and to Manager again"* and that a
                // node without one *"may spawn anywhere on the board"*. The first is a
                // workaround for a gesture that now exists — `Command::SetTerritory` arms it
                // and the next drag sweeps it — and the second is the exact opposite of what
                // `orchestrator.rs` does, which is to refuse every spawn. Two sentences, one
                // obsolete and one inverted, in the row that explains a manager's whole scope.
                _ => {
                    ui.add_enabled(false, egui::Button::new("Not set").frame(true))
                        .on_disabled_hover_text(
                            "This manager has no region, so every spawn it asks for is \
                             refused. Draw one with the button below.",
                        );
                }
            });
            // In both states: with a region, to redraw it; without one, because that is the
            // only way out of the state above.
            command_row(ui, palette, Command::SetTerritory, cmd_ctx, events);
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

        AgentRow::Voice => {
            let Some(agent) = &model.agent else { return };
            let on = agent.voice;
            let built_in = agent.voice_available;
            row(ui, palette, "Voice", |ui| {
                // Two yeses again, and the outer one belongs to the binary rather than to
                // the user — the `browser` arm's arrangement, for the same reason. A build
                // that cannot record still draws the control and says so; hiding it would
                // make an off feature indistinguishable from one that does not exist.
                let button =
                    egui::Button::selectable(on && built_in, if on { "On" } else { "Off" })
                        .frame(true);
                let response = ui.add_enabled(built_in, button);
                if built_in {
                    if response
                        // **The gesture is named again, because it now exists.** This text
                        // used to promise *"hold the key on this node to talk to it"* against
                        // no key at all — nothing in `vellum-app` called `vellum_agent::voice`
                        // — and it was rewritten to say so, under this file's rule that a
                        // string must never describe a gesture the user cannot perform.
                        //
                        // `ActiveState::talk_key` is that key, driven end to end by
                        // `--demo agent-voice`, so the promise is restored **alongside the
                        // thing that performs it and not before**, which is exactly the
                        // condition the old comment set for restoring it.
                        .on_hover_text(if on {
                            "Listening is on for this node. Select it and hold ⌥D to talk; \
                             what you say becomes its prompt, to check before you send it."
                        } else {
                            "Let this node be spoken to. With it on, select the node and hold \
                             ⌥D to talk — what you say is transcribed into its prompt row."
                        })
                        .clicked()
                    {
                        events.agent(AgentEdit::Voice(!on));
                    }
                } else {
                    // The crate's own sentence, not a second one. It names the feature
                    // rather than a command line, deliberately — see its doc comment.
                    response.on_disabled_hover_text(vellum_agent::voice::NOT_BUILT_IN);
                }
            });
            if !built_in {
                caption(ui, palette, "Not built into this copy of Velm.");
            }
        }

        AgentRow::Context => context(ui, palette, model, events),

        AgentRow::Rules => rules(ui, palette, model, cmd_ctx, events),

        AgentRow::NotePath => note_file(ui, palette, state, model, events),

        AgentRow::NoteScope => note_scope(ui, palette, model, events),

        AgentRow::NoteState => {
            let Some(note) = &model.note else { return };
            // Named `phrase` rather than `state`, which is the parameter one scope out. A
            // shadow there is legal and reads as a bug the next time somebody edits this arm.
            let phrase = if note.conflicted {
                "Changed here and on disk — both were kept"
            } else if note.on_disk {
                "In step with the file"
            } else {
                "Not written yet"
            };
            row(ui, palette, "State", |ui| {
                ui.label(
                    egui::RichText::new(phrase)
                        .color(if note.conflicted { palette.warning } else { palette.muted }),
                );
            });
            if note.links > 0 {
                caption(ui, palette, &format!("Links to {} other note(s).", note.links));
            }
        }

        AgentRow::TreeRoot => tree_root(ui, palette, state, model, events),

        AgentRow::TreeOwner => tree_owner(ui, palette, model, events),

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
                        //
                        // **The endpoint is kept only between the two providers that have
                        // one.** `Provider::default_base_url` answers `None` for `Local` and
                        // `Custom` precisely because those *are* their address, so dropping
                        // it while moving between them would silently unconfigure a node that
                        // still looks configured. Carrying it onto a hosted provider would be
                        // the opposite and worse mistake: an address typed for a server on
                        // this machine must never become where Velm sends a hosted request.
                        //
                        // The transport is deliberately **not** carried: it is a per-provider
                        // fact — `ClaudeCli` is not a way of reaching Kimi — so it goes back
                        // to the new provider's own default.
                        let previous = match &chosen {
                            Some(Some(c)) => Some(c.clone()),
                            _ => None,
                        };
                        let mut choice = ProviderChoice::new(provider);
                        choice.model = previous.as_ref().and_then(|c| c.model.clone());
                        if provider.default_base_url().is_none() {
                            choice.base_url = previous.and_then(|c| c.base_url);
                        }
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

    // **Where a local model is listening — and without this row there was nowhere to say
    // it.** `Provider::{Local, Custom}` answer `None` from `default_base_url` on purpose:
    // guessing `localhost:11434` would talk to whichever of llama.cpp, LM Studio, Ollama and
    // vLLM happened to be up. That is right, and until this field was drawn it also meant the
    // two providers whose entire configuration *is* an address had no way to be given one —
    // so "connect a model you run on your own GPU", which feature 16 asks for by name, was
    // reachable only by hand-editing a board file or over the machine-facing IPC verb.
    //
    // Shown only for those two, because for a hosted provider the endpoint is not the user's
    // to choose and a box offering to change it invites a node that silently talks to nothing.
    if effective.provider.default_base_url().is_none() {
        let current = effective.base_url.clone().unwrap_or_default();
        let buffer = AgentPanelState::buffer(&mut state.endpoint, id, &current);
        row(ui, palette, "Endpoint", |ui| {
            let field = ui
                .add(
                    egui::TextEdit::singleline(buffer)
                        .desired_width(CONTROL_WIDTH)
                        .hint_text("http://localhost:11434/v1"),
                )
                .on_hover_text(
                    "The OpenAI-compatible address this node talks to — llama.cpp, LM Studio, \
                     Ollama and vLLM all speak it. Per node, so one board can hold an agent \
                     on a model served from this machine and another on a model served from \
                     somewhere else.",
                );
            if field.lost_focus() {
                let typed = buffer.trim();
                let mut choice = effective.clone();
                choice.base_url = (!typed.is_empty()).then(|| typed.to_owned());
                events.agent(AgentEdit::Provider(Some(choice)));
            }
        });
        if effective.base_url.is_none() {
            caption(
                ui,
                palette,
                "This node has no endpoint yet, so starting it will fail. Velm does not guess \
                 one: four local servers use four different ports.",
            );
        }
    }
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

/// Which file a note is, and — for a note that has none — the gesture that makes one.
///
/// # A note with an empty path is the state this row exists for
///
/// The note tool places a node with `NoteModel::default()`, whose path is empty, and an empty
/// path addresses no file: the poller skips it, nothing is ever written, and the node reads
/// *"Not written yet"* for the rest of its life. That is the whole of feature 8 failing to
/// start, and it failed here — the row was a readout of a string that was always empty.
///
/// So the row has two shapes. **No file**: a name, offered rather than demanded, and a button
/// that creates it. **A file**: its path, and the way to open it in a real editor, which is
/// the reason notes are files at all.
///
/// The name is a *stem*. Which directory it lands in follows from the note's scope, and
/// `NoteStore::dir_for` is the only thing that knows that — a panel that composed a path
/// would put every private note in the shared folder.
fn note_file(
    ui: &mut Ui,
    palette: Palette,
    state: &mut AgentPanelState,
    model: &PanelModel,
    events: &mut EventSink,
) {
    let (Some(note), Some(id)) = (&model.note, model.single_id) else { return };

    if !note.path.is_empty() {
        row(ui, palette, "File", |ui| {
            readout(ui, palette, note.path.clone()).on_hover_text(note.path.clone());
        });
        // Enabled only when it is really there. Handing a path that does not exist to the
        // file manager opens nothing and explains nothing, which is the inert button this
        // house does not ship.
        let reveal = ui.add_enabled(
            note.on_disk,
            egui::Button::new("Show the file").frame(true),
        );
        if note.on_disk {
            if reveal
                .on_hover_text("A note is a real `.md` file; open it in any editor.")
                .clicked()
            {
                events.push(UiEvent::RevealPath(note.path.clone().into()));
            }
        } else {
            reveal.on_disabled_hover_text(format!(
                "{} has not been written yet. It appears the moment something is saved into \
                 it.",
                note.path
            ));
        }
        return;
    }

    let proposed = note.proposed_stem();
    // Read back out of the closure rather than off the buffer afterwards, which is the
    // idiom every other field here uses: the buffer is borrowed out of `state` and the
    // closure is what holds it, so touching it on both sides of `row` is two borrows for
    // one value.
    let mut typed = String::new();
    let buffer = AgentPanelState::buffer(&mut state.note_stem, id, &proposed);
    row(ui, palette, "File", |ui| {
        ui.add(
            egui::TextEdit::singleline(buffer)
                .desired_width(CONTROL_WIDTH)
                .hint_text(proposed.as_str()),
        )
        .on_hover_text(
            "The file's name, without the .md. Velm adds a number if that name is taken.",
        );
        typed = buffer.trim().to_owned();
    });

    // Empty is not refused — it is the offer. The hint already shows what an empty field
    // means, and a button that greys out because the user cleared a field they never filled
    // in is a dead end in the one place the whole feature starts.
    let stem = if typed.is_empty() { proposed.clone() } else { typed };
    if ui
        .add(egui::Button::new("Create the file").frame(true).min_size(egui::vec2(CONTROL_WIDTH, 0.0)))
        .on_hover_text(format!(
            "Writes {stem}.md and points this note at it. Everything typed into the note \
             after that is that file's contents, and any editor — or any agent — can change \
             it."
        ))
        .clicked()
    {
        events.agent(AgentEdit::CreateNoteFile(stem));
    }
    caption(ui, palette, "This note has no file yet, so nothing can read it.");
}

/// Shared with every agent, or private to one — **both directions**.
///
/// # What was missing, and why it was missing
///
/// Only `Shared` had a writer. The other half told the user to *"connect the note to one
/// agent to make it that agent's own"*, and connecting it did nothing at all: no code turned
/// a connector into a scope. So the instruction described a gesture that did not exist, which
/// is worse than describing none — the user does it, watches nothing happen, and concludes
/// the feature is broken rather than absent.
///
/// The connector was the right idea and it is what this reads. `NoteSummary::connected` is
/// the agents on the other end of a line the user drew, and each one is offered by name. The
/// scope carries an **id**, because two agents may share a label.
///
/// With no line drawn, the button is disabled and says so — and *that* instruction is one the
/// app can honour, because drawing a connector is a gesture that exists.
fn note_scope(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
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
            return;
        }

        let connected: &[crate::AgentLink] =
            model.note.as_ref().map_or(&[], |note| note.connected.as_slice());
        if connected.is_empty() {
            ui.add_enabled(false, egui::Button::selectable(false, "Shared").frame(true))
                .on_disabled_hover_text(
                    "Every agent on this board may read and write it. Draw a connector from \
                     this note to one agent to make it that agent's own.",
                );
            return;
        }

        // A menu rather than a toggle, because *private* is not the opposite of *shared* —
        // it is "private to whom", and with two lines drawn the button would have to guess.
        egui::ComboBox::from_id_salt("velm-note-scope")
            .selected_text("Shared")
            .width(CONTROL_WIDTH)
            .show_ui(ui, |ui| {
                if ui.selectable_label(true, "Shared with every agent").clicked() {
                    events.agent(AgentEdit::NoteScope(NoteScope::Shared));
                }
                for link in connected {
                    if ui
                        .selectable_label(false, format!("Private to {}", link.label))
                        .clicked()
                    {
                        events.agent(AgentEdit::NoteScope(NoteScope::Private {
                            agent: link.id.clone(),
                        }));
                    }
                }
            })
            .response
            .on_hover_text(
                "A private note is one agent's own memory; a shared one is the board's.",
            );
    });
}

/// Which directory a file tree shows.
///
/// Editable, where it was a readout — so every tree on a board showed the same derived
/// project root and there was no way to point one at `crates/` and another at `docs/`, which
/// is most of what two trees are for.
///
/// Relative to the project, like a note's path and for the same reason: a project that moves
/// keeps working. Empty is the root itself, which is what an unset tree already means, so
/// clearing the field is a real answer rather than a refusal.
fn tree_root(
    ui: &mut Ui,
    palette: Palette,
    state: &mut AgentPanelState,
    model: &PanelModel,
    events: &mut EventSink,
) {
    let (Some(tree), Some(id)) = (&model.file_tree, model.single_id) else { return };
    let buffer = AgentPanelState::buffer(&mut state.tree_root, id, &tree.root);
    row(ui, palette, "Root", |ui| {
        let field = ui.add(
            egui::TextEdit::singleline(buffer)
                .desired_width(CONTROL_WIDTH)
                .hint_text("The board's folder"),
        );
        // On losing focus, exactly as Role and Folder are: one directory is one edit, and
        // re-reading the filesystem on every keystroke of a path being typed would walk a
        // directory per character.
        if field.lost_focus() {
            events.agent(AgentEdit::TreeRoot(buffer.trim().to_owned()));
        }
    });
    let resolved = match (&tree.project_dir, tree.root.is_empty()) {
        (Some(project), true) => format!("Showing {project}."),
        (Some(project), false) => format!("Showing {project}/{}.", tree.root),
        (None, _) => "This board has no folder, so there is nothing to show.".to_owned(),
    };
    caption(ui, palette, &resolved);
}

/// Which agent a tree belongs to — feature 7's hard requirement, and it had no writer either.
///
/// The same shape as a private note's owner, deliberately: the answer is a line the user drew
/// on the board, the picker offers labels and emits ids, and with no line drawn it says so
/// rather than offering an empty menu.
fn tree_owner(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    let Some(tree) = &model.file_tree else { return };
    row(ui, palette, "Scoped to", |ui| {
        let shown = tree.agent.clone().unwrap_or_else(|| "Nobody in particular".to_owned());
        if tree.connected.is_empty() && tree.agent_id.is_none() {
            ui.add_enabled(false, egui::Button::new(shown).frame(true))
                .on_disabled_hover_text(
                    "Draw a connector from this tree to an agent to scope it to that agent. \
                     Until then it is yours, and no agent is given it.",
                );
            return;
        }

        egui::ComboBox::from_id_salt("velm-tree-owner")
            .selected_text(shown)
            .width(CONTROL_WIDTH)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(tree.agent_id.is_none(), "Nobody in particular")
                    .clicked()
                {
                    events.agent(AgentEdit::TreeOwner(None));
                }
                for link in &tree.connected {
                    let on = tree.agent_id.as_deref() == Some(link.id.as_str());
                    if ui.selectable_label(on, link.label.clone()).clicked() {
                        events.agent(AgentEdit::TreeOwner(Some(link.id.clone())));
                    }
                }
            })
            .response
            .on_hover_text(
                "A tree scoped to an agent is that agent's view of the project — feature 7's \
                 whole point is that two agents do not share one.",
            );
    });
}

/// The files, pages and media this agent has been given.
fn context(ui: &mut Ui, palette: Palette, model: &PanelModel, events: &mut EventSink) {
    let Some(agent) = &model.agent else { return };
    if agent.context.is_empty() {
        row(ui, palette, "Context", |ui| {
            ui.label(egui::RichText::new("Nothing attached").color(palette.muted));
        });
        // **Both halves of this sentence were false when it shipped.** Connecting a note
        // to an agent wrote nothing into `AgentModel::context`, and there was no
        // `WindowEvent::DroppedFile` arm anywhere in the application — the word *drop* did
        // not appear in the repository. The drop is real now; the button below is the other
        // way in, for a file that is not on screen and for anyone who would rather not drag.
        caption(ui, palette, "Drop a file on this node, or attach one here.");
    } else {
        row(ui, palette, "Context", |ui| {
            readout(ui, palette, format!("{} attached", agent.context.len()));
        });
    }

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

    attach(ui, palette, events);
}

/// *Attach a file…* — the picker half of context ingestion.
///
/// `vellum_agent::ingest` is 3,000 lines that had **no caller in the workspace**: nothing
/// wrote `AgentModel::context` at all, so PDFs, documents, audio and pages could be read and
/// never asked for. Two gestures reach it now and this is the one a panel can offer; the
/// other is a file dropped on the node, which is a window event and never comes through here.
///
/// It carries no path, because this crate opens no dialogs and touches no files. What
/// happens to the file — what kind it is, how much came out, whether a converter is missing —
/// is `ingest`'s answer and arrives back as a toast.
fn attach(ui: &mut Ui, palette: Palette, events: &mut EventSink) {
    if ui
        .add(
            egui::Button::new(egui::RichText::new("Attach a file…").color(palette.text))
                .frame(true)
                .min_size(egui::vec2(CONTROL_WIDTH, 0.0)),
        )
        .on_hover_text(
            "A PDF, a document, a page or a recording. Velm reads the text out of it and \
             gives it to this agent; what it could not read, it says.",
        )
        .clicked()
    {
        events.agent(AgentEdit::AttachContext);
    }

    // **The other half of feature 18, which had no door at all.** `ingest` has always known
    // `Kind::Web` and `Kind::YouTube` — it fetches a page, or a transcript — and both entry
    // points into it were filesystem-only: a dropped file and a file picker. So *"websites and
    // YouTube links"*, named in the feature's own sentence, could not be attached by any
    // gesture. A file picker cannot open a URL, so this is a second button rather than a
    // cleverer first one.
    if ui
        .add(
            egui::Button::new(egui::RichText::new("Attach a link…").color(palette.text))
                .frame(true)
                .min_size(egui::vec2(CONTROL_WIDTH, 0.0)),
        )
        .on_hover_text(
            "A web page or a YouTube link. Velm fetches it and gives this agent the readable \
             text — for a video, its transcript if there is one.",
        )
        .clicked()
    {
        events.agent(AgentEdit::AttachLink);
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
                chat_theme: None,
                chat_opacity: 255,
                has_chat_background: false,
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
                voice_available: false,
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

    /// Push-to-talk gets a row whatever the build can do — feature 14's control had **no
    /// emitter at all**: `AgentEdit::Voice` was defined and handled and nothing anywhere
    /// produced one.
    ///
    /// Present rather than conditional on the feature, deliberately. A control that vanishes
    /// in a default build leaves the user with no way to find out that voice exists and is
    /// off, which is exactly the state this shipped in; the row draws and refuses instead,
    /// naming `voice::NOT_BUILT_IN`.
    #[test]
    fn an_agent_gets_a_voice_row_whatever_the_build_can_do() {
        for available in [true, false] {
            let mut item = agent_item(1, RoleKind::Worker);
            item.agent.as_mut().expect("an agent").voice_available = available;
            assert!(
                of(std::slice::from_ref(&item)).contains(&AgentRow::Voice),
                "no voice row with available={available}"
            );
        }

        // …and not for a note, which has no microphone to offer.
        let mut note =
            SelectionItem::new(id(9), ItemFacet::Note, Placement::new(0.0, 0.0, 1.0, 1.0));
        note.note = Some(NoteSummary {
            path: String::new(),
            scope: NoteScope::Shared,
            owner: None,
            conflicted: false,
            links: 0,
            on_disk: false,
            title: "Plan".to_owned(),
            connected: Vec::new(),
        });
        assert!(!of(std::slice::from_ref(&note)).contains(&AgentRow::Voice));
    }

    /// A note that has never been written still gets its file row, because that row is the
    /// only place a file can be asked for.
    ///
    /// The row used to be a readout of `NoteModel::path`, which is empty for every note the
    /// note tool places — so it drew an empty box, and `NoteStore::create` was never called
    /// by anything. A row that is only useful once the thing it describes exists cannot be
    /// the way that thing comes to exist.
    #[test]
    fn a_note_with_no_file_still_gets_the_row_that_makes_one() {
        let mut note =
            SelectionItem::new(id(1), ItemFacet::Note, Placement::new(0.0, 0.0, 1.0, 1.0));
        note.note = Some(NoteSummary {
            path: String::new(),
            scope: NoteScope::Shared,
            owner: None,
            conflicted: false,
            links: 0,
            on_disk: false,
            title: "Engine bay".to_owned(),
            connected: Vec::new(),
        });
        let rows = of(std::slice::from_ref(&note));
        assert!(rows.contains(&AgentRow::NotePath), "{rows:?}");

        // …and the name it offers is the store's own slug of the title, not a second
        // spelling of one. Two answers to "what is this file called" is the user watching
        // the name they accepted turn into a different one.
        let summary = note.note.as_ref().expect("a note");
        assert_eq!(summary.proposed_stem(), "engine-bay");
        assert_eq!(summary.proposed_stem(), vellum_agent::notes::slug("Engine bay"));
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
            title: "Plan".to_owned(),
            connected: Vec::new(),
        });
        let rows = of(std::slice::from_ref(&note));
        assert!(rows.contains(&AgentRow::NoteScope) && rows.contains(&AgentRow::NotePath));
        assert!(!rows.contains(&AgentRow::Provider), "{rows:?}");

        let mut tree =
            SelectionItem::new(id(2), ItemFacet::FileTree, Placement::new(0.0, 0.0, 1.0, 1.0));
        tree.file_tree = Some(FileTreeSummary {
            root: String::new(),
            project_dir: None,
            agent: None,
            agent_id: None,
            connected: Vec::new(),
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
            title: "Plan".to_owned(),
            connected: Vec::new(),
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
            title: "Plan".to_owned(),
            connected: Vec::new(),
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
