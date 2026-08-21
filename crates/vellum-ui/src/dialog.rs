//! Modal dialogs and transient toasts.
//!
//! Both are *requested* by the app rather than raised by the chrome, because the
//! chrome has no way to know whether deleting a board needs confirming. The app
//! pushes a [`Dialog`] with an id it chose, and hears the answer back as a
//! [`DialogEvent`] carrying that same id.
//!
//! Only one dialog is on screen at a time and later ones queue behind it. Stacked
//! modals are a bug factory — the second one's Escape closes the first — and a board
//! app never legitimately needs to ask two questions at once.

use crate::event::{DialogEvent, DialogId, EventSink, UiEvent};
use crate::icon::Icon;
use crate::theme::{
    Backing, GlassSurface, Palette, dialog_frame, floating_frame, paint_glass_edge, radius,
    screen_title, space,
};
use egui::{Color32, Context, Id, Key, Ui, Vec2, vec2};
use std::collections::VecDeque;

/// A question that blocks the board until it is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "the Rules variant carries a whole resolved cascade and its two file paths, and \
              the Schedule variant a schedule plus its hand-off targets. Boxing either would \
              put an allocation behind every dialog in the application to save bytes on a \
              value of which exactly one exists at a time — a modal is singular by definition"
)]
pub enum Dialog {
    Confirm {
        id: DialogId,
        title: String,
        message: String,
        /// The affirmative button's text — "Delete", "Discard", "Replace". Never
        /// "OK": a button labelled with its consequence is the difference between
        /// reading the dialog and dismissing it.
        confirm: String,
        /// Draws the affirmative button in the danger colour and leaves Cancel as
        /// the default focus.
        destructive: bool,
    },
    Rename {
        id: DialogId,
        title: String,
        /// Seeded with the current name and edited in place.
        value: String,
        /// The affirmative button's text. Same rule as [`Dialog::Confirm`]'s: a button
        /// labelled with its consequence is the difference between reading a dialog and
        /// dismissing it. This was the literal "Rename" for every caller, so **New
        /// board**, **Save as** and **New space** all offered to rename something that
        /// did not exist yet.
        confirm: String,
        /// The placeholder shown while the field is empty. Was the literal "Board name"
        /// for every caller, including the two that name a space.
        hint: String,
    },
    /// A reference sheet: headed groups of term-and-definition rows.
    ///
    /// Its own variant rather than a [`Dialog::Confirm`] holding a long string, because
    /// that is what the shortcut sheet was and it did not work. Columns made of space
    /// padding only line up in a monospace face, and the body is drawn in the
    /// proportional one — so `{:<28}` produced a ragged second column, inside a 380px
    /// modal with no scroll, for forty-eight rows.
    ///
    /// `Dialog` is a plain data enum in a `VecDeque` and derives `PartialEq`, so it
    /// cannot hold a closure that draws itself; the rows come as data and the renderer
    /// lays them out.
    Reference {
        id: DialogId,
        title: String,
        sections: Vec<ReferenceSection>,
        /// The dismiss button's text.
        dismiss: String,
    },
    /// Numbered instructions the user follows *outside* the app before continuing.
    ///
    /// Built for Import from Miro, which is the one thing in Velm that cannot be done by
    /// clicking: half of it happens in a browser, on a website this application has no
    /// control over. *"i want other people to be able to understand how to import their
    /// miro boards … it should say the steps super clearly and there should be like i and
    /// when they hove over it a information should pop up why each step they are doing
    /// matters and for what super simply and then they can press continue."*
    ///
    /// Its own variant rather than a [`Self::Confirm`] with newlines in the message, for
    /// the reason [`Self::Reference`] is its own: a numbered list, a hover target per row
    /// and a two-column layout are things a renderer does, and `Dialog` cannot hold a
    /// closure — it is a plain data enum in a `VecDeque` that derives `PartialEq`.
    Steps {
        id: DialogId,
        title: String,
        /// One sentence above the list, saying what the whole thing is for.
        intro: String,
        steps: Vec<Step>,
        /// The affirmative button — "Continue". Same rule as everywhere else: a button
        /// labelled with what happens next, never "OK".
        confirm: String,
    },
    /// When one agent runs by itself, and what it does afterwards — feature 10.
    ///
    /// Its own variant for the reason [`Self::Reference`] and [`Self::Steps`] are theirs: a
    /// recurrence with three shapes and a hand-off picker built from the board's connectors
    /// is a thing a renderer does, and `Dialog` cannot hold a closure.
    ///
    /// The two fields the chrome cannot supply for itself are here because it cannot: which
    /// agents are reachable, and what the user's clock offset is. `vellum-agent` holds no
    /// timezone database on purpose, and this crate reads no clock and no board.
    Schedule {
        id: DialogId,
        title: String,
        schedule: crate::Schedule,
        /// The agents this one is connected to. The hand-off picker offers **only** these,
        /// which is what makes an unreachable target unreachable by construction rather
        /// than by validation — see `crate::agent_dialogs::schedule_problem` for the case
        /// validation is still needed for.
        targets: Vec<crate::AgentLink>,
        /// The user's offset from UTC, in seconds. Seconds rather than hours because a
        /// half-hour timezone and a 6:30 PM schedule are then the same arithmetic.
        utc_offset: i32,
        confirm: String,
    },
    /// One agent's own rule layer, over the cascade it sits in — feature 11.
    ///
    /// Carries the [`ResolvedRules`](vellum_agent::ResolvedRules) so the editor can show
    /// *inherited* against *set here* from the record resolution produced, rather than by
    /// comparing the three layers itself.
    Rules {
        id: DialogId,
        title: String,
        form: crate::agent_dialogs::RulesForm,
        resolved: vellum_agent::ResolvedRules,
        /// Where the two layers this editor **cannot** write live on disk.
        ///
        /// Supplied by the app because only the app knows its data directory and which
        /// folder the board is in. Without it the editor names the layers a value came from
        /// and can say nothing about how to change them, which is the state feature 11
        /// shipped in: two of its three layers were reachable only by reading the source.
        files: crate::agent_dialogs::RuleFiles,
        confirm: String,
    },
    /// A provider credential — features 16 and 17.
    ///
    /// **The key is never seeded and never shown.** `has_key` is the whole of what this
    /// crate is told about an existing credential (`docs/07-agent-canvas.md` §8a), so there
    /// is no state in which a stored key is on screen; signing in replaces it. The typed
    /// value is a [`SecretKey`](crate::SecretKey), whose `Debug` refuses to print it — which
    /// matters because `Dialog` derives `Debug`.
    SignIn {
        id: DialogId,
        provider: vellum_agent::Provider,
        /// Whether one is already stored. Never *which*.
        has_key: bool,
        key: crate::event::SecretKey,
        confirm: String,
    },
}

