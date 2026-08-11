//! The time axis: dates to pixels, pixels back to dates, and the ticks between.
//!
//! One linear map — `x = left + (date - span.start) * day_width` — and everything
//! else in the timeline is expressed through it. Keeping it in one small `Copy` type
//! rather than scattering the arithmetic through the layout is what makes a bar's
//! left edge and the gridline under it provably the same number.
//!
//! A day is a *width*, not a position: [`TimeAxis::x_for`] gives the left edge of a
//! day and [`TimeAxis::x_after`] gives its right edge, so a one-day bar is one day
//! wide instead of being a zero-width line. That is the visual reading of
//! [`DateRange`]'s inclusive end, and the two have to agree or every bar is a day
//! short.

use crate::date::{Date, DateRange};

/// The linear map between dates and x coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeAxis {
    span: DateRange,
    left: f64,
    day_width: f64,
}

/// How coarse the ticks are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TickScale {
    Day,
    /// Weeks beginning on Monday.
    Week,
    Month,
    Quarter,
    Year,
}

/// One mark on the axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    /// The first day of the period this tick begins.
    pub date: Date,
    /// Its left edge, absolute.
    pub x: f64,
    pub scale: TickScale,
}

/// Above this the axis stops emitting ticks.
///
/// A guard, not a design: at any sane zoom the count is the axis width divided by
/// [`TimelineMetrics::min_tick_spacing`](crate::TimelineMetrics::min_tick_spacing),
/// which is in the dozens. It exists so that a corrupt import with a bar spanning
/// ten thousand years produces a coarse axis rather than a million allocations.
const MAX_TICKS: usize = 4096;

impl TickScale {
    /// Coarsest first, which is the order the automatic choice walks in reverse.
    const ALL: [Self; 5] = [Self::Day, Self::Week, Self::Month, Self::Quarter, Self::Year];

    /// Roughly how many days one of these lasts. Only ever used to compare against a
    /// spacing threshold, so the month and quarter approximations are harmless.
    pub const fn approximate_days(self) -> f64 {
        match self {
            Self::Day => 1.0,
            Self::Week => 7.0,
            Self::Month => 30.44,
            Self::Quarter => 91.31,
            Self::Year => 365.25,
        }
    }

    /// The first tick at or after `date`.
    fn first_at_or_after(self, date: Date) -> Date {
        let candidate = match self {
            Self::Day => date,
            Self::Week => date.start_of_week(),
            Self::Month => date.start_of_month(),
            Self::Quarter => date.start_of_quarter(),
            Self::Year => date.start_of_year(),
        };
        if candidate < date { self.next(candidate) } else { candidate }
    }

    /// The tick after this one. `date` is assumed to be on a boundary already.
    fn next(self, date: Date) -> Date {
        match self {
            Self::Day => date.add_days(1),
            Self::Week => date.add_days(7),
            Self::Month => date.start_of_next_month(),
            Self::Quarter => {
                let mut next = date.start_of_next_month();
                for _ in 0..2 {
                    next = next.start_of_next_month();
                }
                next
            }
            Self::Year => Date::from_ymd(date.year() + 1, 1, 1).expect("1 January always exists"),
        }
    }
}

impl Tick {
    /// A default label: `28 Jul`, `Jul 2026`, `Q3 2026`, `2026`.
    ///
    /// English and unlocalised, and produced on demand rather than stored, because
    /// this is a fallback rather than a feature. Formatting a date the way the user's
    /// machine does is the platform's job, and the caller has the [`Date`] to do it
    /// with. `vellum-flow` will not grow a locale database.
    pub fn label(&self) -> String {
        match self.scale {
            TickScale::Day | TickScale::Week => {
                format!("{} {}", self.date.day(), self.date.month_name())
            }
            TickScale::Month => format!("{} {}", self.date.month_name(), self.date.year()),
            TickScale::Quarter => format!("Q{} {}", self.date.quarter(), self.date.year()),
            TickScale::Year => self.date.year().to_string(),
        }
    }
}

impl TimeAxis {
    /// `left` is the x of the first day of `span`. `day_width` is clamped to a
    /// positive value: a zero-width day would make [`TimeAxis::date_at`] divide by
    /// zero and every bar collapse onto one pixel.
    pub fn new(span: DateRange, left: f64, day_width: f64) -> Self {
        let day_width = if day_width.is_finite() && day_width > 0.0 { day_width } else { 1.0 };
        Self { span, left, day_width }
    }

    pub fn span(self) -> DateRange {
        self.span
    }

    pub fn day_width(self) -> f64 {
        self.day_width
    }

    pub fn left(self) -> f64 {
        self.left
    }

    /// The full width of the span.
    pub fn width(self) -> f64 {
        self.span.length() as f64 * self.day_width
    }

