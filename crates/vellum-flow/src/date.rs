//! Calendar dates, at day resolution, with no dependency and no clock.
//!
//! A roadmap bar spans days, not instants. Modelling it with a timestamp would drag
//! in time zones, and a bar that starts "1 July" would move a day when the board is
//! opened in another zone — a class of bug this crate can simply not have. So a
//! [`Date`] is a count of days from 1970-01-01, converted to and from a civil
//! `(year, month, day)` by Howard Hinnant's exact algorithms, which are valid across
//! the whole proleptic Gregorian calendar and involve no lookup tables.
//!
//! There is deliberately **no "today"** here. Everything in this crate is a pure
//! function of its inputs so that a timeline lays out identically in a test, in CI
//! and on a machine whose clock is wrong; the app supplies the current date when it
//! wants to draw a today-marker.
//!
//! # The end of a range is inclusive
//!
//! [`DateRange`]'s `end` is the last day of the range, not the first day after it.
//! A task "1st to the 10th" runs through the 10th, which is what a person means and
//! what Miro shows. Geometry needs the other convention — the bar's right edge is at
//! the *end* of the 10th — so [`DateRange::end_exclusive`] exists and is the only
//! place the `+1` is written. Getting this wrong shows up as a bar one day short,
//! which is a pixel or two on a year-long axis and therefore easy to ship.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Days from the civil epoch, 1970-01-01.
///
/// `i32` covers ±5.8 million years, which is enough for a roadmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Date(i32);

/// Which day of the week a date falls on. Monday first, matching how a week is drawn
/// on a roadmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    /// Monday is 0.
    pub const fn index(self) -> i32 {
        self as i32
    }

    const ALL: [Self; 7] = [
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    pub const fn is_weekend(self) -> bool {
        matches!(self, Self::Saturday | Self::Sunday)
    }
}

/// Three-letter month names, for the axis labels this crate produces by default.
/// English and unlocalised on purpose — see [`crate::timeline::Tick::label`].
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

impl Date {
    /// 1970-01-01, the zero of the day count.
    pub const EPOCH: Self = Self(0);

    /// A calendar date, or `None` if there is no such day — 31 April and 29 February
    /// in a common year are both rejected rather than rolled forward, because a
    /// rolled date is a wrong date that never gets noticed.
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return None;
        }
        Some(Self(days_from_civil(year, month, day)))
    }

    pub const fn from_days(days: i32) -> Self {
        Self(days)
    }

    pub const fn days(self) -> i32 {
        self.0
    }

    pub fn ymd(self) -> (i32, u32, u32) {
        civil_from_days(self.0)
    }

    pub fn year(self) -> i32 {
        self.ymd().0
    }

    pub fn month(self) -> u32 {
        self.ymd().1
    }

    pub fn day(self) -> u32 {
        self.ymd().2
    }

    /// Saturating, so arithmetic near the ends of the range cannot wrap a bar to the
    /// other end of history.
    pub fn add_days(self, days: i32) -> Self {
        Self(self.0.saturating_add(days))
    }

    /// Signed distance in days, `self - other`.
    pub fn days_since(self, other: Self) -> i32 {
        self.0.saturating_sub(other.0)
    }

    pub fn weekday(self) -> Weekday {
        // 1970-01-01 was a Thursday, which is index 3 with Monday first.
        Weekday::ALL[(self.0.rem_euclid(7) as usize + 3) % 7]
    }

    /// The Monday of this date's week, or the date itself if it is a Monday.
    pub fn start_of_week(self) -> Self {
        self.add_days(-self.weekday().index())
    }

    pub fn start_of_month(self) -> Self {
        let (y, m, _) = self.ymd();
        Self(days_from_civil(y, m, 1))
    }

    /// The first day of the next month. Used to walk month boundaries without
    /// needing to know how long each month is.
    pub fn start_of_next_month(self) -> Self {
        let (y, m, _) = self.ymd();
        if m == 12 { Self(days_from_civil(y + 1, 1, 1)) } else { Self(days_from_civil(y, m + 1, 1)) }
    }

    /// The first day of the calendar quarter — 1 January, April, July or October.
    pub fn start_of_quarter(self) -> Self {
        let (y, m, _) = self.ymd();
        Self(days_from_civil(y, m - (m - 1) % 3, 1))
    }

    pub fn start_of_year(self) -> Self {
        Self(days_from_civil(self.ymd().0, 1, 1))
    }

    /// 1-based, so Q1 is January to March.
    pub fn quarter(self) -> u32 {
        (self.month() - 1) / 3 + 1
    }

    /// `Jul`, for axis labels.
    pub fn month_name(self) -> &'static str {
        MONTHS[(self.month() - 1) as usize]
    }
}

