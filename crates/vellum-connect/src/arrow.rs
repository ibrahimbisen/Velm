//! Arrowhead geometry, oriented along the path's tangent.
//!
//! ## The proportions are measured, not invented
//!
//! Miro's SVG export of the reference board defines its one arrowhead as
//!
//! ```svg
//! <path id="LineHeadArrow2" d="M-12.727,-7.545 L0,0 L-12.727,7.545 Z"/>
//! ```
//!
//! and places it with `<use … transform="translate(tip) rotate(θ)" fill="#333333"/>`,
//! where θ is the connector's own angle. So: **the tip sits exactly on the endpoint,
//! the head points along the direction of travel, and it is filled.** The connectors
//! it terminates are drawn `stroke-width="2"`, which is the `t: 2` in their style —
//! hence the ratios below.
//!
//! The same export also shows Miro **stopping the line short of the tip**: a
//! connector spanning 368.15px is drawn `M 0 0 L 357.968 0`, i.e. 10.182px short,
//! which is 0.8 of the head's length. Without that the stroke pokes out of the
//! arrow's base and thickens its silhouette. [`ArrowheadGeometry::trim`] carries it.
//!
//! Only `t: 2` has ever been observed, so *scaling* the head with thickness is an
//! assumption: the observed numbers are equally consistent with a fixed size or with
//! a constant plus a multiple. Scaling is the assumption that keeps a 1px and an 8px
//! connector looking like the same connector at different weights.

use crate::geometry::{Point, Rect, Vec2};
use crate::style::Arrowhead;

/// Head length in multiples of line thickness — `12.727 / 2` from the SVG export.
const LENGTH_PER_THICKNESS: f64 = 6.363_636_363_636_364;
/// Half the head's width in multiples of line thickness — `7.545 / 2`.
const HALF_WIDTH_PER_THICKNESS: f64 = 3.772_727_272_727_272_5;
/// How far into a filled head the line stops, as a fraction of the head's length.
/// `10.182 / 12.727` in the export.
const FILLED_TRIM_FRACTION: f64 = 0.8;
/// Segments in a circular head. 24 keeps the chord error under 1% of the radius,
/// which at these sizes is a fraction of a device pixel even at deep zoom.
const CIRCLE_SEGMENTS: usize = 24;

/// How an arrowhead's vertices are meant to be realised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowDraw {
    /// A closed polygon, filled.
    Filled,
    /// A closed polygon, stroked at the line's thickness.
    ClosedStroke,
    /// An open polyline, stroked at the line's thickness.
    OpenStroke,
}

/// One arrowhead, in world space.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrowheadGeometry {
    pub vertices: Vec<Point>,
    pub draw: ArrowDraw,
    /// How much arc length the connector's line should give up at this end so it
    /// does not show through or beyond the head.
    pub trim: f64,
}

impl ArrowheadGeometry {
    pub fn bounds(&self) -> Option<Rect> {
        Rect::from_points(self.vertices.iter().copied())
    }
}