/// One numbered instruction, and the ⓘ beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What to do, in one line. Numbered by position, so the numbers cannot get out of
    /// step with the list.
    pub what: String,
    /// Why it matters, shown on hovering the ⓘ.
    ///
    /// **Why, not how.** The instruction already says what to do; a tooltip that repeated
    /// it in more words would be noise. This is the sentence that stops a step feeling
    /// arbitrary — which is what the user asked for, in those words.
    pub why: String,
}

impl Step {
    pub fn new(what: impl Into<String>, why: impl Into<String>) -> Self {
        Self { what: what.into(), why: why.into() }
    }
}

/// One headed group of rows in a [`Dialog::Reference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceSection {
    pub heading: String,
    /// `(what it does, what to press)` — in that order, because a reference sheet is
    /// read by scanning for the action and then taking the key beside it.
    pub rows: Vec<(String, String)>,
}

impl Dialog {
    pub fn confirm(
        id: DialogId,
        title: impl Into<String>,
        message: impl Into<String>,
        confirm: impl Into<String>,
    ) -> Self {
        Self::Confirm {
            id,
            title: title.into(),
            message: message.into(),
            confirm: confirm.into(),
            destructive: false,
        }
    }

    /// A confirmation for an action that destroys something.
    pub fn destructive(
        id: DialogId,
        title: impl Into<String>,
        message: impl Into<String>,
        confirm: impl Into<String>,
    ) -> Self {
        Self::Confirm {
            id,
            title: title.into(),
            message: message.into(),
            confirm: confirm.into(),
            destructive: true,
        }
    }

    /// A one-field dialog. Defaults to renaming; [`Dialog::with_confirm`] and
    /// [`Dialog::with_hint`] adjust it for the callers that are creating instead.
    pub fn rename(id: DialogId, title: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Rename {
            id,
            title: title.into(),
            value: value.into(),
            confirm: "Rename".to_owned(),
            hint: "Name".to_owned(),
        }
    }

    /// Renames the affirmative button — "Create", "Save a copy".
    #[must_use]
    pub fn with_confirm(mut self, label: impl Into<String>) -> Self {
        if let Self::Rename { confirm, .. }
        | Self::Confirm { confirm, .. }
        | Self::Steps { confirm, .. }
        | Self::Schedule { confirm, .. }
        | Self::Rules { confirm, .. }
        | Self::SignIn { confirm, .. } = &mut self
        {
            *confirm = label.into();
        }
        self
    }

    /// Sets the text field's placeholder.
    #[must_use]
    pub fn with_hint(mut self, text: impl Into<String>) -> Self {
        if let Self::Rename { hint, .. } = &mut self {
            *hint = text.into();
        }
        self
    }

    /// A reference sheet — see [`Dialog::Reference`].
    pub fn reference(
        id: DialogId,
        title: impl Into<String>,
        sections: Vec<ReferenceSection>,
        dismiss: impl Into<String>,
    ) -> Self {
        Self::Reference { id, title: title.into(), sections, dismiss: dismiss.into() }
    }

    /// Numbered instructions with a ⓘ each — see [`Dialog::Steps`].
    pub fn steps(
        id: DialogId,
        title: impl Into<String>,
        intro: impl Into<String>,
        steps: Vec<Step>,
    ) -> Self {
        Self::Steps {
            id,
            title: title.into(),
            intro: intro.into(),
            steps,
            confirm: "Continue".to_owned(),
        }
    }

    /// A schedule editor for one agent — see [`Dialog::Schedule`].
    pub fn schedule(
        id: DialogId,
        title: impl Into<String>,
        schedule: crate::Schedule,
        targets: Vec<crate::AgentLink>,
        utc_offset: i32,
    ) -> Self {
        Self::Schedule {
            id,
            title: title.into(),
            schedule,
            targets,
            utc_offset,
            confirm: "Save".to_owned(),
        }
    }

    /// A rules editor for one agent — see [`Dialog::Rules`].
    ///
    /// Takes the node's own layer and the resolution it participated in. The form is derived
    /// here so the caller never has to know that the four structured settings live in the
    /// front matter of one string.
    ///
    /// `files` names the global and project layers on disk — a required argument rather than
    /// a builder step, deliberately. It is the only way the two inherited layers become
    /// findable at all, and a caller that could forget it would leave the cascade's top two
    /// thirds invisible exactly as they were.
    pub fn rules(
        id: DialogId,
        title: impl Into<String>,
        rules: &vellum_agent::AgentRules,
        resolved: vellum_agent::ResolvedRules,
        files: crate::agent_dialogs::RuleFiles,
    ) -> Self {
        Self::Rules {
            id,
            title: title.into(),
            form: crate::agent_dialogs::RulesForm::from_rules(rules),
            resolved,
            files,
            confirm: "Save".to_owned(),
        }
    }

    /// A provider sign-in — see [`Dialog::SignIn`]. The key starts empty, always.
    pub fn sign_in(id: DialogId, provider: vellum_agent::Provider, has_key: bool) -> Self {
        Self::SignIn {
            id,
            provider,
            has_key,
            key: crate::event::SecretKey::default(),
            confirm: if has_key { "Replace".to_owned() } else { "Sign in".to_owned() },
        }
    }