    /// The left edge of `date`. Dates outside the span map outside the axis rather
    /// than being clamped — a bar dragged past the end of a fixed axis is off the
    /// end, and pretending otherwise would silently change its dates.
    pub fn x_for(self, date: Date) -> f64 {
        self.left + f64::from(date.days_since(self.span.start())) * self.day_width
    }

    /// The right edge of `date` — the left edge of the day after it.
    pub fn x_after(self, date: Date) -> f64 {
        self.x_for(date.add_days(1))
    }

    /// The left and right edges of a whole range. A range's right edge is past its
    /// last day, so a single-day bar is one day wide.
    pub fn x_for_range(self, range: DateRange) -> (f64, f64) {
        (self.x_for(range.start()), self.x_after(range.end()))
    }

    /// Which day `x` falls in.
    pub fn date_at(self, x: f64) -> Date {
        // Floor, not round: every x inside a day's width belongs to that day, which
        // is what makes hit-testing the axis agree with the bar drawn under it.
        //
        // The nudge is what makes `date_at(x_for(d)) == d` hold *exactly*. A day
        // boundary is computed as `left + n * day_width`, and dividing that back by
        // `day_width` can land a few ulps below `n` — which floors to the day before
        // and puts a tick one day left of the gridline it drew. A billionth of a day
        // is a hundred microseconds; nothing here can express it, so nothing is lost
        // by absorbing it.
        let offset = (x - self.left) / self.day_width + 1e-9;
        self.span.start().add_days(clamp_to_days(offset.floor()))
    }

    /// A pixel distance as a whole number of days — what a horizontal drag means.
    pub fn days_for(self, distance: f64) -> i32 {
        clamp_to_days((distance / self.day_width).round())
    }

    /// The finest scale whose ticks would sit at least `min_spacing` apart.
    pub fn scale_for(self, min_spacing: f64) -> TickScale {
        TickScale::ALL
            .into_iter()
            .find(|scale| scale.approximate_days() * self.day_width >= min_spacing)
            .unwrap_or(TickScale::Year)
    }

    /// Ticks across the span, no closer together than `min_spacing`.
    ///
    /// The first tick is the first period boundary at or after the span's start, so
    /// an axis beginning mid-month starts with the 1st of the next one rather than
    /// with a label nobody can align to.
    pub fn ticks(self, min_spacing: f64) -> Vec<Tick> {
        let scale = self.scale_for(min_spacing);
        let mut ticks = Vec::new();
        let mut date = scale.first_at_or_after(self.span.start());
        while date <= self.span.end() && ticks.len() < MAX_TICKS {
            ticks.push(Tick { date, x: self.x_for(date), scale });
            date = scale.next(date);
        }
        ticks
    }
}

