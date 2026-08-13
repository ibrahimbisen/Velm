//! The three modal editors the Agent Canvas layer needs: a schedule, a rule cascade, and a
//! provider sign-in.
//!
//! `docs/07-agent-canvas.md` §1's module map names this file. Each of the three is a form
//! rather than a question, which is why none of them is a [`Dialog::Confirm`] with a long
//! message: a recurrence with three shapes, a four-field cascade showing where every value
//! came from, and a masked field are things a *renderer* does, and `Dialog` is a plain data
//! enum in a `VecDeque` that derives `PartialEq` and so cannot hold a closure that draws
//! itself. The same reasoning already produced [`Dialog::Reference`] and [`Dialog::Steps`].
//!
//! # The dialogs are raised by the app, not by the chrome
//!
//! `crate::dialog`'s contract, unchanged: the panel emits [`Command::EditAgentSchedule`] or
//! [`Command::EditAgentRules`], the app pushes the dialog seeded with the node's current
//! value **and with the facts this crate cannot know** — which agents are reachable along a
//! connector, what the user's UTC offset is — and hears the answer back as a
//! [`DialogEvent`](crate::DialogEvent) carrying the same [`DialogId`](crate::DialogId). The
//! id is how the app already knows which node it asked about.
//!
//! # A hand-off is refused when the schedule is saved
//!
//! Feature 10's sharp edge, and the reason the target list is passed in. A completion that
//! hands work to an agent with no connector to it cannot be delivered — `bus.rs` refuses a
//! message with no link — so the picker offers **only** reachable agents, and a target that
//! has since become unreachable disables *Save* with a sentence naming it. Discovering that
//! at six in the evening, in a transcript nobody is watching, is the outcome this prevents.
//!
//! [`Command::EditAgentRules`]: crate::Command::EditAgentRules
//! [`Command::EditAgentSchedule`]: crate::Command::EditAgentSchedule
//! [`Dialog::Confirm`]: crate::Dialog::Confirm
//! [`Dialog::Reference`]: crate::Dialog::Reference
//! [`Dialog::Steps`]: crate::Dialog::Steps

use std::collections::BTreeMap;

use crate::event::SecretKey;
use crate::selection::{AgentLink, Field};
use crate::theme::{Palette, screen_title, space, tabular};
use crate::widgets::{Segment, hairline, section_header, segmented};
use egui::Ui;
use vellum_agent::rules::{Field as RuleField, FrontMatter, Permissions};
use vellum_agent::{
    AgentRules, Completion, Layer, Provider, Recurrence, ResolvedRules, RuleFile, Schedule, Trigger,
};

/// How a form dialog was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    Save,
    /// Remove the value entirely — a schedule only. Distinct from [`Self::Cancel`], which
    /// leaves what was there.
    Clear,
    Cancel,
}

// ============================================================================
// The rules editor — feature 11
// ============================================================================

/// The node's own rule layer, as a form.
///
/// Held as separate fields rather than as the [`AgentRules::text`] the document stores,
/// because a text field needs a `String` that survives the frame: re-composing the front
/// matter on every keystroke and re-deriving the buffer from it would put the caret back at
/// the end each time. [`Self::into_rules`] is where the two become one string again, and it
/// is the only place front matter is written.
///
/// **The structured settings live in the front matter and nowhere else.**
/// `docs/07-agent-canvas.md` §7 withdrew the `rules.json` beside the markdown for exactly the
/// reason this form has to respect: two files describing the same four settings is a second
/// source of truth for a value the user may also edit by hand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RulesForm {
    pub tone: String,
    pub output: String,
    pub language: String,
    /// `None` inherits the posture from the layer above — which, for the one field here
    /// that is a *capability* rather than a preference, is the safe direction.
    pub permissions: Option<Permissions>,
    /// The prose after the front matter.
    pub body: String,
    /// Whether the global and project layers apply at all. A cliff, not a slope.
    pub ignore_inherited: bool,
    /// Front-matter keys this build does not know, kept verbatim and in file order.
    ///
    /// Carried through the editor untouched. A rules file written for a later build, or
    /// shared with another tool, must survive a round trip through this one — the same
    /// posture `AgentModel`'s token takes toward unknown JSON fields.
    pub extra: BTreeMap<String, String>,
}

impl RulesForm {
    /// Read a node's own layer into the form.
    pub fn from_rules(rules: &AgentRules) -> Self {
        let file = RuleFile::parse(&rules.text);
        Self {
            tone: file.front.tone.clone().unwrap_or_default(),
            output: file.front.output.clone().unwrap_or_default(),
            language: file.front.language.clone().unwrap_or_default(),
            permissions: file.front.permissions,
            body: file.body.clone(),
            ignore_inherited: rules.ignore_inherited,
            extra: file.front.extra.clone(),
        }
    }