    pub const fn id(&self) -> DialogId {
        match self {
            Self::Confirm { id, .. }
            | Self::Rename { id, .. }
            | Self::Reference { id, .. }
            | Self::Steps { id, .. }
            | Self::Schedule { id, .. }
            | Self::Rules { id, .. }
            | Self::SignIn { id, .. } => *id,
        }
    }
}

/// How loud a toast is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

impl ToastKind {
    const fn accent(self, palette: Palette) -> Color32 {
        match self {
            Self::Info => palette.accent,
            Self::Success => palette.success,
            Self::Error => palette.danger,
        }
    }

    const fn icon(self) -> Icon {
        match self {
            Self::Info => Icon::Info,
            Self::Success => Icon::Check,
            Self::Error => Icon::Close,
        }
    }
}

/// A message that appears, is readable, and goes away.
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    /// Seconds on screen. Errors default to longer because they are the ones worth
    /// reading, and a failed import that vanishes in two seconds is a bug report.
    pub seconds: f64,
}

impl Toast {
    pub fn info(text: impl Into<String>) -> Self {
        Self { kind: ToastKind::Info, text: text.into(), seconds: 3.0 }
    }

    pub fn success(text: impl Into<String>) -> Self {
        Self { kind: ToastKind::Success, text: text.into(), seconds: 3.0 }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { kind: ToastKind::Error, text: text.into(), seconds: 7.0 }
    }
}

/// A toast plus the time it should disappear, in egui's clock.
#[derive(Debug, Clone, PartialEq)]
struct LiveToast {
    toast: Toast,
    expires_at: f64,
}

/// The queue of dialogs and toasts, held by the chrome between frames.
#[derive(Debug, Default)]
pub struct DialogStack {
    pending: VecDeque<Dialog>,
    toasts: Vec<LiveToast>,
    /// Where the toasts were drawn last frame. They float over the canvas, so the
    /// renderer blurs behind them like any other floating chrome.
    glass: Vec<GlassSurface>,
}

impl DialogStack {
    /// Queues a dialog. If one is already showing, this waits behind it.
    pub fn push(&mut self, dialog: Dialog) {
        self.pending.push_back(dialog);
    }

    /// Withdraws a queued or showing dialog — for when the app resolves the question
    /// itself, such as a file appearing while its "not found" dialog is queued.
    pub fn dismiss(&mut self, id: DialogId) {
        self.pending.retain(|d| d.id() != id);
    }

