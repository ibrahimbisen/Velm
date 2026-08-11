//! Connector styling, decoded from Miro's compact style keys.
//!
//! The reference board carries one connector style, repeated on all 18 connectors:
//!
//! ```json
//! {"lc":3355443,"ls":2,"t":2,"lt":1,"a_start":0,"a_end":9,"VER":2,"jump":0}
//! ```
//!
//! That is a narrow sample, so every mapping below records whether it is *verified*
//! against Miro's own SVG export of the same board — the strongest correctness
//! signal available, because the two formats come out of different Miro code paths —
//! or merely *inferred*. Four of the keys are now verified:
//!
//! | Key | Value | Renders as | Basis |
//! |---|---|---|---|
//! | `lt` | 1 | straight | all 18 connectors export as a single `M … L …` — no curve, no elbow |
//! | `ls` | 2 | solid | none of the 18 carries a `stroke-dasharray` |
//! | `a_start` | 0 | nothing | `LineHeadArrow2` appears 18 times for 18 connectors, i.e. one head each |
//! | `a_end` | 9 | filled triangle | that head is `M-12.727,-7.545 L0,0 L-12.727,7.545 Z` with a solid `fill` |
//!
//! **Colour lives elsewhere.** `lc` and opacity are not modelled here: nothing in
//! this crate's output depends on them, and a connector's colour is a uniform for
//! the whole draw rather than something the geometry has to know. `vellum-import`
//! already carries it on its own `ConnectorStyle`.

use serde::{Deserialize, Serialize};

use crate::geometry::Polyline;

/// How a connector gets from one endpoint to the other — Miro's `lt` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RoutingMode {
    /// A single straight segment.
    #[default]
    Straight,
    /// A cubic bezier that leaves each endpoint along its edge normal.
    Curved,
    /// Axis-aligned segments that route around obstacles.
    Orthogonal,
}

impl RoutingMode {
    /// Decodes Miro's `lt`.
    ///
    /// **Only `1` is verified.** Miro's UI offers exactly three connector shapes, so
    /// the remaining two codes must be in this space, but which is which has not
    /// been observed — the reference board uses `lt: 1` throughout. Unknown codes
    /// fall back to [`RoutingMode::Straight`] because a straight connector between
    /// the correct two anchors still says the correct thing about the diagram,
    /// whereas a wrongly-elbowed one wanders through unrelated widgets.
    pub fn from_miro(lt: i64) -> Self {
        match lt {
            // Verified: the SVG export renders every `lt: 1` connector as `M … L …`.
            1 => Self::Straight,
            // Inferred, not observed.
            2 => Self::Orthogonal,
            3 => Self::Curved,
            _ => Self::Straight,
        }
    }
}

/// Whether the line is drawn solid, dashed or dotted — Miro's `ls` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineStyle {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

impl LineStyle {
    /// Decodes Miro's `ls`.
    ///
    /// **Only `2` is verified** — as solid, since the reference board's connectors
    /// export with no `stroke-dasharray`. The other two codes are inferred; the
    /// board contains no dashed connector to check against.
    pub fn from_miro(ls: i64) -> Self {
        match ls {
            0 => Self::Dotted,
            1 => Self::Dashed,
            2 => Self::Solid,
            _ => Self::Solid,
        }
    }

    /// The dash pattern to draw this style with, or `None` for a solid line.
    ///
    /// Patterns scale with thickness so a 1px and an 8px dashed connector read as
    /// the same style rather than as different ones. The specific ratios are a
    /// convention, not an oracle result: the reference board has no dashed line to
    /// measure.
    pub fn dash_pattern(self, thickness: f64) -> Option<DashPattern> {
        let t = thickness.max(crate::geometry::EPSILON);
        match self {
            Self::Solid => None,
            Self::Dashed => Some(DashPattern { on: t * 4.0, off: t * 3.0 }),
            // A "dot" is a round-capped segment of near-zero length; giving it a
            // small positive length keeps the stroke tessellator's job well-defined.
            Self::Dotted => Some(DashPattern { on: t * 0.75, off: t * 2.0 }),
        }
    }
}

/// An on/off dash cycle, measured in world px of arc length.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DashPattern {
    pub on: f64,
    pub off: f64,
}

impl DashPattern {
    /// Cuts a polyline into its drawn runs.
    ///
    /// Arc length is measured along the flattened path, so a dash cycle stays the
    /// same physical length through corners and around curves. An `on` or `off`
    /// length of zero would loop forever, so both are floored.
    pub fn split(self, line: &Polyline) -> Vec<Polyline> {
        let period = self.on.max(crate::geometry::EPSILON) + self.off.max(0.0);
        let total = line.length();
        if total <= crate::geometry::EPSILON || period <= crate::geometry::EPSILON {
            return Vec::new();
        }

        let mut runs = Vec::new();
        let mut at = 0.0;
        while at < total {
            let piece = line.sub_polyline(at, (at + self.on).min(total));
            if !piece.is_empty() {
                runs.push(piece);
            }
            at += period;
        }
        runs
    }
}

/// A connector terminator.
///
/// Miro encodes these as integer codes in `a_start` / `a_end`, and exactly two of
/// them have been seen. The enum covers the forms Miro's UI offers so that a
/// renderer written against it does not need widening later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Arrowhead {
    #[default]
    None,
    /// An open "V" of two strokes, drawn at the line's own thickness.
    LineArrow,
    /// A solid triangle. Verified as Miro's code `9`.
    FilledTriangle,
    /// The same triangle, outlined rather than filled.
    OpenTriangle,
    Circle,
    FilledCircle,
    Diamond,
    FilledDiamond,
}