    /// Write the form back into a node's own layer.
    ///
    /// Note: [`AgentRules::overrides`] is left **empty on purpose**. It is a cache of what
    /// resolution decided, and the app writes it back from `ResolvedRules::override_names()`
    /// after applying the edit. Authoring it here would be a second, hand-written source of
    /// truth for provenance — and it would be the one the inspector believed while the agent
    /// ran on the other.
    pub fn into_rules(self) -> AgentRules {
        let front = FrontMatter {
            tone: non_empty(&self.tone),
            output: non_empty(&self.output),
            language: non_empty(&self.language),
            permissions: self.permissions,
            extra: self.extra,
        };
        AgentRules {
            text: compose(&front, &self.body),
            overrides: Vec::new(),
            ignore_inherited: self.ignore_inherited,
        }
    }

    /// Whether this node sets a field itself.
    ///
    /// Unused today: the editor draws provenance from [`ResolvedRules`]'s own record, which
    /// is the arrangement `docs/07-agent-canvas.md` §7 requires — the display must not
    /// re-derive what the agent actually got. Kept because it is the right predicate for the
    /// editor's own "cleared here" affordance, which is the next thing anyone will want.
    #[expect(dead_code, reason = "the editor reads provenance from ResolvedRules; see above")]
    fn sets(&self, field: RuleField) -> bool {
        match field {
            RuleField::Tone => !self.tone.trim().is_empty(),
            RuleField::Output => !self.output.trim().is_empty(),
            RuleField::Language => !self.language.trim().is_empty(),
            RuleField::Permissions => self.permissions.is_some(),
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Render front matter and body back into one markdown document.
///
/// The inverse of [`RuleFile::parse`], and it has to be exactly that: the file is
/// hand-editable, so a round trip through the editor must not rewrite what somebody typed
/// into a shape their next `git diff` does not recognise.
///
/// Three things are load-bearing:
///
/// - **The `---` must be the very first line.** `split_front_matter` returns *no* front
///   matter the moment the first non-empty line is anything else, so a leading blank line
///   would silently turn every setting into prose.
/// - **A value may not contain a newline**, or it would close the block early and the rest
///   would become body. Nothing in the editor produces one — the four fields are single-line
///   — and they are flattened anyway, because "nothing produces one" is a claim about today.
/// - **An empty front matter is omitted**, except when the body itself opens with `---`.
///   That one case would otherwise be re-read as front matter on the way back in, which is
///   the only way this function can corrupt a file rather than merely reformat one.
pub fn compose(front: &FrontMatter, body: &str) -> String {
    let body = body.trim();

    if front.is_empty() {
        return if body.starts_with("---") {
            // An empty block, so the body's own rule is not mistaken for a fence.
            format!("---\n---\n\n{body}")
        } else {
            body.to_owned()
        };
    }

    let mut out = String::from("---\n");
    // The four named settings first, in `Field::ALL`'s order, then whatever this build did
    // not recognise — so a file this editor has touched reads the same way twice.
    for (key, value) in [
        (RuleField::Tone.key(), front.tone.clone()),
        (RuleField::Output.key(), front.output.clone()),
        (RuleField::Language.key(), front.language.clone()),
        (
            RuleField::Permissions.key(),
            front.permissions.map(|posture| posture.tag().to_owned()),
        ),
    ] {
        if let Some(value) = value {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(&one_line(&value));
            out.push('\n');
        }
    }
    for (key, value) in &front.extra {
        out.push_str(key);
        out.push_str(": ");
        out.push_str(&one_line(value));
        out.push('\n');
    }
    out.push_str("---\n");

    if !body.is_empty() {
        out.push('\n');
        out.push_str(body);
        out.push('\n');
    }
    out
}

/// Flattens whitespace so a value cannot close the front-matter block.
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The rules editor's body — the resolved cascade, then the node's own layer.
pub(crate) fn rules_editor(
    ui: &mut Ui,
    palette: Palette,
    form: &mut RulesForm,
    resolved: &ResolvedRules,
    confirm: &str,
) -> Option<Answer> {
    let mut answer = None;

    // What the agent is actually run with, and where every part of it came from — read out
    // of the resolution rather than compared here. `docs/07-agent-canvas.md` §7.
    section_header(ui, palette, "In force now");
    for setting in resolved.rows() {
        ui.horizontal(|ui| {
            ui.add_sized(
                egui::vec2(space::of(24), ui.spacing().interact_size.y),
                egui::Label::new(
                    egui::RichText::new(setting.field.label()).color(palette.muted),
                )
                .selectable(false),
            );
            match &setting.value {
                Some(value) => {
                    ui.label(egui::RichText::new(value.as_str()).color(palette.text));
                }
                None => {
                    ui.label(egui::RichText::new("Not set").color(palette.faint));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let colour =
                    if setting.from == Layer::Agent { palette.accent } else { palette.faint };
                ui.label(
                    egui::RichText::new(setting.from.label())
                        .color(colour)
                        .size(crate::theme::text::LABEL),
                );
            });
        });
    }

    ui.add_space(space::of(2));
    hairline(ui, palette);
    section_header(ui, palette, "Set here");

    // Each field is a plain text box: leaving it empty **inherits**, which is the state the
    // cascade is for. There is no separate "inherit" switch, because two controls for one
    // value is the arrangement `CLAUDE.md`'s grid entry already had to remove once.
    for (field, buffer) in [
        (RuleField::Tone, &mut form.tone),
        (RuleField::Output, &mut form.output),
        (RuleField::Language, &mut form.language),
    ] {
        let inherited = inherited_value(resolved, field);
        ui.horizontal(|ui| {
            ui.add_sized(
                egui::vec2(space::of(24), ui.spacing().interact_size.y),
                egui::Label::new(egui::RichText::new(field.label()).color(palette.muted))
                    .selectable(false),
            );
            ui.add(
                egui::TextEdit::singleline(buffer)
                    .desired_width(f32::INFINITY)
                    .hint_text(inherited.unwrap_or_else(|| "Inherit".to_owned())),
            );
        });
    }

    // Permissions is a posture rather than a phrase, and it is the one field here that is a
    // capability boundary — so it is a closed set with an explicit *Inherit*, and an
    // unreadable value in a file falls through to the layer above rather than to a default.
    ui.horizontal(|ui| {
        ui.add_sized(
            egui::vec2(space::of(24), ui.spacing().interact_size.y),
            egui::Label::new(
                egui::RichText::new(RuleField::Permissions.label()).color(palette.muted),
            )
            .selectable(false),
        );
        let options = [
            Segment::text(None, "Inherit"),
            Segment::text(Some(Permissions::Ask), "Ask"),
            Segment::text(Some(Permissions::Reads), "Reads"),
            Segment::text(Some(Permissions::All), "Never ask"),
        ];
        if let Some(chosen) = segmented(ui, palette, &Field::Uniform(form.permissions), &options) {
            form.permissions = chosen;
        }
    });
    if let Some(posture) = form.permissions {
        ui.label(
            egui::RichText::new(posture.label())
                .color(palette.faint)
                .size(crate::theme::text::LABEL),
        );
    }

    ui.add_space(space::of(2));
    ui.label(
        egui::RichText::new("Instructions for this agent alone")
            .color(palette.muted)
            .size(crate::theme::text::LABEL),
    );
    ui.add(
        egui::TextEdit::multiline(&mut form.body)
            .desired_width(f32::INFINITY)
            .desired_rows(4)
            .hint_text("Appended last, so it wins."),
    );

    ui.add_space(space::of(2));
    // The cliff. `ignore_inherited` removes the global and project layers **entirely** —
    // their settings as well as their prose — so the warning says so rather than leaving
    // the reader to discover that a permission they set globally stopped applying.
    let mut ignore = form.ignore_inherited;
    if ui
        .checkbox(&mut ignore, "Ignore the global and project rules")
        .on_hover_text(
            "Removes both layers entirely — their settings as well as their words. \
             Partial inheritance is not offered: \"this agent ignores the house style \
             except for the parts it does not mention\" is not a sentence anybody can \
             reason about.",
        )
        .changed()
    {
        form.ignore_inherited = ignore;
    }

    ui.add_space(space::of(4));
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let save = egui::Button::new(egui::RichText::new(confirm).color(palette.on_accent))
                .fill(palette.accent);
            if ui.add(save).clicked() {
                answer = Some(Answer::Save);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });

    answer
}

/// What a field would resolve to if this node stopped setting it — the placeholder in the
/// empty text box, so *inherit* names what it inherits.
fn inherited_value(resolved: &ResolvedRules, field: RuleField) -> Option<String> {
    if resolved.provenance(field) == Layer::Agent {
        // Set here, so what the box would fall back to is not in the resolution — it was
        // overwritten. Say the honest thing rather than guessing at the layer beneath.
        return Some("Inherit".to_owned());
    }
    resolved.rows().into_iter().find(|row| row.field == field).and_then(|row| row.value)
}

// ============================================================================
// The schedule editor — feature 10
// ============================================================================

/// The three shapes a recurrence takes, as a value a segmented control can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Interval,
    Daily,
    Weekly,
}

impl Shape {
    const fn of(recurrence: Recurrence) -> Self {
        match recurrence {
            Recurrence::Interval { .. } => Self::Interval,
            Recurrence::Daily { .. } => Self::Daily,
            Recurrence::Weekly { .. } => Self::Weekly,
        }
    }
}

/// The four trigger conditions, as a value a picker can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerShape {
    Always,
    FilesChanged,
    NoteChanged,
    LastRunFailed,
}