    pub fn is_showing(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The dialog currently on screen, if any.
    pub fn current(&self) -> Option<&Dialog> {
        self.pending.front()
    }

    pub fn toast(&mut self, toast: Toast, now: f64) {
        let expires_at = now + toast.seconds;
        self.toasts.push(LiveToast { toast, expires_at });
    }

    /// Number of toasts still on screen. Exposed for tests and for an app that wants
    /// to avoid stacking twenty import warnings.
    pub fn toast_count(&self) -> usize {
        self.toasts.len()
    }

    fn expire(&mut self, now: f64) {
        self.toasts.retain(|t| t.expires_at > now);
    }

    /// Where the toasts were drawn, for the renderer to blur behind.
    ///
    /// The modal is deliberately absent: `docs/05-design-language.md` §3a rules
    /// translucency out for dialogs, because a modal you can see through undermines
    /// its own job.
    pub fn glass_surfaces(&self) -> &[GlassSurface] {
        &self.glass
    }

    /// Draws whatever is showing.
    pub fn show(&mut self, ctx: &Context, palette: Palette, events: &mut EventSink) {
        let now = ctx.input(|i| i.time);
        self.expire(now);
        self.glass.clear();
        self.show_dialog(ctx, palette, events);
        self.show_toasts(ctx, palette, now);
    }

    fn show_dialog(&mut self, ctx: &Context, palette: Palette, events: &mut EventSink) {
        let Some(dialog) = self.pending.front_mut() else { return };
        let id = dialog.id();
        let mut outcome = None;
        // A file the dialog asked to be shown. Collected out of the modal's closure rather
        // than pushed from inside it, because `events` is borrowed for the whole of
        // `show_dialog` and the closure already holds `dialog` mutably. Revealing a file is
        // not an answer to the dialog — the editor stays open — so it cannot ride `outcome`.
        let mut reveal: Option<std::path::PathBuf> = None;
        // An *Edit* pressed on one of the inherited rule layers. Collected out of the closure
        // for the same borrow reason as `reveal` — but unlike a reveal, this **does** answer
        // the dialog, because the app replaces this editor with one on that layer.
        let mut edit_layer: Option<vellum_agent::Layer> = None;

        // Opaque, always: a modal exists to stop everything else, and one you can see
        // through undermines its own job. `dialog_frame` cannot be made glass even
        // under a translucent palette, which is where that rule is enforced.
        let modal = egui::Modal::new(Id::new(("vellum-dialog", id)))
            .frame(dialog_frame(palette))
            .show(ctx, |ui| {
                // A reference sheet is two columns and dozens of rows; everything else
                // is a sentence and two buttons. One width cannot serve both, and 380px
                // is what made the shortcut sheet a wall of wrapped text.
                ui.set_max_width(match dialog {
                    Dialog::Reference { .. } => space::of(140),
                    // Wider than a sentence and narrower than the sheet: each step is a
                    // line of instruction that must not wrap into three, and a list that
                    // reaches 140 makes the eye travel back across the whole modal to find
                    // the next number.
                    Dialog::Steps { .. } => space::of(118),
                    // A form, not a question: a label column, a control and — in the rules
                    // editor — a provenance caption hard right, on the same line.
                    Dialog::Schedule { .. } | Dialog::Rules { .. } => space::of(125),
                    _ => space::of(95),
                });
                match dialog {
                    Dialog::Confirm { title, message, confirm, destructive, .. } => {
                        ui.label(screen_title(title.as_str()).color(palette.text));
                        ui.add_space(space::of(2));
                        ui.label(egui::RichText::new(message.as_str()).color(palette.muted));
                        ui.add_space(space::of(5));
                        // Enter agrees, but **only when agreeing is not destructive**.
                        // There is no text field here to route the key through, so this is
                        // a plain read rather than the focus dance the `Rename` arm needs.
                        // A `Delete board` dialog deliberately has no keyboard shortcut:
                        // the whole point of asking is that the answer be deliberate, and
                        // Enter is what a user presses to dismiss something they have not
                        // read. Escape still cancels either way.
                        let submitted =
                            !*destructive && ui.input(|i| i.key_pressed(Key::Enter));
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // Destructive and affirmative wear the same red:
                                // `xr-red` carries both roles, and the button's label
                                // — "Delete", never "OK" — is what distinguishes them.
                                let fill =
                                    if *destructive { palette.danger } else { palette.accent };
                                let button = egui::Button::new(
                                    egui::RichText::new(confirm.as_str())
                                        .color(palette.on_accent),
                                )
                                .fill(fill);
                                if ui.add(button).clicked() || submitted {
                                    outcome = Some(DialogEvent::Confirmed(id));
                                }
                                if ui.button("Cancel").clicked() {
                                    outcome = Some(DialogEvent::Cancelled(id));
                                }
                            });
                        });
                    }
                    Dialog::Rename { title, value, confirm, hint, .. } => {
                        ui.label(screen_title(title.as_str()).color(palette.text));
                        ui.add_space(space::of(3));
                        let field_id = Id::new(("vellum-dialog-field", id));
                        let field = ui.add(
                            egui::TextEdit::singleline(value)
                                .id(field_id)
                                .desired_width(f32::INFINITY)
                                .hint_text(hint.as_str()),
                        );
                        // **Read the focus edge before re-taking focus below.** `TextEdit`
                        // handles Enter itself by surrendering focus, and that surrender
                        // is the only signal it gives — there is no "submitted" flag on
                        // the response. `request_focus` sets `focused_widget` back
                        // synchronously, so a `lost_focus()` read *after* it can never be
                        // true. That one line of ordering is why Enter did nothing at all
                        // in this dialog: the `submitted` term below was wired into the
                        // confirm button from the start and never once evaluated true.
                        //
                        // Not an event-consumption problem, which was the first guess:
                        // `TextEdit` reads through `filtered_events`, which clones rather
                        // than consumes, so `key_pressed` here still sees the Enter.
                        let submitted =
                            field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

                        // `Memory::has_focus`, not `Response::has_focus`: the latter is
                        // false whenever the *OS window* is unfocused, so clicking away
                        // and back counted as a fresh open and re-selected the name under
                        // the user's cursor.
                        let first_frame = !ui.memory(|memory| memory.has_focus(field_id));
                        if !submitted {
                            field.request_focus();
                            // Select the seed on the way in, so the first keystroke
                            // replaces it. *"when i am making a new board it should
                            // immidietly start by selecting the title so that when i start
                            // typing it clears it"* — and it is what Finder and every
                            // rename field do. Latched on the frame focus is taken;
                            // re-selecting every frame would make the field impossible to
                            // click into.
                            if first_frame {
                                select_all(ui.ctx(), field_id, value);
                            }
                        }
                        ui.add_space(space::of(5));
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let valid = !value.trim().is_empty();
                                let save = ui.add_enabled(
                                    valid,
                                    egui::Button::new(
                                        egui::RichText::new(confirm.as_str())
                                            .color(palette.on_accent),
                                    )
                                    .fill(palette.accent),
                                );
                                if (save.clicked() || submitted) && valid {
                                    outcome = Some(DialogEvent::Renamed(
                                        id,
                                        value.trim().to_owned(),
                                    ));
                                }
                                if ui.button("Cancel").clicked() {
                                    outcome = Some(DialogEvent::Cancelled(id));
                                }
                            });
                        });
                    }
                    Dialog::Reference { title, sections, dismiss, .. } => {
                        ui.label(screen_title(title.as_str()).color(palette.text));
                        ui.add_space(space::of(3));
                        reference_sheet(ui, palette, sections);
                        ui.add_space(space::of(4));
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let button = egui::Button::new(
                                    egui::RichText::new(dismiss.as_str())
                                        .color(palette.on_accent),
                                )
                                .fill(palette.accent);
                                if ui.add(button).clicked() {
                                    outcome = Some(DialogEvent::Confirmed(id));
                                }
                            });
                        });
                    }
                    Dialog::Steps { title, intro, steps, confirm, .. } => {
                        ui.label(screen_title(title.as_str()).color(palette.text));
                        ui.add_space(space::of(2));
                        ui.label(egui::RichText::new(intro.as_str()).color(palette.muted));
                        ui.add_space(space::of(4));
                        step_list(ui, palette, steps);
                        ui.add_space(space::of(5));
                        // Enter continues. Non-destructive — nothing has happened yet and
                        // the next dialog still has to be answered — so the `Confirm` arm's
                        // rule about Enter and destruction does not bite here.
                        let submitted = ui.input(|i| i.key_pressed(Key::Enter));
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let button = egui::Button::new(
                                    egui::RichText::new(confirm.as_str())
                                        .color(palette.on_accent),
                                )
                                .fill(palette.accent);
                                if ui.add(button).clicked() || submitted {
                                    outcome = Some(DialogEvent::Confirmed(id));
                                }
                                if ui.button("Cancel").clicked() {
                                    outcome = Some(DialogEvent::Cancelled(id));
                                }
                            });
                        });
                    }
                    // The three agent forms. Each draws its own body in
                    // `crate::agent_dialogs` and answers what the user pressed; the mapping
                    // from that answer to a `DialogEvent` is here, so the payload rides the
                    // id the app asked with.
                    Dialog::Schedule { title, schedule, targets, utc_offset, confirm, .. } => {
                        ui.label(screen_title(title.as_str()).color(palette.text));
                        ui.add_space(space::of(3));
                        egui::ScrollArea::vertical().max_height(space::of(150)).show(ui, |ui| {
                            match crate::agent_dialogs::schedule_editor(
                                ui,
                                palette,
                                schedule,
                                targets.as_slice(),
                                *utc_offset,
                                confirm.as_str(),
                            ) {
                                Some(crate::agent_dialogs::Answer::Save) => {
                                    outcome =
                                        Some(DialogEvent::ScheduleSet(id, Some(schedule.clone())));
                                }
                                // `None`, not a disabled schedule: the two are different
                                // states and only one of them keeps the prompt.
                                Some(crate::agent_dialogs::Answer::Clear) => {
                                    outcome = Some(DialogEvent::ScheduleSet(id, None));
                                }
                                Some(crate::agent_dialogs::Answer::Cancel) => {
                                    outcome = Some(DialogEvent::Cancelled(id));
                                }
                                None => {}
                            }
                        });
                    }
                    Dialog::Rules { title, form, resolved, files, confirm, .. } => {
                        ui.label(screen_title(title.as_str()).color(palette.text));
                        ui.add_space(space::of(3));
                        egui::ScrollArea::vertical().max_height(space::of(150)).show(ui, |ui| {
                            match crate::agent_dialogs::rules_editor(
                                ui,
                                palette,
                                form,
                                resolved,
                                files,
                                &mut reveal,
                                &mut edit_layer,
                                confirm.as_str(),
                            ) {
                                Some(crate::agent_dialogs::Answer::Save) => {
                                    outcome = Some(DialogEvent::RulesSet(
                                        id,
                                        form.clone().into_rules(),
                                    ));
                                }
                                Some(
                                    crate::agent_dialogs::Answer::Cancel
                                    | crate::agent_dialogs::Answer::Clear,
                                ) => {
                                    outcome = Some(DialogEvent::Cancelled(id));
                                }
                                None => {}
                            }
                        });
                    }
                    Dialog::SignIn { provider, has_key, key, confirm, .. } => {
                        match crate::agent_dialogs::sign_in(
                            ui,
                            palette,
                            *provider,
                            *has_key,
                            key,
                            confirm.as_str(),
                        ) {
                            Some(crate::agent_dialogs::Answer::Save) => {
                                outcome = Some(DialogEvent::SignedIn(id, key.clone()));
                            }
                            Some(
                                crate::agent_dialogs::Answer::Cancel
                                | crate::agent_dialogs::Answer::Clear,
                            ) => {
                                outcome = Some(DialogEvent::Cancelled(id));
                            }
                            None => {}
                        }
                    }
                }
            });

        // Escape and a click on the backdrop both mean cancel — never confirm, and
        // never nothing, which would leave a dialog that cannot be dismissed.
        if outcome.is_none() && modal.should_close() {
            outcome = Some(DialogEvent::Cancelled(id));
        }

        // Before the outcome, so a *Show* pressed on the same frame something closed the
        // dialog still reaches the app. Nothing here can produce both, and depending on that
        // is how the one case that does arrive later gets dropped.
        if let Some(path) = reveal {
            events.push(UiEvent::RevealPath(path));
        }

        // Only when nothing else answered. The two cannot both happen in one frame — a click
        // lands on one button — and preferring the real answer means that if they ever could,
        // a *Save* is never discarded in favour of reopening the editor somewhere else.
        if outcome.is_none()
            && let Some(layer) = edit_layer
        {
            outcome = Some(DialogEvent::EditRuleLayer(id, layer));
        }

        if let Some(outcome) = outcome {
            self.pending.pop_front();
            events.push(UiEvent::Dialog(outcome));
        }
    }

    fn show_toasts(&mut self, ctx: &Context, palette: Palette, now: f64) {
        if self.toasts.is_empty() {
            return;
        }
        // Toasts sit bottom-left: the status cluster owns bottom-right, and the top
        // is the menu bar. Nothing important on a board lives in that corner.
        let mut drawn: Vec<egui::Rect> = Vec::with_capacity(self.toasts.len());
        egui::Area::new(Id::new("vellum-toasts"))
            .anchor(egui::Align2::LEFT_BOTTOM, vec2(space::of(4), -space::of(4)))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = space::of(2);
                for live in &self.toasts {
                    // Fade the last half-second rather than blinking out. 500ms is
                    // slower than §3's 120–160ms because this is the one motion the
                    // user did not cause, and it has to be readable on its way out.
                    let remaining = (live.expires_at - now) as f32;
                    let opacity = remaining.clamp(0.0, 0.5) / 0.5;
                    let accent = live.toast.kind.accent(palette);
                    let inner = floating_frame(palette)
                        .inner_margin(egui::Margin::symmetric(
                            space::of(3) as i8,
                            space::of(2) as i8,
                        ))
                        .multiply_with_opacity(opacity)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::splat(space::of(4)),
                                    egui::Sense::hover(),
                                );
                                live.toast.kind.icon().paint(
                                    &ui.painter().clone(),
                                    rect,
                                    accent,
                                    crate::widgets::ICON_STROKE,
                                );
                                ui.label(
                                    egui::RichText::new(live.toast.text.as_str())
                                        .color(palette.text),
                                );
                            });
                        });
                    paint_glass_edge(
                    ui.painter(),
                    inner.response.rect,
                    palette,
                    Backing::Canvas,
                    f32::from(radius::LARGE),
                );
                    drawn.push(inner.response.rect);
                }
            });

        if let Some(glass) = palette.glass(Backing::Canvas) {
            self.glass.extend(drawn.into_iter().filter(egui::Rect::is_positive).map(|rect| {
                GlassSurface {
                    rect,
                    corner_radius: f32::from(radius::LARGE),
                    opacity: glass.opacity,
                }
            }));
        }

        // Without this the toast never disappears on an idle canvas: egui only
        // repaints on input, and expiry is a function of the clock, not of input.
        let next = self.toasts.iter().map(|t| t.expires_at).fold(f64::INFINITY, f64::min);
        if next.is_finite() {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((next - now).max(0.0)));
        }
    }
}