/// Builds an arrowhead with its tip at `tip`, pointing along `direction`.
///
/// `direction` points *away* from the line — at the end of a connector that is the
/// path's tangent, and at the start it is the tangent reversed. Returns `None` for
/// [`Arrowhead::None`], for a non-positive thickness, and for a direction with no
/// length, which is the zero-length-connector case: an arrowhead with no direction
/// has no correct orientation, and picking one arbitrarily would point it at random.
pub fn arrowhead(
    kind: Arrowhead,
    tip: Point,
    direction: Vec2,
    thickness: f64,
) -> Option<ArrowheadGeometry> {
    if kind == Arrowhead::None || thickness <= 0.0 {
        return None;
    }
    let forward = direction.normalized()?;
    let left = forward.left_normal();

    let length = thickness * LENGTH_PER_THICKNESS;
    let half_width = thickness * HALF_WIDTH_PER_THICKNESS;
    let base = tip - forward.scaled(length);
    let wing = left.scaled(half_width);

    Some(match kind {
        Arrowhead::None => unreachable!("filtered above"),

        // An open "V": the line runs all the way to the tip, because there is
        // nothing for it to show through.
        Arrowhead::LineArrow => ArrowheadGeometry {
            vertices: vec![base + wing, tip, base - wing],
            draw: ArrowDraw::OpenStroke,
            trim: 0.0,
        },

        // Verified against `LineHeadArrow2`.
        Arrowhead::FilledTriangle => ArrowheadGeometry {
            vertices: vec![tip, base + wing, base - wing],
            draw: ArrowDraw::Filled,
            trim: length * FILLED_TRIM_FRACTION,
        },

        // Hollow: the line meets the base rather than tucking under it, otherwise it
        // stops in mid-air inside the outline.
        Arrowhead::OpenTriangle => ArrowheadGeometry {
            vertices: vec![tip, base + wing, base - wing],
            draw: ArrowDraw::ClosedStroke,
            trim: length,
        },

        Arrowhead::Circle | Arrowhead::FilledCircle => {
            let radius = half_width;
            let center = tip - forward.scaled(radius);
            let filled = kind == Arrowhead::FilledCircle;
            ArrowheadGeometry {
                vertices: circle(center, radius),
                draw: if filled { ArrowDraw::Filled } else { ArrowDraw::ClosedStroke },
                trim: if filled { radius * 2.0 * FILLED_TRIM_FRACTION } else { radius * 2.0 },
            }
        }

        Arrowhead::Diamond | Arrowhead::FilledDiamond => {
            let waist = tip - forward.scaled(length / 2.0);
            let filled = kind == Arrowhead::FilledDiamond;
            ArrowheadGeometry {
                vertices: vec![tip, waist + wing, base, waist - wing],
                draw: if filled { ArrowDraw::Filled } else { ArrowDraw::ClosedStroke },
                trim: if filled { length * FILLED_TRIM_FRACTION } else { length },
            }
        }
    })
}

