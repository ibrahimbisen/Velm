//! When an agent runs by itself, and what it does afterwards.
//!
//! Feature 10, both halves: the configuration and the local execution are here and now; the
//! part that needs the app to be running when the laptop is closed is a later phase and is
//! marked as such in `docs/07-agent-canvas.md` §10. Nothing about the stored shape changes
//! when that phase lands — this is what it would read.
//!
//! # Time is passed in, never read
//!
//! Every function here takes "now" as an argument. That is what makes a schedule testable:
//! *"does a daily 18:00 job fire at 17:59"* is a question about arithmetic, and a module
//! that called `SystemTime::now()` internally could only be tested by waiting.

use serde::{Deserialize, Serialize};

/// Seconds since the Unix epoch. The one time type this module uses.
///
/// A bare `u64` rather than `SystemTime` because it is serialised into a board file, and
/// because every comparison here is arithmetic on seconds. Local-time questions — *"6 PM in
/// whose evening"* — are resolved by the caller supplying [`Recurrence::Daily`]'s offset,
/// which keeps this module free of a timezone database.
pub type Timestamp = u64;

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// How often a scheduled agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "every")]
pub enum Recurrence {
    /// Every `minutes` minutes from the moment it was armed.
    Interval { minutes: u32 },
    /// Once a day, `seconds_after_midnight` into the user's own day.
    ///
    /// The offset is seconds rather than an hour and a minute so that a half-hour timezone
    /// — India, Newfoundland — and a 6:30 PM schedule are the same arithmetic. `utc_offset`
    /// is the user's offset in seconds, supplied by the caller: this module holds no
    /// timezone database, and a schedule that silently moved when the rules changed would
    /// be worse than one that is explicit.
    Daily { seconds_after_midnight: u32, utc_offset: i32 },
    /// Once a week, on `weekday` (0 = Monday) at the same offset a daily job uses.
    Weekly { weekday: u8, seconds_after_midnight: u32, utc_offset: i32 },
}

impl Recurrence {
    /// The first fire time strictly after `after`.
    ///
    /// Strictly after, which is what stops a job that has just run from running again
    /// immediately: the scheduler asks for the next time *after the one it just served*.
    pub fn next_after(self, after: Timestamp) -> Timestamp {
        match self {
            Self::Interval { minutes } => {
                let step = u64::from(minutes.max(1)) * MINUTE;
                after + step
            }
            Self::Daily { seconds_after_midnight, utc_offset } => {
                next_daily(after, u64::from(seconds_after_midnight), utc_offset, DAY)
            }
            Self::Weekly { weekday, seconds_after_midnight, utc_offset } => {
                let target = next_daily(
                    after,
                    u64::from(seconds_after_midnight),
                    utc_offset,
                    DAY,
                );
                // Walk forward a day at a time to the requested weekday. At most seven
                // steps, and it costs nothing next to being right about leap seconds we
                // deliberately do not model.
                let mut candidate = target;
                for _ in 0..7 {
                    if weekday_of(candidate, utc_offset) == weekday.min(6) {
                        return candidate;
                    }
                    candidate += DAY;
                }
                candidate
            }
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Interval { minutes } if minutes % 60 == 0 && minutes >= 60 => {
                let hours = minutes / 60;
                if hours == 1 { "Every hour".into() } else { format!("Every {hours} hours") }
            }
            Self::Interval { minutes } => format!("Every {minutes} minutes"),
            Self::Daily { seconds_after_midnight, .. } => {
                format!("Every day at {}", clock(seconds_after_midnight))
            }
            Self::Weekly { weekday, seconds_after_midnight, .. } => format!(
                "Every {} at {}",
                weekday_name(weekday),
                clock(seconds_after_midnight)
            ),
        }
    }
}