/// Selects a text field's whole contents, so the next keystroke replaces it.
///
/// egui keeps a `TextEdit`'s caret in its own memory rather than on the response, so
/// this loads that state, sets the range and stores it back. It needs the field's `Id`
/// to be one we chose — the default is derived from position, which moves.
///
/// Measured in **characters**, not bytes: `CCursor` counts characters, and a board named
/// with anything outside ASCII would otherwise select a prefix that ends mid-glyph.
pub(crate) fn select_all(ctx: &Context, field: Id, value: &str) {
    let Some(mut state) = egui::TextEdit::load_state(ctx, field) else { return };
    let end = egui::text::CCursor::new(value.chars().count());
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
    state.store(ctx, field);
}

/// Lays a [`Dialog::Reference`]'s sections out as two aligned columns.
///
/// The columns are made with a real layout, not with space padding. The old sheet was
/// one `format!("  {:<28} {}")` per line rendered in the *proportional* body face,
/// where padding aligns nothing — which is the ragged right-hand column the user saw.
///
/// The right column is right-aligned and drawn in the numeric monospace face, matching
/// the command palette and the menu bar: a key combination is a value, and every place
/// in the app that shows one shows it the same way.
fn reference_sheet(ui: &mut Ui, palette: Palette, sections: &[ReferenceSection]) {
    // Bounded, then scrolled. Forty-eight rows do not fit any window worth assuming,
    // and the old dialog had no scroll at all — it simply ran off the bottom.
    egui::ScrollArea::vertical().max_height(space::of(150)).show(ui, |ui| {
        for (index, section) in sections.iter().enumerate() {
            if index > 0 {
                ui.add_space(space::of(3));
            }
            ui.label(crate::theme::section_label(section.heading.as_str(), palette));
            ui.add_space(space::UNIT);

            for (what, keys) in &section.rows {
                ui.horizontal(|ui| {
                    let height = ui.spacing().interact_size.y;
                    // The key column is fixed and the action column takes the rest, so
                    // every key in the sheet starts at the same x whatever sits left of
                    // it. `Shift+Cmd+CloseBracket` was the widest thing here before the
                    // punctuation fix; `Shift+Cmd+]` is the widest now.
                    let keys_width = space::of(30);
                    let actions_width = (ui.available_width() - keys_width).max(space::of(20));
                    // `allocate_ui_with_layout`, not `add_sized`: the latter *centres*
                    // its child in the space given, which set every action floating at
                    // a different x and undid the alignment this column exists for.
                    ui.allocate_ui_with_layout(
                        egui::vec2(actions_width, height),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(what.as_str()).color(palette.text),
                                )
                                .selectable(false)
                                .wrap_mode(egui::TextWrapMode::Truncate),
                            );
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(crate::theme::numeric(keys.as_str()).color(palette.muted));
                    });
                });
            }
        }
    });
}