impl TriggerShape {
    const ALL: [Self; 4] =
        [Self::Always, Self::FilesChanged, Self::NoteChanged, Self::LastRunFailed];

    const fn of(trigger: &Trigger) -> Self {
        match trigger {
            Trigger::Always => Self::Always,
            Trigger::FilesChanged => Self::FilesChanged,
            Trigger::NoteChanged { .. } => Self::NoteChanged,
            Trigger::LastRunFailed => Self::LastRunFailed,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Always => "Every time",
            Self::FilesChanged => "Only if files changed",
            Self::NoteChanged => "Only if a note changed",
            Self::LastRunFailed => "Only if the last run failed",
        }
    }
}

/// Why a schedule cannot be saved, or `None`.
///
/// **Pure, and the whole of feature 10's refusal.** A hand-off travels along a connector the
/// user drew; a target with no line to this agent cannot be delivered to, and `bus.rs` would
/// refuse the message at fire time — inside a transcript, at whatever hour the schedule was
/// set for, where nobody is looking. So it is refused here, at the moment the schedule is
/// saved, with the target named.
pub fn schedule_problem(schedule: &Schedule, targets: &[AgentLink]) -> Option<String> {
    let Completion::HandOff { agent } = &schedule.completion else { return None };

    if targets.iter().any(|link| &link.id == agent) {
        return None;
    }
    Some(if targets.is_empty() {
        "This agent is not connected to any other, so it has nobody to hand its work to. \
         Draw a connector between them first."
            .to_owned()
    } else {
        format!(
            "The agent this hands off to is no longer connected to this one. Pick one of \
             the {} it is connected to, or choose another ending.",
            targets.len()
        )
    })
}