/// The next occurrence of a local time-of-day, strictly after `after`.
fn next_daily(after: Timestamp, offset_into_day: u64, utc_offset: i32, period: u64) -> Timestamp {
    // Shift into the user's local frame, find the day boundary, add the offset, shift back.
    let local = shift(after, utc_offset);
    let midnight = local - local % period;
    let today = midnight + offset_into_day.min(period - 1);
    let local_next = if today > local { today } else { today + period };
    unshift(local_next, utc_offset)
}

fn shift(t: Timestamp, offset: i32) -> u64 {
    t.saturating_add_signed(i64::from(offset))
}

fn unshift(t: u64, offset: i32) -> Timestamp {
    t.saturating_add_signed(-i64::from(offset))
}

/// 0 = Monday. 1970-01-01 was a Thursday, which is index 3.
fn weekday_of(t: Timestamp, utc_offset: i32) -> u8 {
    let days = shift(t, utc_offset) / DAY;
    ((days + 3) % 7) as u8
}

fn weekday_name(index: u8) -> &'static str {
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]
        [(index as usize).min(6)]
}

fn clock(seconds_after_midnight: u32) -> String {
    let (h, m) = (seconds_after_midnight / 3600, (seconds_after_midnight % 3600) / 60);
    format!("{h:02}:{m:02}")
}

/// What a scheduled agent does when its run finishes.
///
/// Feature 10's three answers exactly. The third is not a placeholder: an agent that checks
/// something hourly and reports only when it matters is the useful shape, and one that
/// announces "nothing to report" twenty-four times a day is the reason people turn
/// scheduling off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "then")]
pub enum Completion {
    /// Put a summary in front of the user — a card on the board and an entry in the
    /// away-mode digest.
    #[default]
    Report,
    /// Hand the result to an agent this one is connected to. The target must be reachable
    /// along a connector; a hand-off with no line is refused when the schedule is saved,
    /// not silently at 6 PM.
    HandOff { agent: String },
    /// Nothing, unless the run itself asked for attention.
    Nothing,
}

impl Completion {
    pub fn label(&self) -> String {
        match self {
            Self::Report => "Report back to me".into(),
            Self::HandOff { agent } => format!("Hand off to {agent}"),
            Self::Nothing => "Do nothing".into(),
        }
    }
}

/// A condition that must hold for a scheduled run to happen at all.
///
/// Checked at fire time, so a schedule that would otherwise wake an agent to discover it has
/// nothing to do simply does not wake it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "when")]
pub enum Trigger {
    /// No condition: run every time.
    #[default]
    Always,
    /// Only if something under the agent's working directory changed since the last run.
    FilesChanged,
    /// Only if a note the agent can see changed since the last run.
    NoteChanged { path: String },
    /// Only if the previous run failed — a retry.
    ///
    /// Reads [`Schedule::last_failed`], which is written by whoever runs the turn. That is
    /// this module's whole involvement: *what counts as a failure* is a question about a
    /// transport and a process, so it is answered where those live. `vellum-app`'s
    /// `AgentRuntime::complete` is that place, and its doc comment enumerates the three ways
    /// a scheduled run can end without a turn ever finishing — all three of which have to
    /// write the flag, or this condition is one that quietly cannot hold.
    ///
    /// ⚠ It was exactly that for as long as it existed: nothing in the workspace ever set
    /// `last_failed` to `true`, so choosing this trigger produced an agent that never ran
    /// again and said nothing about why.
    LastRunFailed,
}

impl Trigger {
    pub fn label(&self) -> String {
        match self {
            Self::Always => "Every time".into(),
            Self::FilesChanged => "Only if files changed".into(),
            Self::NoteChanged { path } => format!("Only if {path} changed"),
            Self::LastRunFailed => "Only if the last run failed".into(),
        }
    }
}