/// Days are `i32`; a pixel offset need not be. Saturates rather than wrapping, so a
/// drag of a billion pixels pins a bar at the end of time instead of teleporting it
/// to the start.
fn clamp_to_days(value: f64) -> i32 {
    if value.is_nan() {
        0
    } else {
        value.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> Date {
        text.parse().unwrap()
    }

    fn axis(start: &str, end: &str, day_width: f64) -> TimeAxis {
        TimeAxis::new(DateRange::new(date(start), date(end)), 100.0, day_width)
    }

    #[test]
    fn a_day_is_a_width_not_a_position() {
        let a = axis("2026-07-01", "2026-07-31", 10.0);
        assert_eq!(a.x_for(date("2026-07-01")), 100.0);
        assert_eq!(a.x_after(date("2026-07-01")), 110.0);
        assert_eq!(a.x_for(date("2026-07-11")), 200.0);

        // A one-day bar is one day wide, not zero.
        let (left, right) = a.x_for_range(DateRange::single(date("2026-07-05")));
        assert_eq!(right - left, 10.0);
        // A ten-day bar is ten days wide, inclusive of its end.
        let (left, right) = a.x_for_range(DateRange::new(date("2026-07-01"), date("2026-07-10")));
        assert_eq!(right - left, 100.0);
        assert_eq!(a.width(), 310.0);
    }

    #[test]
    fn a_point_anywhere_in_a_day_reads_as_that_day() {
        let a = axis("2026-07-01", "2026-07-31", 10.0);
        assert_eq!(a.date_at(100.0), date("2026-07-01"));
        assert_eq!(a.date_at(109.9), date("2026-07-01"));
        assert_eq!(a.date_at(110.0), date("2026-07-02"));
        // Before the axis is a real date, not a clamp.
        assert_eq!(a.date_at(90.0), date("2026-06-30"));
        // And it inverts x_for exactly.
        for day in 0..31 {
            let d = date("2026-07-01").add_days(day);
            assert_eq!(a.date_at(a.x_for(d)), d);
            assert_eq!(a.date_at(a.x_after(d) - 0.001), d);
        }
    }

    #[test]
    fn a_drag_distance_becomes_whole_days() {
        let a = axis("2026-07-01", "2026-07-31", 10.0);
        assert_eq!(a.days_for(0.0), 0);
        assert_eq!(a.days_for(4.0), 0);
        assert_eq!(a.days_for(6.0), 1);
        assert_eq!(a.days_for(-25.0), -3, "away from zero at the half");
        assert_eq!(a.days_for(f64::INFINITY), i32::MAX, "no wrap-around");
    }

    /// The axis coarsens as it zooms out, and never emits labels that would collide.
    #[test]
    fn the_scale_coarsens_as_days_get_narrower() {
        let span = DateRange::new(date("2020-01-01"), date("2030-01-01"));
        let cases = [
            (80.0, TickScale::Day),
            (10.0, TickScale::Week),
            (3.0, TickScale::Month),
            (0.8, TickScale::Quarter),
            (0.1, TickScale::Year),
        ];
        for (day_width, expected) in cases {
            let a = TimeAxis::new(span, 0.0, day_width);
            assert_eq!(a.scale_for(56.0), expected, "at {day_width}px per day");
        }
    }

    #[test]
    fn ticks_land_on_period_boundaries_and_carry_their_x() {
        let a = axis("2026-07-15", "2026-11-05", 3.0);
        let ticks = a.ticks(56.0);
        assert_eq!(ticks[0].scale, TickScale::Month);
        assert_eq!(ticks[0].date.to_string(), "2026-08-01", "the first boundary after the start");
        assert_eq!(ticks[0].x, a.x_for(date("2026-08-01")));
        assert_eq!(
            ticks.iter().map(|t| t.date.to_string()).collect::<Vec<_>>(),
            ["2026-08-01", "2026-09-01", "2026-10-01", "2026-11-01"]
        );
        assert!(ticks.iter().all(|t| a.span().contains(t.date)));
    }

    #[test]
    fn quarter_and_year_ticks_step_correctly_across_a_year_boundary() {
        let a = TimeAxis::new(DateRange::new(date("2025-11-01"), date("2027-03-01")), 0.0, 0.8);
        let ticks = a.ticks(56.0);
        assert_eq!(ticks[0].scale, TickScale::Quarter);
        assert_eq!(
            ticks.iter().map(|t| t.date.to_string()).collect::<Vec<_>>(),
            ["2026-01-01", "2026-04-01", "2026-07-01", "2026-10-01", "2027-01-01"]
        );
        assert_eq!(ticks[0].label(), "Q1 2026");

        let years = TimeAxis::new(DateRange::new(date("2025-11-01"), date("2029-03-01")), 0.0, 0.1);
        assert_eq!(
            years.ticks(56.0).iter().map(|t| t.date.to_string()).collect::<Vec<_>>(),
            ["2026-01-01", "2027-01-01", "2028-01-01", "2029-01-01"]
        );
    }

    #[test]
    fn week_ticks_begin_on_mondays() {
        let a = axis("2026-07-15", "2026-08-15", 10.0);
        let ticks = a.ticks(56.0);
        assert_eq!(ticks[0].scale, TickScale::Week);
        assert!(ticks.iter().all(|t| t.date.weekday() == crate::date::Weekday::Monday));
        assert_eq!(ticks[0].date.to_string(), "2026-07-20");
        assert_eq!(ticks[0].label(), "20 Jul");
    }

    #[test]
    fn labels_say_what_the_scale_means() {
        let day = Tick { date: date("2026-07-28"), x: 0.0, scale: TickScale::Day };
        assert_eq!(day.label(), "28 Jul");
        assert_eq!(Tick { scale: TickScale::Month, ..day }.label(), "Jul 2026");
        assert_eq!(Tick { scale: TickScale::Quarter, ..day }.label(), "Q3 2026");
        assert_eq!(Tick { scale: TickScale::Year, ..day }.label(), "2026");
    }

    #[test]
    fn a_degenerate_axis_does_not_divide_by_zero_or_run_away() {
        let a = TimeAxis::new(DateRange::single(date("2026-07-01")), 0.0, 0.0);
        assert!(a.day_width() > 0.0);
        assert_eq!(a.date_at(0.0), date("2026-07-01"));
        assert_eq!(a.ticks(56.0).len(), 1, "one day, one tick");

        // A span no board could hold still terminates, coarsely.
        let huge = TimeAxis::new(DateRange::new(date("0001-01-01"), date("9999-12-31")), 0.0, 0.001);
        let ticks = huge.ticks(56.0);
        assert_eq!(ticks[0].scale, TickScale::Year);
        assert!(ticks.len() <= MAX_TICKS);
    }
}