/// ISO 8601, `YYYY-MM-DD`. Years before 1 and after 9999 print with however many
/// digits they need rather than being truncated to four.
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        if (0..=9999).contains(&y) {
            write!(f, "{y:04}-{m:02}-{d:02}")
        } else {
            write!(f, "{y}-{m:02}-{d:02}")
        }
    }
}

/// A string that is not an ISO date this crate can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateError(pub String);

impl fmt::Display for DateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a YYYY-MM-DD date", self.0)
    }
}

impl std::error::Error for DateError {}

/// Accepts `YYYY-MM-DD` and nothing else. Deliberately strict: this parses dates
/// written by us and by an importer, not free text typed by a user, and a lenient
/// parser here would turn a malformed import into a plausible wrong date.
impl FromStr for Date {
    type Err = DateError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let err = || DateError(text.to_owned());
        let (year, rest) = text.split_once('-').ok_or_else(err)?;
        let (month, day) = rest.split_once('-').ok_or_else(err)?;
        if month.len() != 2 || day.len() != 2 {
            return Err(err());
        }
        let year: i32 = year.parse().map_err(|_| err())?;
        let month: u32 = month.parse().map_err(|_| err())?;
        let day: u32 = day.parse().map_err(|_| err())?;
        Self::from_ymd(year, month, day).ok_or_else(err)
    }
}

/// A span of whole days, **both ends inclusive**. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRange {
    start: Date,
    end: Date,
}

impl DateRange {
    /// A range from `start` to `end`. An `end` before `start` collapses to a
    /// single-day range rather than inverting — a backwards drag of a bar's left
    /// handle produces exactly that, every time, and a negative-length bar would
    /// reach the lane packer as an interval that ends before it begins.
    pub fn new(start: Date, end: Date) -> Self {
        Self { start, end: if end < start { start } else { end } }
    }

    /// A range of `days` days beginning at `start`. Zero or negative lengths give a
    /// single day, for the same reason.
    pub fn days_from(start: Date, days: i32) -> Self {
        Self::new(start, start.add_days(days.max(1) - 1))
    }

    pub fn single(day: Date) -> Self {
        Self { start: day, end: day }
    }

    pub const fn start(self) -> Date {
        self.start
    }

    /// The last day *in* the range.
    pub const fn end(self) -> Date {
        self.end
    }

    /// The first day *after* the range — where a bar's right edge is drawn, and the
    /// boundary the lane packer compares against.
    pub fn end_exclusive(self) -> Date {
        self.end.add_days(1)
    }

    /// Length in days; never less than 1.
    pub fn length(self) -> i32 {
        self.end.days_since(self.start).saturating_add(1)
    }

    pub fn contains(self, day: Date) -> bool {
        self.start <= day && day <= self.end
    }

    /// True when the two share at least one day. Touching ranges — one ending the
    /// day the other starts — do **not** overlap; that is the case a roadmap has
    /// constantly, and treating it as an overlap would double the lanes on a board
    /// of back-to-back tasks.
    pub fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// The smallest range containing both.
    pub fn union(self, other: Self) -> Self {
        Self { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    /// Slides the whole range, keeping its length.
    pub fn shifted(self, days: i32) -> Self {
        Self { start: self.start.add_days(days), end: self.end.add_days(days) }
    }

    /// Moves the start, keeping the end put — dragging the left handle of a bar.
    pub fn with_start(self, start: Date) -> Self {
        Self::new(start, self.end)
    }

    /// Moves the end, keeping the start put — dragging the right handle.
    pub fn with_end(self, end: Date) -> Self {
        Self::new(self.start, end)
    }

    /// Grows the range by `days` at each end, for an axis that needs breathing room
    /// around its content.
    pub fn padded(self, days: i32) -> Self {
        Self { start: self.start.add_days(-days), end: self.end.add_days(days) }
    }
}

impl fmt::Display for DateRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..={}", self.start, self.end)
    }
}

pub fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to `y-m-d`, from Howard Hinnant's `days_from_civil`.
///
/// The shift by March makes the leap day the last day of the year, which is what
/// removes every special case: from there the length of a 400-year era is a
/// constant 146,097 days and the day-of-year is one linear expression.
fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    (era * 146_097 + doe - 719_468) as i32
}