impl Arrowhead {
    /// Decodes Miro's `a_start` / `a_end`.
    ///
    /// `0` and `9` are verified against the SVG export. **Every other non-zero code
    /// decodes to [`Arrowhead::FilledTriangle`]**, deliberately: an arrowhead
    /// carries the direction of a relationship, so dropping an unknown one reverses
    /// or erases the meaning of the diagram, while drawing it as the wrong shape
    /// merely looks wrong. Miro's own default is the filled triangle, which makes it
    /// the least surprising stand-in.
    ///
    /// There is no `to_miro`: six of the eight forms have no known code, and an
    /// export mapping would be fabricated rather than recovered.
    pub fn from_miro(code: i64) -> Self {
        match code {
            0 => Self::None,
            _ => Self::FilledTriangle,
        }
    }

    /// Whether the head encloses an area that gets filled, as opposed to being
    /// drawn as strokes.
    pub fn is_filled(self) -> bool {
        matches!(self, Self::FilledTriangle | Self::FilledCircle | Self::FilledDiamond)
    }
}

/// Everything about a connector's appearance that changes its *geometry*.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConnectorStyle {
    pub routing: RoutingMode,
    pub line: LineStyle,
    /// Miro's `t`, in world px. The reference board uses `2` throughout.
    pub thickness: f64,
    pub start_arrow: Arrowhead,
    pub end_arrow: Arrowhead,
    /// Miro's `jump`: whether this connector hops over the ones it crosses.
    pub jump_overs: bool,
}

impl Default for ConnectorStyle {
    fn default() -> Self {
        Self {
            routing: RoutingMode::Straight,
            line: LineStyle::Solid,
            thickness: 2.0,
            start_arrow: Arrowhead::None,
            end_arrow: Arrowhead::None,
            jump_overs: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;

    /// The one style the reference board actually contains, decoded end to end.
    #[test]
    fn the_reference_boards_connector_style_decodes_as_the_svg_renders_it() {
        assert_eq!(RoutingMode::from_miro(1), RoutingMode::Straight);
        assert_eq!(LineStyle::from_miro(2), LineStyle::Solid);
        assert_eq!(LineStyle::from_miro(2).dash_pattern(2.0), None);
        assert_eq!(Arrowhead::from_miro(0), Arrowhead::None);
        assert_eq!(Arrowhead::from_miro(9), Arrowhead::FilledTriangle);
    }

    #[test]
    fn an_unknown_arrowhead_code_still_draws_an_arrow() {
        assert_eq!(Arrowhead::from_miro(4), Arrowhead::FilledTriangle);
        assert_eq!(Arrowhead::from_miro(-3), Arrowhead::FilledTriangle);
        assert!(Arrowhead::from_miro(4).is_filled());
    }

    #[test]
    fn an_unknown_routing_code_falls_back_to_straight() {
        assert_eq!(RoutingMode::from_miro(99), RoutingMode::Straight);
        assert_eq!(RoutingMode::from_miro(-1), RoutingMode::Straight);
    }

    #[test]
    fn dash_lengths_scale_with_thickness() {
        let thin = LineStyle::Dashed.dash_pattern(1.0).unwrap();
        let thick = LineStyle::Dashed.dash_pattern(8.0).unwrap();
        assert!((thick.on / thin.on - 8.0).abs() < 1e-9);
        assert!((thick.off / thin.off - 8.0).abs() < 1e-9);
    }

    #[test]
    fn dashing_covers_the_line_in_alternating_runs() {
        let line = Polyline::new([Point::new(0.0, 0.0), Point::new(100.0, 0.0)]);
        let runs = DashPattern { on: 10.0, off: 10.0 }.split(&line);
        assert_eq!(runs.len(), 5);
        assert!((runs.iter().map(Polyline::length).sum::<f64>() - 50.0).abs() < 1e-9);
        assert_eq!(runs[0].points.first(), Some(&Point::new(0.0, 0.0)));
        assert_eq!(runs[0].points.last(), Some(&Point::new(10.0, 0.0)));
        assert_eq!(runs[1].points.first(), Some(&Point::new(20.0, 0.0)));
    }

    #[test]
    fn a_final_dash_is_clipped_rather_than_overhanging() {
        // 25px of line, 20px period: a full dash at 0–10 and a stub at 20–25.
        let line = Polyline::new([Point::new(0.0, 0.0), Point::new(25.0, 0.0)]);
        let runs = DashPattern { on: 10.0, off: 10.0 }.split(&line);
        assert_eq!(runs.len(), 2);
        assert!((runs[1].length() - 5.0).abs() < 1e-9);
        assert_eq!(runs.last().unwrap().points.last(), Some(&Point::new(25.0, 0.0)));
    }

    #[test]
    fn dashing_a_zero_length_line_terminates() {
        let line = Polyline::new([Point::new(5.0, 5.0), Point::new(5.0, 5.0)]);
        assert!(DashPattern { on: 10.0, off: 10.0 }.split(&line).is_empty());
        assert!(DashPattern { on: 0.0, off: 0.0 }.split(&line).is_empty());
    }

    #[test]
    fn dashing_follows_a_corner() {
        let line = Polyline::new([
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
        ]);
        let runs = DashPattern { on: 15.0, off: 5.0 }.split(&line);
        // The first run turns the corner rather than cutting it.
        assert_eq!(runs[0].points.len(), 3);
        assert!((runs[0].length() - 15.0).abs() < 1e-9);
    }
}