fn circle(center: Point, radius: f64) -> Vec<Point> {
    (0..CIRCLE_SEGMENTS)
        .map(|i| {
            let theta =
                std::f64::consts::TAU * (i as f64) / (CIRCLE_SEGMENTS as f64);
            Point::new(center.x + radius * theta.cos(), center.y + radius * theta.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const THICKNESS: f64 = 2.0;

    /// The head Miro actually exported, rebuilt from the same inputs.
    #[test]
    fn the_filled_triangle_reproduces_miros_own_arrowhead() {
        let head =
            arrowhead(Arrowhead::FilledTriangle, Point::ORIGIN, Vec2::X, THICKNESS).unwrap();

        assert_eq!(head.draw, ArrowDraw::Filled);
        assert_eq!(head.vertices[0], Point::ORIGIN);
        // `M-12.727,-7.545 … L-12.727,7.545` — the two base corners.
        let mut corners = [head.vertices[1], head.vertices[2]];
        corners.sort_by(|a, b| a.y.total_cmp(&b.y));
        assert!((corners[0].x - -12.727_272_727_272_727).abs() < 1e-9, "{corners:?}");
        assert!((corners[0].y - -7.545_454_545_454_545).abs() < 1e-9);
        assert!((corners[1].y - 7.545_454_545_454_545).abs() < 1e-9);
        // And the line stops 10.182px short, as `M 0 0 L 357.968 0` does.
        assert!((head.trim - 10.181_818_181_818_182).abs() < 1e-9, "{}", head.trim);
    }

    #[test]
    fn a_head_points_along_the_tangent_it_is_given() {
        let tip = Point::new(100.0, 100.0);
        for (dir, expect_base) in [
            (Vec2::X, Point::new(100.0 - 12.727_272_727_272_727, 100.0)),
            (Vec2::Y, Point::new(100.0, 100.0 - 12.727_272_727_272_727)),
            (-Vec2::X, Point::new(100.0 + 12.727_272_727_272_727, 100.0)),
        ] {
            let head = arrowhead(Arrowhead::FilledTriangle, tip, dir, THICKNESS).unwrap();
            let base = head.vertices[1].midpoint(head.vertices[2]);
            assert!(base.distance_to(expect_base) < 1e-9, "{dir:?} gave {base:?}");
        }
    }

    #[test]
    fn a_diagonal_tangent_keeps_the_head_symmetric_about_it() {
        let dir = Vec2::new(1.0, 1.0).normalized().unwrap();
        let head = arrowhead(Arrowhead::FilledTriangle, Point::ORIGIN, dir, THICKNESS).unwrap();
        let base = head.vertices[1].midpoint(head.vertices[2]);
        // The base's midpoint lies exactly back along the tangent from the tip.
        let back = (Point::ORIGIN - base).normalized().unwrap();
        assert!((back.x - dir.x).abs() < 1e-9 && (back.y - dir.y).abs() < 1e-9, "{back:?}");
        // And both wings are the same distance from the axis.
        assert!(
            (head.vertices[1].distance_to(base) - head.vertices[2].distance_to(base)).abs() < 1e-9
        );
    }

    #[test]
    fn head_size_scales_with_thickness() {
        let thin = arrowhead(Arrowhead::FilledTriangle, Point::ORIGIN, Vec2::X, 1.0).unwrap();
        let thick = arrowhead(Arrowhead::FilledTriangle, Point::ORIGIN, Vec2::X, 8.0).unwrap();
        assert!((thick.trim / thin.trim - 8.0).abs() < 1e-9);
        let (a, b) = (thin.bounds().unwrap(), thick.bounds().unwrap());
        assert!((b.height() / a.height() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn every_form_produces_usable_geometry() {
        let forms = [
            (Arrowhead::LineArrow, ArrowDraw::OpenStroke),
            (Arrowhead::FilledTriangle, ArrowDraw::Filled),
            (Arrowhead::OpenTriangle, ArrowDraw::ClosedStroke),
            (Arrowhead::Circle, ArrowDraw::ClosedStroke),
            (Arrowhead::FilledCircle, ArrowDraw::Filled),
            (Arrowhead::Diamond, ArrowDraw::ClosedStroke),
            (Arrowhead::FilledDiamond, ArrowDraw::Filled),
        ];
        for (kind, draw) in forms {
            let head = arrowhead(kind, Point::ORIGIN, Vec2::X, THICKNESS).unwrap();
            assert_eq!(head.draw, draw, "{kind:?}");
            assert!(head.vertices.len() >= 3, "{kind:?}");
            assert!(head.vertices.iter().all(|v| v.is_finite()), "{kind:?}");
            assert!(head.trim >= 0.0 && head.trim.is_finite(), "{kind:?}");
            assert_eq!(kind.is_filled(), draw == ArrowDraw::Filled, "{kind:?}");
        }
    }

    #[test]
    fn a_circular_head_touches_the_endpoint() {
        let head = arrowhead(Arrowhead::FilledCircle, Point::ORIGIN, Vec2::X, THICKNESS).unwrap();
        let bounds = head.bounds().unwrap();
        let radius = THICKNESS * HALF_WIDTH_PER_THICKNESS;
        assert!((bounds.max.x - 0.0).abs() < 1e-9, "{bounds:?}");
        assert!((bounds.width() - radius * 2.0).abs() < 1e-6);
    }

    #[test]
    fn the_open_v_does_not_trim_the_line() {
        let head = arrowhead(Arrowhead::LineArrow, Point::ORIGIN, Vec2::X, THICKNESS).unwrap();
        assert_eq!(head.trim, 0.0);
        assert_eq!(head.vertices[1], Point::ORIGIN, "the middle vertex is the tip");
    }

    #[test]
    fn nothing_is_generated_without_a_direction_or_a_head() {
        assert!(arrowhead(Arrowhead::None, Point::ORIGIN, Vec2::X, THICKNESS).is_none());
        assert!(arrowhead(Arrowhead::FilledTriangle, Point::ORIGIN, Vec2::ZERO, THICKNESS).is_none());
        assert!(arrowhead(Arrowhead::FilledTriangle, Point::ORIGIN, Vec2::X, 0.0).is_none());
    }
}