/// The schedule editor's body.
pub(crate) fn schedule_editor(
    ui: &mut Ui,
    palette: Palette,
    schedule: &mut Schedule,
    targets: &[AgentLink],
    utc_offset: i32,
    confirm: &str,
) -> Option<Answer> {
    let mut answer = None;

    ui.checkbox(&mut schedule.enabled, "Run this agent on a schedule")
        .on_hover_text(
            "Off keeps everything below it, so turning a schedule off and on again does \
             not cost you the prompt you wrote.",
        );

    // Everything below is configuration, and it stays editable while the schedule is off —
    // that is what makes "off without deleting the configuration" mean anything.
    ui.add_space(space::of(2));
    section_header(ui, palette, "How often");
    ui.horizontal(|ui| {
        let options = [
            Segment::text(Shape::Interval, "Every N minutes"),
            Segment::text(Shape::Daily, "Daily"),
            Segment::text(Shape::Weekly, "Weekly"),
        ];
        let current = Shape::of(schedule.recurrence);
        if let Some(shape) = segmented(ui, palette, &Field::Uniform(current), &options)
            && shape != current
        {
            // Carry the time of day across a shape change, so switching from Daily to
            // Weekly and back does not lose the hour that was chosen.
            let seconds = time_of_day(schedule.recurrence).unwrap_or(18 * 3600);
            schedule.recurrence = match shape {
                Shape::Interval => Recurrence::Interval { minutes: 60 },
                Shape::Daily => {
                    Recurrence::Daily { seconds_after_midnight: seconds, utc_offset }
                }
                Shape::Weekly => Recurrence::Weekly {
                    weekday: 0,
                    seconds_after_midnight: seconds,
                    utc_offset,
                },
            };
        }
    });

    match &mut schedule.recurrence {
        Recurrence::Interval { minutes } => {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Every").color(palette.muted));
                let mut value = *minutes;
                let drag = tabular(ui, |ui| {
                    ui.add(egui::DragValue::new(&mut value).speed(1.0).range(1..=10_080))
                });
                if drag.changed() {
                    *minutes = value;
                }
                ui.label(egui::RichText::new("minutes").color(palette.muted));
            });
        }
        Recurrence::Daily { seconds_after_midnight, .. } => {
            clock_row(ui, palette, seconds_after_midnight);
        }
        Recurrence::Weekly { weekday, seconds_after_midnight, .. } => {
            weekday_row(ui, palette, weekday);
            clock_row(ui, palette, seconds_after_midnight);
        }
    }

    ui.add_space(space::of(2));
    section_header(ui, palette, "Only when");
    let shape = TriggerShape::of(&schedule.trigger);
    egui::ComboBox::from_id_salt("velm-schedule-trigger")
        .selected_text(shape.label())
        .width(space::of(60))
        .show_ui(ui, |ui| {
            for option in TriggerShape::ALL {
                if ui.selectable_label(option == shape, option.label()).clicked() {
                    schedule.trigger = match option {
                        TriggerShape::Always => Trigger::Always,
                        TriggerShape::FilesChanged => Trigger::FilesChanged,
                        TriggerShape::NoteChanged => {
                            Trigger::NoteChanged { path: String::new() }
                        }
                        TriggerShape::LastRunFailed => Trigger::LastRunFailed,
                    };
                }
            }
        });
    if let Trigger::NoteChanged { path } = &mut schedule.trigger {
        ui.add(
            egui::TextEdit::singleline(path)
                .desired_width(f32::INFINITY)
                .hint_text("Which note — a path under the project"),
        );
    }

    ui.add_space(space::of(2));
    section_header(ui, palette, "Afterwards");
    completion_row(ui, palette, schedule, targets);

    ui.add_space(space::of(2));
    ui.label(
        egui::RichText::new("What to ask it when it wakes")
            .color(palette.muted)
            .size(crate::theme::text::LABEL),
    );
    ui.add(
        egui::TextEdit::multiline(&mut schedule.prompt)
            .desired_width(f32::INFINITY)
            .desired_rows(3)
            .hint_text("Empty carries on with its standing instructions."),
    );

    ui.add_space(space::of(2));
    ui.label(egui::RichText::new(schedule.summary()).color(palette.muted));

    // Note: Enter is **not** wired to Save here, and that is a decision rather than an omission.
    // `crate::dialog`'s rule is that Enter confirms a non-destructive dialog — but this form
    // has a multiline prompt in it, where Enter means a new line. A key that sometimes
    // submits and sometimes types is worse than one that only ever types.
    let problem = schedule_problem(schedule, targets);
    if let Some(why) = &problem {
        ui.add_space(space::of(2));
        ui.label(egui::RichText::new(why.as_str()).color(palette.warning));
    }

    ui.add_space(space::of(4));
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let save = egui::Button::new(egui::RichText::new(confirm).color(palette.on_accent))
                .fill(palette.accent);
            let response = ui.add_enabled(problem.is_none(), save);
            if let Some(why) = &problem {
                response.on_disabled_hover_text(why.as_str());
            } else if response.clicked() {
                answer = Some(Answer::Save);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(Answer::Cancel);
            }
            if ui
                .button("Remove")
                .on_hover_text("Take the schedule off this agent. It then runs only when asked.")
                .clicked()
            {
                answer = Some(Answer::Clear);
            }
        });
    });

    answer
}