/// Lays a [`Dialog::Steps`]'s instructions out as numbered rows, each with a ⓘ.
///
/// The number is **drawn from the row's position**, not carried on the [`Step`], so a step
/// inserted in the middle cannot leave the list reading 1, 2, 2, 4 — the failure a
/// hand-numbered string invites and that nothing would catch, because the numbers are
/// inside the sentences.
///
/// The ⓘ is a **drawn circle with a letter in it**, not the character `ⓘ`: U+24D8 is
/// outside every plain sans face this app ships or falls back to (trap 10), so it would
/// draw tofu on the one control whose whole job is to look like an offer of help.
fn step_list(ui: &mut Ui, palette: Palette, steps: &[Step]) {
    for (index, step) in steps.iter().enumerate() {
        if index > 0 {
            ui.add_space(space::of(2));
        }
        ui.horizontal_top(|ui| {
            // A fixed number column, so every instruction starts at the same x whether its
            // number is one digit or two.
            let column = space::of(5);
            ui.allocate_ui_with_layout(
                vec2(column, ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::TOP),
                |ui| {
                    ui.label(
                        crate::theme::numeric(format!("{}.", index + 1)).color(palette.accent),
                    );
                },
            );
            // The ⓘ is reserved on the right and the sentence takes what is left, so a
            // wrapped instruction does not push the badge off the modal.
            let badge = space::of(6);
            let text_width = (ui.available_width() - badge).max(space::of(20));
            ui.allocate_ui_with_layout(
                vec2(text_width, 0.0),
                egui::Layout::left_to_right(egui::Align::TOP),
                |ui| {
                    ui.label(egui::RichText::new(step.what.as_str()).color(palette.text));
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                info_badge(ui, palette).on_hover_text(step.why.as_str());
            });
        });
    }
}

