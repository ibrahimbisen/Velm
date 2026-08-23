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
/// ⚠ **Not `Copy`, because [`Self::Cron`] carries a string.** Everything here takes `&self`
/// as a result; that is the whole cost, and it buys the fourth shape `docs/07-agent-canvas.md`
/// §10 has specified since it was written — *"interval / daily / weekly / cron expression"* —
/// of which only the first three existed. A person who already knows cron should not have to
/// translate *"weekdays at 07:30"* into a shape this enum happens to have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A five-field cron expression — `minute hour day-of-month month day-of-week` — in the
    /// user's own local time, which `utc_offset` converts.
    ///
    /// Deliberately the **classic five fields and no more**: no seconds column, no `@reboot`,
    /// no step-with-range beyond `*/n`. A schedule that fires an AI agent does not need
    /// second resolution, and every extension is another spelling a user has to guess right.
    /// An expression this cannot parse is refused when the schedule is saved, not silently at
    /// six in the evening — see [`Recurrence::parse_cron`].
    Cron { expression: String, utc_offset: i32 },
}

impl Recurrence {
    /// The first fire time strictly after `after`.
    ///
    /// Strictly after, which is what stops a job that has just run from running again
    /// immediately: the scheduler asks for the next time *after the one it just served*.
    pub fn next_after(&self, after: Timestamp) -> Timestamp {
        match *self {
            Self::Interval { minutes } => {
                let step = u64::from(minutes.max(1)) * MINUTE;
                after + step
            }
            Self::Daily { seconds_after_midnight, utc_offset } => {
                next_daily(after, u64::from(seconds_after_midnight), utc_offset, DAY)
            }
            Self::Cron { ref expression, utc_offset } => {
                next_cron(expression, after, utc_offset)
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

    /// Whether an expression is one this understands, for the editor to refuse a save on.
    pub fn parse_cron(expression: &str) -> Option<CronFields> {
        CronFields::parse(expression)
    }

    pub fn label(&self) -> String {
        match *self {
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
            Self::Cron { ref expression, .. } => format!("Cron: {expression}"),
        }
    }
}

/// A parsed five-field cron expression.
///
/// Each field is the **set of values it matches**, expanded at parse time. A `Vec<bool>` per
/// field rather than a matcher to evaluate per candidate: the search below tries at most a
/// year of minutes, and asking "is this minute in the set" has to be a lookup rather than a
/// re-parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFields {
    minute: Vec<bool>,
    hour: Vec<bool>,
    day_of_month: Vec<bool>,
    month: Vec<bool>,
    day_of_week: Vec<bool>,
    /// Whether each **day** field was written as `*`.
    ///
    /// ⚠ Recorded at parse time rather than inferred from the set, and that distinction is a
    /// bug this module already had: the sets are sized `high + 1` so a 1-based field like
    /// day-of-month has a permanently-false index 0, which makes *"every value is set"* false
    /// for `*` — so both day fields read as restricted, the OR rule below applied to every
    /// expression, and `30 7 * * 1-5` fired on Saturday because `*` had "matched" the
    /// day-of-month. Cron itself decides this from the field being a star, and so does this.
    day_of_month_star: bool,
    day_of_week_star: bool,
}

impl CronFields {
    /// `minute hour day-of-month month day-of-week`, each `*`, `n`, `a,b`, `a-b` or `*/n`.
    ///
    /// `None` for anything else, which is what lets the editor refuse a save rather than
    /// accept an expression that would never fire. Day-of-week takes 0 **or** 7 for Sunday,
    /// because both spellings are in every crontab anybody has copied from.
    pub fn parse(expression: &str) -> Option<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, day_of_month, month, day_of_week] = fields.as_slice() else {
            return None;
        };
        Some(Self {
            day_of_month_star: day_of_month.trim() == "*",
            day_of_week_star: day_of_week.trim() == "*",
            minute: field(minute, 0, 59)?,
            hour: field(hour, 0, 23)?,
            day_of_month: field(day_of_month, 1, 31)?,
            month: field(month, 1, 12)?,
            day_of_week: {
                let mut days = field(day_of_week, 0, 7)?;
                // 7 and 0 are both Sunday. Folded here so the match below can index 0..=6.
                if days.get(7).copied().unwrap_or(false) {
                    days[0] = true;
                }
                days.truncate(7);
                days
            },
        })
    }

    /// Whether a local civil time matches.
    ///
    /// ⚠ **Day-of-month and day-of-week are OR, not AND, when both are restricted.** That is
    /// cron's own rule and it surprises everyone who has not been bitten by it — `0 0 1 * 1`
    /// is *the first of the month **and** every Monday*, not *Mondays that fall on the first*.
    /// Implementing the intuitive reading would make every expression copied from a crontab
    /// fire on the wrong days.
    fn matches(&self, civil: Civil) -> bool {
        let dom_restricted = !self.day_of_month_star;
        let dow_restricted = !self.day_of_week_star;
        let dom = self.day_of_month.get(civil.day as usize).copied().unwrap_or(false);
        let dow = self.day_of_week.get(civil.weekday as usize).copied().unwrap_or(false);
        let day = match (dom_restricted, dow_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        };
        day && self.minute.get(civil.minute as usize).copied().unwrap_or(false)
            && self.hour.get(civil.hour as usize).copied().unwrap_or(false)
            && self.month.get(civil.month as usize).copied().unwrap_or(false)
    }
}

