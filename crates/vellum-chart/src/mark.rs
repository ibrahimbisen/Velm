//! The marks themselves — the only ink in a chart that is data.
//!
//! One `enum` rather than five parallel lists, because a renderer wants a single
//! ordered draw list: an area's wash goes under its line, a line goes under its
//! dots, and the order marks appear in is the order they are drawn in. Keeping them
//! in separate collections would push that ordering onto every caller.
//!
//! Colour is resolved into each mark. The alternative — a series index the renderer
//! looks up — would leave the palette's capacity rule (see [`crate::palette`]) to be
//! re-implemented downstream, and it is exactly the kind of rule that gets
//! re-implemented differently.

use crate::colour::Colour;
use crate::geom::{Arc, Point, Polyline, Rect};
use serde::{Deserialize, Serialize};

/// Which end of a bar carries the 4px round.
///
/// A bar is rounded at the end that shows the value and square at the end it grows
/// from. That asymmetry is doing work: the square end says *this is where zero is*,
/// and a bar rounded at both ends floats free of its own baseline. Interior segments
/// of a stack are [`BarEnd::None`] — they have no free end at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BarEnd {
    None,
    Top,
    Bottom,
    Left,
    Right,
}

/// One rectangle: a bar, a column, or one segment of a stack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub series: usize,
    pub index: usize,
    pub value: f64,
    pub rect: Rect,
    /// Zero when the bar is too short for a round to read as anything but a smudge.
    pub radius: f32,
    pub rounded_end: BarEnd,
    pub colour: Colour,
}

/// A line series as a polyline. One [`Line`] per *unbroken run* of a series: a gap in
/// the data ends one and starts the next, so a missing month leaves a hole instead
/// of a straight line drawn through values nobody measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub series: usize,
    pub path: Polyline,
    pub width: f32,
    pub colour: Colour,
}

/// The wash under a line: the same run, closed back along the baseline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Area {
    pub series: usize,
    /// A closed ring, ready to tessellate: out along the values, back along the
    /// baseline or the series below it.
    pub outline: Polyline,
    /// The series colour at ~10% — a wash, never a saturated block. Identity still
    /// comes from the line drawn on top of it.
    pub fill: Colour,
}

/// A scatter point, or a marker on a line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dot {
    pub series: usize,
    pub index: usize,
    pub centre: Point,
    pub radius: f32,
    pub colour: Colour,
    /// A ring of *surface* colour, so overlapping dots stay countable and a dot on
    /// its own line stays visible. Part of the mark's hit target, not just spacing.
    pub ring_width: f32,
    pub ring_colour: Colour,
}

/// One slice of a pie or donut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slice {
    pub series: usize,
    pub index: usize,
    pub value: f64,
    /// The slice's share of the total, `0..=1` — what a percentage label prints, and
    /// the only number a pie actually communicates.
    pub share: f64,
    pub arc: Arc,
    pub colour: Colour,
}

/// Anything a chart draws as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mark {
    Bar(Bar),
    Line(Line),
    Area(Area),
    Dot(Dot),
    Slice(Slice),
}

impl Mark {
    /// The series a mark belongs to — for hit-testing, hover and highlighting.
    pub fn series(&self) -> usize {
        match self {
            Self::Bar(bar) => bar.series,
            Self::Line(line) => line.series,
            Self::Area(area) => area.series,
            Self::Dot(dot) => dot.series,
            Self::Slice(slice) => slice.series,
        }
    }

    pub fn colour(&self) -> Colour {
        match self {
            Self::Bar(bar) => bar.colour,
            Self::Line(line) => line.colour,
            Self::Area(area) => area.fill,
            Self::Dot(dot) => dot.colour,
            Self::Slice(slice) => slice.colour,
        }
    }

    /// Every coordinate is finite. Asserted across whole chart geometries in the
    /// test suite: a single `NaN` vertex does not draw and does not complain, which
    /// makes it the most expensive kind of bug to find later.
    pub fn is_finite(&self) -> bool {
        match self {
            Self::Bar(bar) => bar.rect.is_finite() && bar.radius.is_finite(),
            Self::Line(line) => line.path.is_finite(),
            Self::Area(area) => area.outline.is_finite(),
            Self::Dot(dot) => dot.centre.is_finite() && dot.radius.is_finite(),
            Self::Slice(slice) => slice.arc.is_finite(),
        }
    }
}

/// Caps a bar's corner radius so the round never exceeds half the bar's short side.
///
/// A 4px radius on a 3px-tall bar turns the bar into a lozenge and misreads its
/// length. Shrinking the radius keeps the value honest; the corner simply stops
/// being visible, which is the correct outcome at that size.
pub fn bar_radius(rect: &Rect, radius: f32) -> f32 {
    radius.min(rect.width * 0.5).min(rect.height * 0.5).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_radius_never_exceeds_half_the_bar() {
        assert_eq!(bar_radius(&Rect::new(0.0, 0.0, 20.0, 100.0), 4.0), 4.0);
        assert_eq!(bar_radius(&Rect::new(0.0, 0.0, 20.0, 3.0), 4.0), 1.5);
        assert_eq!(bar_radius(&Rect::new(0.0, 0.0, 0.0, 0.0), 4.0), 0.0);
    }

    #[test]
    fn a_mark_reports_the_series_it_belongs_to() {
        let bar = Mark::Bar(Bar {
            series: 2,
            index: 1,
            value: 5.0,
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            radius: 4.0,
            rounded_end: BarEnd::Top,
            colour: Colour::hex(0xE65B58),
        });
        assert_eq!(bar.series(), 2);
        assert_eq!(bar.colour(), Colour::hex(0xE65B58));
        assert!(bar.is_finite());
    }

    #[test]
    fn a_nan_coordinate_is_reported_rather_than_drawn() {
        let dot = Mark::Dot(Dot {
            series: 0,
            index: 0,
            centre: Point::new(f32::NAN, 0.0),
            radius: 4.0,
            colour: Colour::hex(0x000000),
            ring_width: 2.0,
            ring_colour: Colour::hex(0xFFFFFF),
        });
        assert!(!dot.is_finite());
    }
}