/// The time of day a recurrence carries, if it carries one.
const fn time_of_day(recurrence: Recurrence) -> Option<u32> {
    match recurrence {
        Recurrence::Interval { .. } => None,
        Recurrence::Daily { seconds_after_midnight, .. }
        | Recurrence::Weekly { seconds_after_midnight, .. } => Some(seconds_after_midnight),
    }
}

/// Hours and minutes, as two numbers.
///
/// Two fields rather than one text box, because a schedule is not a string: `18:0` and `6pm`
/// and `1800` are all things a person types into a free field, and only one of them can be
/// read back. Seconds are the stored unit — a half-hour timezone is why — so this is the one
/// place they are turned into a clock.
fn clock_row(ui: &mut Ui, palette: Palette, seconds_after_midnight: &mut u32) {
    let mut hours = *seconds_after_midnight / 3600;
    let mut minutes = (*seconds_after_midnight % 3600) / 60;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("At").color(palette.muted));
        let h = tabular(ui, |ui| {
            ui.add(egui::DragValue::new(&mut hours).speed(1.0).range(0..=23))
        });
        ui.label(egui::RichText::new(":").color(palette.muted));
        let m = tabular(ui, |ui| {
            ui.add(egui::DragValue::new(&mut minutes).speed(1.0).range(0..=59))
        });
        if h.changed() || m.changed() {
            *seconds_after_midnight = hours * 3600 + minutes * 60;
        }
        ui.label(
            egui::RichText::new("in your own day")
                .color(palette.faint)
                .size(crate::theme::text::LABEL),
        );
    });
}

const WEEKDAYS: [&str; 7] =
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