/// A schedule attached to one agent node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    /// Off without deleting the configuration. Turning a schedule off and on again must not
    /// cost the user the prompt they wrote.
    pub enabled: bool,
    pub recurrence: Recurrence,
    #[serde(skip_serializing_if = "is_default")]
    pub trigger: Trigger,
    #[serde(skip_serializing_if = "is_default")]
    pub completion: Completion,
    /// What the agent is asked when it wakes. Empty means "carry on with what you were
    /// doing", which is meaningful for an agent with standing instructions in its rules.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// When it last ran, so a trigger can ask "since when" and the UI can say "last run".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Timestamp>,
    /// Whether that last run failed, for [`Trigger::LastRunFailed`].
    ///
    /// Set by the runtime, never here — see [`Trigger::LastRunFailed`]. Written only when
    /// `true`, so a schedule saved before this field existed round-trips byte for byte.
    #[serde(skip_serializing_if = "is_false")]
    pub last_failed: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            enabled: false,
            recurrence: Recurrence::Daily { seconds_after_midnight: 18 * 3600, utc_offset: 0 },
            trigger: Trigger::Always,
            completion: Completion::Report,
            prompt: String::new(),
            last_run: None,
            last_failed: false,
        }
    }
}

impl Schedule {
    /// When this schedule next wants to run, given the time now.
    ///
    /// `None` when it is disabled — which is what keeps a disabled schedule out of the
    /// scheduler's queue entirely rather than in it and skipped.
    pub fn next_fire(&self, now: Timestamp) -> Option<Timestamp> {
        if !self.enabled {
            return None;
        }
        Some(self.recurrence.next_after(self.last_run.unwrap_or(now).max(now.saturating_sub(1))))
    }

    /// Whether it is due at `now`.
    pub fn is_due(&self, now: Timestamp) -> bool {
        self.enabled && self.next_fire(now).is_some_and(|next| next <= now)
    }