/// The ⓘ: a ringed circle with an `i` in it, sized to a menu row's icon.
///
/// `Sense::hover` rather than a button — it does nothing when clicked, and a control that
/// depresses and then does nothing is worse than one that plainly cannot be pressed.
fn info_badge(ui: &mut Ui, palette: Palette) -> egui::Response {
    let side = space::of(4);
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(side), egui::Sense::hover());
    // Brighter under the pointer, which is the only thing that says a tooltip is coming.
    let ink = if response.hovered() { palette.accent } else { palette.muted };
    let painter = ui.painter();
    painter.circle_stroke(rect.center(), side * 0.5 - 1.0, egui::Stroke::new(1.2, ink));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "i",
        egui::FontId::proportional(side * 0.62),
        ink,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light() -> Palette {
        Palette::LIGHT
    }

    #[test]
    fn dialogs_queue_rather_than_stack() {
        let mut stack = DialogStack::default();
        assert!(!stack.is_showing());
        stack.push(Dialog::confirm(DialogId(1), "First", "…", "Yes"));
        stack.push(Dialog::confirm(DialogId(2), "Second", "…", "Yes"));
        assert_eq!(stack.current().map(Dialog::id), Some(DialogId(1)));
        assert!(stack.is_showing());
    }

    #[test]
    fn a_dismissed_dialog_leaves_the_queue_wherever_it_sits() {
        let mut stack = DialogStack::default();
        stack.push(Dialog::confirm(DialogId(1), "First", "…", "Yes"));
        stack.push(Dialog::rename(DialogId(2), "Rename", "Board"));
        stack.dismiss(DialogId(2));
        stack.dismiss(DialogId(1));
        assert!(!stack.is_showing());
    }

    /// The button says what it will do. Every one-field dialog used to say "Rename",
    /// including **New board**, which offered to rename a board that did not exist yet.
    #[test]
    fn a_one_field_dialog_labels_its_button_with_the_thing_it_does() {
        let create = Dialog::rename(DialogId(1), "New board", "Untitled board")
            .with_confirm("Create")
            .with_hint("Board name");
        let Dialog::Rename { confirm, hint, value, .. } = &create else { panic!("wrong variant") };
        assert_eq!(confirm, "Create");
        assert_eq!(hint, "Board name");
        assert_eq!(value, "Untitled board", "the seed is what gets selected on open");

        // Renaming keeps the default, so the plain call is still the plain case.
        let rename = Dialog::rename(DialogId(2), "Rename board", "garage");
        let Dialog::Rename { confirm, .. } = &rename else { panic!("wrong variant") };
        assert_eq!(confirm, "Rename");
    }

    #[test]
    fn a_destructive_confirm_keeps_its_text_and_sets_the_flag() {
        let dialog = Dialog::destructive(DialogId(3), "Delete board", "Gone for good.", "Delete");
        match dialog {
            Dialog::Confirm { title, confirm, destructive, .. } => {
                assert_eq!(title, "Delete board");
                assert_eq!(confirm, "Delete");
                assert!(destructive);
            }
            Dialog::Rename { .. }
            | Dialog::Reference { .. }
            | Dialog::Steps { .. }
            | Dialog::Schedule { .. }
            | Dialog::Rules { .. }
            | Dialog::SignIn { .. } => {
                panic!("wrong variant")
            }
        }
    }

    /// A sign-in dialog never carries a key in, so there is no state in which a stored
    /// credential is on screen. `has_key` is the whole of what this crate is told.
    #[test]
    fn a_sign_in_starts_empty_even_when_a_key_is_already_stored() {
        let dialog = Dialog::sign_in(DialogId(11), vellum_agent::Provider::Kimi, true);
        let Dialog::SignIn { key, has_key, confirm, .. } = &dialog else { panic!("wrong variant") };
        assert!(has_key);
        assert!(key.is_empty(), "an existing key must never be seeded into the field");
        assert_eq!(confirm, "Replace", "the button says what pressing it does to the old one");

        let fresh = Dialog::sign_in(DialogId(12), vellum_agent::Provider::Kimi, false);
        let Dialog::SignIn { confirm, .. } = &fresh else { panic!("wrong variant") };
        assert_eq!(confirm, "Sign in");

        // And the whole dialog is `Debug`, which is exactly why the key type is not.
        let mut typed = Dialog::sign_in(DialogId(13), vellum_agent::Provider::Kimi, false);
        if let Dialog::SignIn { key, .. } = &mut typed {
            key.buffer_mut().push_str("sk-should-never-print");
        }
        assert!(!format!("{typed:?}").contains("should-never-print"), "{typed:?}");
    }

    /// The schedule editor is seeded with what the node has and with the two facts the
    /// chrome cannot know: who is reachable, and what the user's clock offset is.
    #[test]
    fn a_schedule_dialog_carries_the_facts_the_chrome_cannot_work_out() {
        let targets = vec![crate::AgentLink { id: "2@1".into(), label: "Builder".into() }];
        let dialog = Dialog::schedule(
            DialogId(14),
            "Schedule",
            vellum_agent::Schedule::default(),
            targets.clone(),
            3 * 3600,
        );
        let Dialog::Schedule { targets: carried, utc_offset, confirm, .. } = &dialog else {
            panic!("wrong variant")
        };
        assert_eq!(carried, &targets, "the hand-off picker offers only these");
        assert_eq!(*utc_offset, 10_800, "seconds, so a half-hour timezone is expressible");
        assert_eq!(confirm, "Save");
    }

    /// The instructions in front of Import from Miro. Two things are asserted rather than
    /// the words: that **every** step carries a reason — a ⓘ with nothing behind it is a
    /// control that promises help and gives none, which is worse than no badge — and that
    /// the button says what happens next rather than "OK".
    #[test]
    fn every_step_of_a_steps_dialog_says_why_it_matters() {
        let dialog = Dialog::steps(
            DialogId(8),
            "Import from Miro",
            "Two steps happen in Miro, two here.",
            vec![Step::new("Press ⌘A", "Velm copies what is selected."), Step::new("Press ⌘C", "")],
        );
        let Dialog::Steps { steps, confirm, .. } = &dialog else { panic!("wrong variant") };
        assert_eq!(confirm, "Continue");
        let missing: Vec<&str> = steps
            .iter()
            .filter(|s| s.why.trim().is_empty())
            .map(|s| s.what.as_str())
            .collect();
        assert_eq!(
            missing,
            vec!["Press ⌘C"],
            "a step with no reason draws a ⓘ that answers nothing when hovered"
        );
    }

    #[test]
    fn toasts_expire_on_the_clock_and_errors_outlast_the_rest() {
        let mut stack = DialogStack::default();
        stack.toast(Toast::info("Saved"), 0.0);
        stack.toast(Toast::error("Import failed"), 0.0);
        assert_eq!(stack.toast_count(), 2);

        stack.expire(4.0);
        assert_eq!(stack.toast_count(), 1, "the info toast has gone");
        stack.expire(8.0);
        assert_eq!(stack.toast_count(), 0);
    }

    /// Escape must resolve the dialog, and must resolve it as *cancelled*. A modal
    /// that swallows Escape is the worst failure mode available to this code.
    #[test]
    fn escape_cancels_the_showing_dialog_and_reports_its_id() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        stack.push(Dialog::confirm(DialogId(9), "Close board", "Unsaved changes.", "Discard"));

        // First pass lays the modal out; the escape is delivered on the second.
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            stack.show(ui.ctx(), light(), &mut events);
        });
        assert!(events.is_empty());

        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run_ui(input, |ui| {
            stack.show(ui.ctx(), light(), &mut events);
        });

        assert_eq!(
            events.take(),
            vec![UiEvent::Dialog(DialogEvent::Cancelled(DialogId(9)))]
        );
        assert!(!stack.is_showing());
    }

    /// Drives one key through the dialog, over two passes: the first lays the modal
    /// out and focuses the field, the second delivers the key. One pass is not enough
    /// — the field cannot lose focus on the frame it first takes it.
    fn press(stack: &mut DialogStack, events: &mut EventSink, key: Key) {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            stack.show(ui.ctx(), light(), events);
        });
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run_ui(input, |ui| {
            stack.show(ui.ctx(), light(), events);
        });
    }

    /// *"when i press enter it should automatically create it"*.
    ///
    /// This is the regression test for the ordering bug: `TextEdit` surrenders focus to
    /// signal Enter, and the `request_focus` that keeps the field focused used to run
    /// *before* that edge was read, so it erased the only evidence the key had been
    /// pressed. Note what this test does **not** do: synthesise a "submitted" flag. It
    /// presses the key and asserts the event, which is the only version that fails
    /// against the old code.
    #[test]
    fn enter_confirms_a_one_field_dialog_and_reports_the_trimmed_value() {
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        stack.push(Dialog::rename(DialogId(4), "New space", "  Cars  ").with_confirm("Create"));

        press(&mut stack, &mut events, Key::Enter);

        assert_eq!(
            events.take(),
            vec![UiEvent::Dialog(DialogEvent::Renamed(DialogId(4), "Cars".to_owned()))],
            "Enter must confirm, and must trim the way the button does"
        );
        assert!(!stack.is_showing(), "confirming pops the dialog");
    }

    /// A one-field dialog cannot be confirmed into a nameless board. The button is
    /// disabled for an empty value and Enter must agree with the button.
    #[test]
    fn enter_does_nothing_when_the_field_is_empty() {
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        stack.push(Dialog::rename(DialogId(5), "New board", "   "));

        press(&mut stack, &mut events, Key::Enter);

        assert!(events.is_empty(), "an all-whitespace name is not a name");
        assert!(stack.is_showing(), "the dialog stays up to be corrected");
    }

    #[test]
    fn enter_confirms_a_plain_question() {
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        let question = Dialog::confirm(DialogId(6), "Replace", "A board exists.", "Replace");
        stack.push(question);

        press(&mut stack, &mut events, Key::Enter);

        assert_eq!(events.take(), vec![UiEvent::Dialog(DialogEvent::Confirmed(DialogId(6)))]);
    }

    /// The twin of the test above, and the more important of the two. Enter is what a
    /// user presses to make a dialog go away without reading it, so it must not be able
    /// to delete a board.
    #[test]
    fn enter_never_confirms_a_destructive_question() {
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        stack.push(Dialog::destructive(DialogId(7), "Delete board", "Gone for good.", "Delete"));

        press(&mut stack, &mut events, Key::Enter);

        assert!(events.is_empty(), "Enter must not delete anything");
        assert!(stack.is_showing());

        // Escape still resolves it, so it is not a dialog with no keyboard escape.
        press(&mut stack, &mut events, Key::Escape);
        assert_eq!(events.take(), vec![UiEvent::Dialog(DialogEvent::Cancelled(DialogId(7)))]);
    }

    #[test]
    fn toasts_render_without_a_dialog_present() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Dark);
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        stack.toast(Toast::success("Board saved"), 0.0);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            stack.show(ui.ctx(), Palette::DARK, &mut events);
        });
        assert!(events.is_empty());
    }

    /// A toast floats over the canvas and is glass; the modal beneath it never is.
    /// If the modal ever appeared in this list the renderer would blur the board
    /// behind a surface whose whole job is to stop you looking at it.
    #[test]
    fn toasts_are_glass_and_the_modal_is_not() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        stack.toast(Toast::info("Saved"), 0.0);
        stack.toast(Toast::error("Import failed"), 0.0);
        stack.push(Dialog::confirm(DialogId(1), "Close board", "Unsaved changes.", "Discard"));

        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            stack.show(ui.ctx(), light(), &mut events);
        });
        assert_eq!(stack.glass_surfaces().len(), 2, "one per toast, and none for the modal");
        assert!(stack.glass_surfaces().iter().all(|g| g.opacity < u8::MAX));

        // …and a modal drawn under reduced transparency is still opaque, which it was
        // anyway — the point is that the *toasts* stop being glass too.
        let opaque = Palette::LIGHT.opaque();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            stack.show(ui.ctx(), opaque, &mut events);
        });
        assert!(stack.glass_surfaces().is_empty());
        let _ = events.take();
    }

    /// An expired toast must take its glass region with it, or the renderer keeps
    /// blurring a rectangle with nothing in it.
    #[test]
    fn an_expired_toast_withdraws_its_glass_region() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut stack = DialogStack::default();
        let mut events = EventSink::default();
        stack.toast(Toast::info("Saved"), 0.0);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            stack.show(ui.ctx(), light(), &mut events);
        });
        assert_eq!(stack.glass_surfaces().len(), 1);

        stack.expire(99.0);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            stack.show(ui.ctx(), light(), &mut events);
        });
        assert!(stack.glass_surfaces().is_empty());
    }
}