fn weekday_row(ui: &mut Ui, palette: Palette, weekday: &mut u8) {
    let current = usize::from(*weekday).min(6);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("On").color(palette.muted));
        egui::ComboBox::from_id_salt("velm-schedule-weekday")
            .selected_text(WEEKDAYS[current])
            .width(space::of(30))
            .show_ui(ui, |ui| {
                for (index, name) in WEEKDAYS.iter().enumerate() {
                    if ui.selectable_label(index == current, *name).clicked() {
                        *weekday = u8::try_from(index).unwrap_or(0);
                    }
                }
            });
    });
}

/// What happens when the run finishes — report, hand off, or nothing.
///
/// **The hand-off picker offers only reachable agents**, which is what makes an unreachable
/// target unreachable *by construction* rather than by validation. The validation in
/// [`schedule_problem`] is still needed, for the target that was reachable when the schedule
/// was written and is not now.
fn completion_row(
    ui: &mut Ui,
    palette: Palette,
    schedule: &mut Schedule,
    targets: &[AgentLink],
) {
    let selected = schedule.completion.label();
    egui::ComboBox::from_id_salt("velm-schedule-completion")
        .selected_text(selected)
        .width(space::of(60))
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(
                    matches!(schedule.completion, Completion::Report),
                    "Report back to me",
                )
                .clicked()
            {
                schedule.completion = Completion::Report;
            }
            if ui
                .selectable_label(matches!(schedule.completion, Completion::Nothing), "Do nothing")
                .clicked()
            {
                schedule.completion = Completion::Nothing;
            }

            if targets.is_empty() {
                // Named rather than absent: a missing option reads as a feature that does
                // not exist, and this one exists and is one connector away.
                ui.add_enabled(false, egui::Button::new("Hand off to…").frame(false))
                    .on_disabled_hover_text(crate::command::reason::NO_AGENT_LINK);
                return;
            }
            for link in targets {
                let on = matches!(
                    &schedule.completion,
                    Completion::HandOff { agent } if agent == &link.id
                );
                if ui.selectable_label(on, format!("Hand off to {}", link.label)).clicked() {
                    schedule.completion = Completion::HandOff { agent: link.id.clone() };
                }
            }
        });

    if targets.is_empty() {
        ui.label(
            egui::RichText::new(crate::command::reason::NO_AGENT_LINK)
                .color(palette.faint)
                .size(crate::theme::text::LABEL),
        );
    }
}

// ============================================================================
// Provider sign-in — features 16 and 17
// ============================================================================