    /// A one-line description for the node and the inspector.
    pub fn summary(&self) -> String {
        if !self.enabled {
            return "Not scheduled".into();
        }
        let mut text = self.recurrence.label();
        if !matches!(self.trigger, Trigger::Always) {
            text.push_str(", ");
            text.push_str(&self.trigger.label().to_lowercase());
        }
        text
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every assertion here supplies its own "now". A module that read the clock itself
    /// could only be tested by waiting, which is why it does not.
    const NOON_UTC: Timestamp = 1_700_000_000 - (1_700_000_000 % DAY) + 12 * HOUR;

    #[test]
    fn an_interval_fires_one_step_after_the_last_run() {
        let every_ten = Recurrence::Interval { minutes: 10 };
        assert_eq!(every_ten.next_after(1000), 1000 + 600);
        // A zero interval would be a busy loop; it is clamped to a minute.
        assert_eq!(Recurrence::Interval { minutes: 0 }.next_after(0), 60);
    }

    /// The case the feature request named: *"every day at 6 PM"*. Asked just before, it is
    /// today; asked just after, it is tomorrow.
    #[test]
    fn a_daily_schedule_fires_today_before_the_hour_and_tomorrow_after_it() {
        let six_pm = Recurrence::Daily { seconds_after_midnight: 18 * HOUR as u32, utc_offset: 0 };
        let midnight = NOON_UTC - 12 * HOUR;

        let just_before = midnight + 18 * HOUR - 60;
        assert_eq!(six_pm.next_after(just_before), midnight + 18 * HOUR);

        let just_after = midnight + 18 * HOUR + 60;
        assert_eq!(six_pm.next_after(just_after), midnight + DAY + 18 * HOUR);

        // Exactly on the hour is *after*, not now — or a job that just ran runs again.
        assert_eq!(six_pm.next_after(midnight + 18 * HOUR), midnight + DAY + 18 * HOUR);
    }

    /// Six in the evening means six in the *user's* evening. A schedule that fired at 18:00
    /// UTC for someone in Istanbul would be a schedule that fires at nine.
    #[test]
    fn a_daily_schedule_respects_the_users_own_evening() {
        let midnight = NOON_UTC - 12 * HOUR;
        let utc = Recurrence::Daily { seconds_after_midnight: 18 * HOUR as u32, utc_offset: 0 };
        let istanbul =
            Recurrence::Daily { seconds_after_midnight: 18 * HOUR as u32, utc_offset: 3 * HOUR as i32 };

        let morning = midnight + 8 * HOUR;
        // Three hours ahead, so their 18:00 is 15:00 UTC — three hours *earlier* in UTC.
        assert_eq!(
            utc.next_after(morning) - istanbul.next_after(morning),
            3 * HOUR,
            "the timezone offset was applied in the wrong direction"
        );
    }

    /// A half-hour timezone is why the offset is seconds rather than an hour field.
    #[test]
    fn a_half_hour_timezone_is_expressible() {
        let midnight = NOON_UTC - 12 * HOUR;
        let india = Recurrence::Daily {
            seconds_after_midnight: 18 * HOUR as u32,
            utc_offset: (5 * HOUR + 30 * MINUTE) as i32,
        };
        let fires = india.next_after(midnight + 2 * HOUR);
        assert_eq!((fires - midnight) % DAY, 12 * HOUR + 30 * MINUTE);
    }

    #[test]
    fn a_weekly_schedule_lands_on_the_requested_day() {
        let weekly =
            Recurrence::Weekly { weekday: 0, seconds_after_midnight: 9 * HOUR as u32, utc_offset: 0 };
        let fires = weekly.next_after(NOON_UTC);
        assert_eq!(weekday_of(fires, 0), 0, "a Monday schedule did not land on a Monday");
        assert!(fires > NOON_UTC);
        assert_eq!((fires % DAY), 9 * HOUR);
    }

    /// A disabled schedule is not in the queue at all, rather than in it and skipped — which
    /// is what keeps the scheduler thread from existing on a board with nothing scheduled.
    #[test]
    fn a_disabled_schedule_never_wants_to_run() {
        let mut schedule = Schedule::default();
        assert!(!schedule.enabled, "schedules must be off until asked for");
        assert_eq!(schedule.next_fire(NOON_UTC), None);
        assert!(!schedule.is_due(NOON_UTC));
        assert_eq!(schedule.summary(), "Not scheduled");

        schedule.enabled = true;
        assert!(schedule.next_fire(NOON_UTC).is_some());
    }

    /// Turning a schedule off must not cost the user the prompt they wrote.
    #[test]
    fn disabling_keeps_the_configuration() {
        let schedule = Schedule {
            enabled: false,
            prompt: "check the build".into(),
            completion: Completion::HandOff { agent: "42@7".into() },
            ..Schedule::default()
        };
        let json = serde_json::to_string(&schedule).unwrap();
        let back: Schedule = serde_json::from_str(&json).unwrap();
        assert_eq!(back.prompt, "check the build");
        assert_eq!(back.completion, Completion::HandOff { agent: "42@7".into() });
    }

    #[test]
    fn the_summary_reads_as_a_sentence() {
        let schedule = Schedule {
            enabled: true,
            recurrence: Recurrence::Daily { seconds_after_midnight: 18 * HOUR as u32, utc_offset: 0 },
            trigger: Trigger::FilesChanged,
            ..Schedule::default()
        };
        assert_eq!(schedule.summary(), "Every day at 18:00, only if files changed");

        assert_eq!(Recurrence::Interval { minutes: 60 }.label(), "Every hour");
        assert_eq!(Recurrence::Interval { minutes: 120 }.label(), "Every 2 hours");
        assert_eq!(Recurrence::Interval { minutes: 15 }.label(), "Every 15 minutes");
    }

    #[test]
    fn a_schedule_round_trips() {
        let schedule = Schedule {
            enabled: true,
            recurrence: Recurrence::Weekly {
                weekday: 4,
                seconds_after_midnight: 17 * HOUR as u32,
                utc_offset: -5 * HOUR as i32,
            },
            trigger: Trigger::NoteChanged { path: "notes/plan.md".into() },
            completion: Completion::Nothing,
            prompt: "summarise the week".into(),
            last_run: Some(NOON_UTC),
            last_failed: true,
        };
        let json = serde_json::to_string(&schedule).unwrap();
        assert_eq!(serde_json::from_str::<Schedule>(&json).unwrap(), schedule);
    }
}