/// One field of an expression, expanded to the values it matches.
fn field(text: &str, low: u32, high: u32) -> Option<Vec<bool>> {
    let mut set = vec![false; high as usize + 1];
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok().filter(|n| *n > 0)?),
            None => (part, 1),
        };
        let (from, to) = if range == "*" {
            (low, high)
        } else if let Some((from, to)) = range.split_once('-') {
            (from.parse().ok()?, to.parse().ok()?)
        } else {
            let one: u32 = range.parse().ok()?;
            // `5/15` means "from 5, every 15" — the same shape `*/15` has.
            if step > 1 { (one, high) } else { (one, one) }
        };
        if from < low || to > high || from > to {
            return None;
        }
        let mut at = from;
        while at <= to {
            set[at as usize] = true;
            at += step;
        }
    }
    set.iter().any(|on| *on).then_some(set)
}

/// A civil date and time, in whatever frame the caller shifted into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    /// 0 = Sunday, matching cron rather than this module's Monday-first weekday.
    weekday: u32,
}

/// The next minute strictly after `after` that the expression matches.
///
/// A minute-by-minute walk, capped at one year. Cron's own reference implementations do the
/// same, for the same reason: the alternative is field arithmetic that has to be right about
/// February, and a year of minutes is half a million lookups on a `Vec<bool>` — microseconds,
/// once, when a schedule is armed. The cap is what stops `0 0 30 2 *` — the 30th of February —
/// searching forever; it answers a year out, and the editor refuses to save an expression
/// that never fires.
fn next_cron(expression: &str, after: Timestamp, utc_offset: i32) -> Timestamp {
    let Some(fields) = CronFields::parse(expression) else {
        // Unparseable expressions are refused at the editor. Reaching here means a board file
        // was hand-edited: answer far in the future rather than firing every minute.
        return after.saturating_add(365 * DAY);
    };
    // Start at the next whole minute after `after`, so a job cannot fire twice in one minute.
    let start = shift(after, utc_offset) / MINUTE * MINUTE + MINUTE;
    for step in 0..MINUTES_IN_A_YEAR {
        let local = start + step * MINUTE;
        if fields.matches(civil_of(local)) {
            return unshift(local, utc_offset);
        }
    }
    after.saturating_add(365 * DAY)
}

const MINUTES_IN_A_YEAR: u64 = 366 * 24 * 60;