/// The exact inverse of [`days_from_civil`].
fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = i64::from(days) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    ((y + i64::from(m <= 2)) as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> Date {
        text.parse().expect("test date should be valid")
    }

    #[test]
    fn the_epoch_is_where_it_should_be_and_prints_as_iso() {
        assert_eq!(Date::EPOCH.ymd(), (1970, 1, 1));
        assert_eq!(Date::EPOCH.to_string(), "1970-01-01");
        assert_eq!(Date::EPOCH.weekday(), Weekday::Thursday);
        assert_eq!(date("2026-07-28").weekday(), Weekday::Tuesday);
    }

    /// The conversion pair is the only arithmetic in this file that could be subtly
    /// wrong, so it is checked exhaustively across two centuries — every day from
    /// 1900 to 2100, in both directions.
    #[test]
    fn civil_conversion_round_trips_every_day_across_two_centuries() {
        let mut expected = Date::from_ymd(1900, 1, 1).unwrap().days();
        for year in 1900..2100 {
            for month in 1..=12 {
                for day in 1..=days_in_month(year, month) {
                    let d = Date::from_ymd(year, month, day).unwrap();
                    assert_eq!(d.days(), expected, "{year}-{month}-{day}");
                    assert_eq!(d.ymd(), (year, month, day));
                    expected += 1;
                }
            }
        }
    }

    #[test]
    fn leap_days_exist_only_in_leap_years() {
        assert!(Date::from_ymd(2024, 2, 29).is_some());
        assert!(Date::from_ymd(2026, 2, 29).is_none());
        assert!(Date::from_ymd(2000, 2, 29).is_some(), "divisible by 400");
        assert!(Date::from_ymd(1900, 2, 29).is_none(), "divisible by 100 but not 400");
        assert!(Date::from_ymd(2026, 4, 31).is_none());
        assert!(Date::from_ymd(2026, 13, 1).is_none());
    }

    #[test]
    fn dates_before_the_epoch_work_the_same_way() {
        let d = date("1815-06-18");
        assert!(d.days() < 0);
        assert_eq!(d.ymd(), (1815, 6, 18));
        assert_eq!(d.add_days(1).ymd(), (1815, 6, 19));
    }

    #[test]
    fn period_starts_land_on_the_boundaries_they_name() {
        let d = date("2026-07-28");
        assert_eq!(d.start_of_week().to_string(), "2026-07-27");
        assert_eq!(d.start_of_month().to_string(), "2026-07-01");
        assert_eq!(d.start_of_quarter().to_string(), "2026-07-01");
        assert_eq!(d.start_of_year().to_string(), "2026-01-01");
        assert_eq!(d.quarter(), 3);
        assert_eq!(date("2026-12-15").start_of_next_month().to_string(), "2027-01-01");
        assert_eq!(date("2026-02-10").start_of_quarter().to_string(), "2026-01-01");
        assert_eq!(d.start_of_week().weekday(), Weekday::Monday);
    }

    /// The inclusive-end rule, stated as the two facts that depend on it.
    #[test]
    fn a_range_includes_its_end_day() {
        let r = DateRange::new(date("2026-07-01"), date("2026-07-10"));
        assert_eq!(r.length(), 10);
        assert!(r.contains(date("2026-07-10")));
        assert_eq!(r.end_exclusive().to_string(), "2026-07-11");
        assert_eq!(DateRange::days_from(date("2026-07-01"), 10), r);
    }

    /// Back-to-back tasks are the common case on a roadmap; if these counted as
    /// overlapping, every sequential plan would need one lane per task.
    #[test]
    fn touching_ranges_do_not_overlap_but_shared_days_do() {
        let first = DateRange::new(date("2026-07-01"), date("2026-07-10"));
        let next = DateRange::new(date("2026-07-11"), date("2026-07-20"));
        assert!(!first.overlaps(next));
        assert!(!next.overlaps(first));

        let straddling = DateRange::new(date("2026-07-10"), date("2026-07-12"));
        assert!(first.overlaps(straddling), "they share the 10th");
        assert!(straddling.overlaps(next));
    }

    #[test]
    fn an_inverted_range_collapses_to_a_day_rather_than_going_negative() {
        let backwards = DateRange::new(date("2026-07-10"), date("2026-07-01"));
        assert_eq!(backwards.length(), 1);
        assert_eq!(backwards.start(), backwards.end());
        // Dragging a bar's left handle past its right one is exactly this.
        let bar = DateRange::new(date("2026-07-01"), date("2026-07-05"));
        assert_eq!(bar.with_start(date("2026-07-20")).length(), 1);
        assert_eq!(DateRange::days_from(date("2026-07-01"), 0).length(), 1);
    }

    #[test]
    fn shifting_keeps_the_length_and_union_covers_both() {
        let r = DateRange::new(date("2026-07-01"), date("2026-07-10"));
        let moved = r.shifted(5);
        assert_eq!(moved.length(), r.length());
        assert_eq!(moved.start().to_string(), "2026-07-06");
        let other = DateRange::new(date("2026-06-20"), date("2026-06-25"));
        let both = r.union(other);
        assert_eq!(both.start(), other.start());
        assert_eq!(both.end(), r.end());
    }

    #[test]
    fn malformed_dates_are_refused_with_the_input_in_the_message() {
        for bad in ["", "2026", "2026-7-28", "2026-07-32", "yesterday", "2026/07/28"] {
            let err = bad.parse::<Date>().unwrap_err();
            assert!(err.to_string().contains(bad), "{err}");
        }
    }
}