/// The sign-in form: one masked field, and a great deal of saying what will happen to it.
///
/// **Nothing here ever shows a key.** The field starts empty even when one is already stored
/// — `has_key` is all this crate is told, per `docs/07-agent-canvas.md` §8a — so signing in
/// again *replaces*, and there is no state in which an existing credential is on screen to be
/// read over somebody's shoulder or captured in a screenshot.
pub(crate) fn sign_in(
    ui: &mut Ui,
    palette: Palette,
    provider: Provider,
    has_key: bool,
    key: &mut SecretKey,
    confirm: &str,
) -> Option<Answer> {
    let mut answer = None;

    ui.label(screen_title(format!("Sign in to {}", provider.label())).color(palette.text));
    ui.add_space(space::of(2));

    if provider.supports_subscription() {
        ui.label(
            egui::RichText::new(format!(
                "You may not need this. Velm runs {} through its own command-line tool, \
                 which already holds your subscription — no key, and nothing billed per \
                 token. A key is only needed when that tool is not installed.",
                provider.label()
            ))
            .color(palette.muted),
        );
        ui.add_space(space::of(2));
    }

    ui.label(
        egui::RichText::new(if has_key {
            "A key is already stored. Signing in again replaces it."
        } else {
            "The key is stored in Velm's own credentials file, at owner-only permissions."
        })
        .color(palette.muted),
    );
    ui.add_space(space::of(2));

    // `password(true)`, and the buffer is a `SecretKey`'s — so there is no `String` here for
    // a future `Debug` to reach.
    let field = ui.add(
        egui::TextEdit::singleline(key.buffer_mut())
            .password(true)
            .desired_width(f32::INFINITY)
            .hint_text("Paste your API key"),
    );
    // Enter submits. Non-destructive, and there is one field — the `Rename` dialog's own
    // idiom: `TextEdit` signals Enter by surrendering focus, which is the only signal it
    // gives, so the edge is read here and nothing re-takes focus afterwards.
    let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

    ui.add_space(space::of(2));
    ui.label(
        egui::RichText::new(
            "It is never written to a board, a transcript, an export or a log line.",
        )
        .color(palette.faint)
        .size(crate::theme::text::LABEL),
    );

    ui.add_space(space::of(4));
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let valid = !key.is_empty();
            let save = ui.add_enabled(
                valid,
                egui::Button::new(egui::RichText::new(confirm).color(palette.on_accent))
                    .fill(palette.accent),
            );
            if (save.clicked() || submitted) && valid {
                answer = Some(Answer::Save);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });

    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(id: &str, label: &str) -> AgentLink {
        AgentLink { id: id.to_owned(), label: label.to_owned() }
    }

    /// The round trip is the whole safety property of the rules editor: a file the user
    /// hand-wrote must survive being opened and saved without losing anything.
    ///
    /// Including the part nobody thought of — a key this build has never heard of. A rules
    /// file may be shared with another tool, or written by a later Velm, and an editor that
    /// silently dropped what it did not recognise would be a data-loss gesture disguised as
    /// a save button.
    #[test]
    fn a_rules_file_survives_a_trip_through_the_editor() {
        let original = "---\ntone: blunt\nlanguage: German\npermissions: reads\n\
                        telepathy: yes\n---\n\nAlways show the diff.";
        let rules = AgentRules { text: original.to_owned(), ..AgentRules::default() };

        let form = RulesForm::from_rules(&rules);
        assert_eq!(form.tone, "blunt");
        assert_eq!(form.language, "German");
        assert_eq!(form.permissions, Some(Permissions::Reads));
        assert_eq!(form.body, "Always show the diff.");
        assert_eq!(form.extra.get("telepathy").map(String::as_str), Some("yes"));

        let written = form.into_rules();
        let back = RuleFile::parse(&written.text);
        assert_eq!(back.front.tone.as_deref(), Some("blunt"));
        assert_eq!(back.front.language.as_deref(), Some("German"));
        assert_eq!(back.front.permissions, Some(Permissions::Reads));
        assert_eq!(back.front.extra.get("telepathy").map(String::as_str), Some("yes"));
        assert_eq!(back.body, "Always show the diff.");

        // …and it is stable, so a second save produces the same bytes as the first and a
        // `git diff` of a rules file shows what changed rather than a reformat.
        let twice = RulesForm::from_rules(&written).into_rules();
        assert_eq!(twice.text, written.text);
    }

    /// Clearing a field must remove the key rather than write an empty one — an empty value
    /// is skipped by the parser anyway, but writing `tone:` into somebody's file is a
    /// reformat they did not ask for. The point is that the field then **inherits** again.
    #[test]
    fn clearing_a_field_returns_it_to_the_layer_above() {
        let rules = AgentRules { text: "---\ntone: terse\n---".to_owned(), ..AgentRules::default() };
        let mut form = RulesForm::from_rules(&rules);
        form.tone = "   ".to_owned();
        let written = form.into_rules();
        assert!(!written.text.contains("tone"), "{}", written.text);

        let global = RuleFile::parse("---\ntone: warm\n---");
        let resolved = vellum_agent::rules::resolve(
            &global,
            &RuleFile::default(),
            &written,
            "Reviewer",
        );
        assert_eq!(resolved.tone.as_ref().map(|s| s.value.as_str()), Some("warm"));
        assert_eq!(resolved.provenance(RuleField::Tone), Layer::Global);
    }

    /// The one way this function can corrupt a file rather than merely reformat it: a body
    /// that opens with `---` would be re-read as front matter on the way back in.
    #[test]
    fn a_body_that_opens_with_a_rule_is_not_mistaken_for_front_matter() {
        let form = RulesForm { body: "---\nnot settings\n".to_owned(), ..RulesForm::default() };
        let written = form.into_rules();
        let back = RuleFile::parse(&written.text);
        assert!(back.front.is_empty(), "the body's own rule became settings");
        assert!(back.body.starts_with("---"), "{}", back.body);
    }

    /// A newline inside a value would close the block early and turn the rest of the
    /// settings into prose. Nothing in the editor can type one today; the flattening is
    /// what makes that a property of the function rather than of the fields above it.
    #[test]
    fn a_value_cannot_close_the_block_it_lives_in() {
        let form = RulesForm { tone: "blunt\n---\nlanguage: Klingon".to_owned(), ..RulesForm::default() };
        let written = form.into_rules();
        let back = RuleFile::parse(&written.text);
        assert_eq!(back.front.language, None, "a value escaped into its own setting");
        assert!(
            back.front.tone.as_deref().is_some_and(|tone| tone.contains("blunt")),
            "{:?}",
            back.front.tone
        );
    }

    /// The editor never authors `overrides` — that vector is a cache of what resolution
    /// decided, and the app writes it back from `override_names()`. A hand-written one is a
    /// second source of truth for provenance, and it is the one the inspector would believe.
    #[test]
    fn the_editor_leaves_the_override_cache_to_resolution() {
        let rules = AgentRules {
            text: "---\ntone: terse\n---".to_owned(),
            overrides: vec!["language".to_owned()],
            ignore_inherited: false,
        };
        let written = RulesForm::from_rules(&rules).into_rules();
        assert!(written.overrides.is_empty(), "{:?}", written.overrides);

        // …and what the cache *should* say is derivable from the resolution, which is where
        // the app is expected to get it.
        let resolved = vellum_agent::rules::resolve(
            &RuleFile::default(),
            &RuleFile::default(),
            &written,
            "Reviewer",
        );
        assert_eq!(resolved.override_names(), vec!["tone".to_owned()]);
    }

    /// Feature 10's refusal, which is the whole reason the target list is passed in. It has
    /// to fire when the schedule is **saved** — at six in the evening the failure lands in a
    /// transcript nobody is watching.
    #[test]
    fn a_hand_off_to_an_unconnected_agent_is_refused_at_save_time() {
        let connected = [link("2@1", "Builder")];

        let reachable = Schedule {
            completion: Completion::HandOff { agent: "2@1".to_owned() },
            ..Schedule::default()
        };
        assert_eq!(schedule_problem(&reachable, &connected), None);

        let stale = Schedule {
            completion: Completion::HandOff { agent: "9@1".to_owned() },
            ..Schedule::default()
        };
        let why = schedule_problem(&stale, &connected).expect("an unreachable target is refused");
        assert!(why.contains("no longer connected"), "{why}");

        // With nothing connected at all the sentence has to say what to *do*, because the
        // user's next question is "how do I connect them".
        let why = schedule_problem(&stale, &[]).expect("nowhere to hand off to");
        assert!(why.contains("connector"), "{why}");

        // Everything that is not a hand-off is always saveable.
        for completion in [Completion::Report, Completion::Nothing] {
            let schedule = Schedule { completion, ..Schedule::default() };
            assert_eq!(schedule_problem(&schedule, &[]), None);
        }
    }

    /// Switching the shape of a recurrence must not lose the hour that was chosen — the
    /// commonest edit here is "make my daily six o'clock a weekly six o'clock".
    #[test]
    fn the_time_of_day_survives_a_change_of_shape() {
        let daily = Recurrence::Daily { seconds_after_midnight: 18 * 3600, utc_offset: 3600 };
        assert_eq!(time_of_day(daily), Some(18 * 3600));

        let weekly =
            Recurrence::Weekly { weekday: 4, seconds_after_midnight: 9 * 3600, utc_offset: 0 };
        assert_eq!(time_of_day(weekly), Some(9 * 3600));

        assert_eq!(time_of_day(Recurrence::Interval { minutes: 30 }), None);
    }

    /// Every form has to survive being drawn, in each of its shapes.
    #[test]
    fn the_editors_draw_in_every_shape() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let targets = [link("2@1", "Builder")];

        for recurrence in [
            Recurrence::Interval { minutes: 15 },
            Recurrence::Daily { seconds_after_midnight: 18 * 3600, utc_offset: 0 },
            Recurrence::Weekly { weekday: 2, seconds_after_midnight: 9 * 3600, utc_offset: 0 },
        ] {
            for completion in [
                Completion::Report,
                Completion::Nothing,
                Completion::HandOff { agent: "2@1".to_owned() },
                Completion::HandOff { agent: "gone".to_owned() },
            ] {
                let mut schedule = Schedule {
                    enabled: true,
                    recurrence,
                    trigger: Trigger::NoteChanged { path: "notes/plan.md".to_owned() },
                    completion,
                    ..Schedule::default()
                };
                let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                    let _ = schedule_editor(
                        ui,
                        Palette::LIGHT,
                        &mut schedule,
                        &targets,
                        3600,
                        "Save",
                    );
                });
            }
        }

        // …and with nothing connected, where the hand-off row has to explain itself.
        let mut schedule = Schedule::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = schedule_editor(ui, Palette::LIGHT, &mut schedule, &[], 0, "Save");
        });

        let resolved = vellum_agent::rules::resolve(
            &RuleFile::parse("---\ntone: warm\n---\nHouse style."),
            &RuleFile::default(),
            &AgentRules::default(),
            "Reviewer",
        );
        let mut form = RulesForm::from_rules(&AgentRules::default());
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = rules_editor(ui, Palette::LIGHT, &mut form, &resolved, "Save");
        });

        let mut key = SecretKey::default();
        for provider in [Provider::Claude, Provider::Kimi] {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let _ = sign_in(ui, Palette::LIGHT, provider, false, &mut key, "Sign in");
            });
        }
    }
}