/// Civil time from a local-frame Unix timestamp, by the days-from-epoch algorithm.
///
/// Written out rather than taken from a crate: this module's whole discipline is that it holds
/// no timezone database and reads no clock, and `chrono` would bring both.
fn civil_of(local: u64) -> Civil {
    let days = (local / DAY) as i64;
    let seconds = local % DAY;
    // 1970-01-01 was a Thursday; cron counts Sunday as 0.
    let weekday = ((days + 4).rem_euclid(7)) as u32;

    // Howard Hinnant's civil_from_days, shifted to a March-based year so leap day lands last.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };

    Civil {
        year,
        month,
        day,
        hour: (seconds / HOUR) as u32,
        minute: ((seconds % HOUR) / MINUTE) as u32,
        weekday,
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

    /// 1970-01-01T00:00:00Z is a Thursday, and every other date here is checked against a
    /// calendar rather than against this module's own arithmetic.
    #[test]
    fn civil_time_is_right_about_leap_years_and_weekdays() {
        // 2024-02-29T12:34:00Z — a leap day, which is the date this algorithm exists to get
        // right and the one a naive month table gets wrong.
        let leap = civil_of(1_709_210_040);
        assert_eq!((leap.year, leap.month, leap.day), (2024, 2, 29));
        assert_eq!((leap.hour, leap.minute), (12, 34));
        assert_eq!(leap.weekday, 4, "2024-02-29 was a Thursday");

        // 2026-01-01T00:00:00Z, a Thursday.
        let new_year = civil_of(1_767_225_600);
        assert_eq!((new_year.year, new_year.month, new_year.day), (2026, 1, 1));
        assert_eq!(new_year.weekday, 4);
    }

    #[test]
    fn a_cron_field_expands_every_form_it_accepts() {
        let every = field("*", 0, 5).expect("a star");
        assert!(every.iter().all(|on| *on));

        let one = field("3", 0, 5).expect("a number");
        assert_eq!(one.iter().filter(|on| **on).count(), 1);
        assert!(one[3]);

        let list = field("1,4", 0, 5).expect("a list");
        assert!(list[1] && list[4] && !list[2]);

        let range = field("2-4", 0, 5).expect("a range");
        assert!(range[2] && range[3] && range[4] && !range[1]);

        let step = field("*/2", 0, 5).expect("a step");
        assert!(step[0] && step[2] && step[4] && !step[1]);

        // Out of range, backwards, and nonsense are refused rather than clamped: an
        // expression that means nothing must fail the editor's save, not fire at a time
        // nobody asked for.
        assert!(field("6", 0, 5).is_none());
        assert!(field("4-2", 0, 5).is_none());
        assert!(field("*/0", 0, 5).is_none());
        assert!(field("many", 0, 5).is_none());
    }

    #[test]
    fn an_expression_needs_exactly_five_fields() {
        assert!(CronFields::parse("30 7 * * 1-5").is_some());
        assert!(CronFields::parse("30 7 * *").is_none(), "four fields");
        assert!(CronFields::parse("0 30 7 * * 1-5").is_none(), "a seconds column");
        assert!(CronFields::parse("").is_none());
        // Sunday is 0 or 7, because both are in every crontab anybody has copied.
        let sunday_seven = CronFields::parse("0 0 * * 7").expect("7 is Sunday");
        let sunday_zero = CronFields::parse("0 0 * * 0").expect("0 is Sunday");
        assert_eq!(sunday_seven, sunday_zero);
    }

    /// The weekday walk, at a real date: 2026-01-01 is a Thursday, so *weekdays at 07:30*
    /// fires that morning, then Friday, then skips to Monday.
    #[test]
    fn a_weekday_expression_skips_the_weekend() {
        let midnight = 1_767_225_600; // 2026-01-01T00:00:00Z, a Thursday
        let recurrence =
            Recurrence::Cron { expression: "30 7 * * 1-5".to_owned(), utc_offset: 0 };

        let thursday = recurrence.next_after(midnight);
        assert_eq!(thursday, midnight + 7 * HOUR + 30 * MINUTE);

        let friday = recurrence.next_after(thursday);
        assert_eq!(friday, thursday + DAY);

        // Saturday and Sunday are skipped: the next one is three days later, not one.
        let monday = recurrence.next_after(friday);
        assert_eq!(monday, friday + 3 * DAY, "the weekend was not skipped");
    }

    /// ⚠ Cron's own rule, and the one everybody gets wrong: with **both** day fields
    /// restricted they are OR-ed, not AND-ed. `0 0 1 * 1` is the first of the month *and*
    /// every Monday.
    #[test]
    fn the_two_day_fields_are_or_when_both_are_restricted() {
        let fields = CronFields::parse("0 0 1 * 1").expect("an expression");
        // 2026-01-01 is a Thursday and the first of the month: matches on day-of-month alone.
        assert!(fields.matches(civil_of(1_767_225_600)));
        // 2026-01-05 is a Monday and not the first: matches on day-of-week alone.
        assert!(fields.matches(civil_of(1_767_225_600 + 4 * DAY)));
        // 2026-01-02 is a Friday and not the first: matches neither.
        assert!(!fields.matches(civil_of(1_767_225_600 + DAY)));
    }

    /// A schedule fires **strictly after** the moment it is asked about, or a job that has
    /// just run at 07:30 would be armed for 07:30 again and run every minute of that minute.
    #[test]
    fn the_next_fire_is_strictly_after_the_one_it_just_served() {
        let recurrence = Recurrence::Cron { expression: "* * * * *".to_owned(), utc_offset: 0 };
        let now = 1_767_225_600;
        let next = recurrence.next_after(now);
        assert_eq!(next, now + MINUTE);
        assert!(recurrence.next_after(next) > next);
    }

    /// An expression that can never match answers far in the future rather than searching
    /// for ever. The 30th of February is the honest example.
    #[test]
    fn an_expression_that_never_matches_terminates() {
        let never = Recurrence::Cron { expression: "0 0 30 2 *".to_owned(), utc_offset: 0 };
        let now = 1_767_225_600;
        assert!(never.next_after(now) >= now + 365 * DAY);
    }

    /// The offset is the user's, exactly as it is for a daily job: *07:30 in whose morning*
    /// is the only question this module answers about time zones.
    #[test]
    fn a_cron_time_is_local_like_every_other_recurrence() {
        let midnight = 1_767_225_600; // 2026-01-01T00:00:00Z
        let utc = Recurrence::Cron { expression: "0 9 * * *".to_owned(), utc_offset: 0 };
        // Two hours east: 09:00 local is 07:00 UTC.
        let east = Recurrence::Cron { expression: "0 9 * * *".to_owned(), utc_offset: 2 * 3600 };
        assert_eq!(utc.next_after(midnight), midnight + 9 * HOUR);
        assert_eq!(east.next_after(midnight), midnight + 7 * HOUR);
    }


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
