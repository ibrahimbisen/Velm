//! Signed distance parameters for the shapes that do not need triangles.
//!
//! `docs/01-architecture.md` §3 puts SDF shapes on the critical path: an analytic
//! shape is one draw of one instanced quad, resolution-independent at any zoom,
//! with borders and shadows falling out of the same distance value. Tessellation is
//! the fallback, not the default.
//!
//! ## Which shapes qualify, and why exactly those
//!
//! The dividing line is not "is there a formula" — there are published closed forms
//! for pies, stars and n-gons. It is **whether the shape's parameters can absorb a
//! non-uniform scale.** A shape here is defined in a unit box and then stretched
//! onto an item rect that is rarely square, and a signed distance field is only
//! meaningful under a *uniform* transform: stretch a circle's SDF by 2× in x and
//! every distance it reports is wrong, in a way that shows up as a border that is
//! visibly thicker on the sides than on the top.
//!
//! Three families survive that test, and they are exactly the three below:
//!
//! - **Axis-aligned boxes**, with or without corner radii. The half-extents take
//!   the stretch, and the radii stay in item units, so the corners stay circular.
//! - **Ellipses**, whose two radii are independent by construction.
//! - **Any polygon**, because a non-uniform scale applied to its vertices *is* the
//!   stretched polygon — nothing is approximated.
//!
//! That last one covers most of the shape catalogue: diamonds, trapezoids,
//! parallelograms, every regular polygon, stars, crosses, arrows, off-page
//! connectors and the flowchart symbols built from straight lines are all exactly
//! SDF-able. The shapes that fall through to tessellation are the ones with curves
//! that are not a whole ellipse — clouds, hearts, cylinders, documents, speech
//! bubbles, pie and arc sectors — plus anything carrying interior detail lines,
//! which a fill distance field cannot express at all.
//!
//! ## Coordinate space
//!
//! Every formula takes `p` in **item-local space**: origin at the item's centre,
//! `y` downwards, units the same as `Size` was given in. That is the unit box
//! translated by −0.5 and scaled by the item size, so a fragment shader computes it
//! with one multiply-add from its interpolated quad coordinate.
//!
//! Formulas are Inigo Quilez's 2D distance functions, adapted to y-down. They are
//! reproduced here in Rust as [`SdfParams::distance`] rather than left as comments,
//! for one reason: the test suite asserts that the sign of the analytic distance
//! agrees with the tessellated outline's `contains` over a dense grid, for every
//! shape that reports parameters. A shader and a mesh disagreeing about where a
//! shape's edge is would otherwise be found by eye, months later.

use crate::shape::Shape;
use crate::unit::{Point, Size};
use serde::{Deserialize, Serialize};

/// The parameters a fragment shader needs to evaluate a shape analytically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SdfParams {
    /// Axis-aligned rectangle.
    ///
    /// ```wgsl
    /// fn sd_box(p: vec2f, half_extent: vec2f) -> f32 {
    ///     let d = abs(p) - half_extent;
    ///     return length(max(d, vec2f(0.0))) + min(max(d.x, d.y), 0.0);
    /// }
    /// ```
    ///
    /// The `length(max(d,0))` term is the distance outside; the `min(max,0)` term
    /// is the negative distance inside. Exact everywhere.
    Box { half_extent: [f32; 2] },

    /// Rectangle with per-corner radii, in CSS order: top-left, top-right,
    /// bottom-right, bottom-left. This is the shape a sticky note, a frame and a
    /// terminator all reduce to, and the reason the renderer needs no shape data at
    /// all for the most common items on a board.
    ///
    /// ```wgsl
    /// fn sd_rounded_box(p: vec2f, half_extent: vec2f, radii: vec4f) -> f32 {
    ///     // radii = (top_left, top_right, bottom_right, bottom_left), y down
    ///     var r = select(radii.x, radii.y, p.x > 0.0);
    ///     if (p.y > 0.0) { r = select(radii.w, radii.z, p.x > 0.0); }
    ///     let q = abs(p) - half_extent + r;
    ///     return min(max(q.x, q.y), 0.0) + length(max(q, vec2f(0.0))) - r;
    /// }
    /// ```
    ///
    /// It is the box formula on a rectangle inset by `r`, with `r` added back — a
    /// rounded shape is its skeleton dilated by the radius. Exact everywhere.
    RoundedBox { half_extent: [f32; 2], corner_radii: [f32; 4] },

    /// Axis-aligned ellipse.
    ///
    /// ```wgsl
    /// fn sd_ellipse(p: vec2f, r: vec2f) -> f32 {
    ///     let k1 = length(p / r);
    ///     let k2 = length(p / (r * r));
    ///     return k1 * (k1 - 1.0) / k2;
    /// }
    /// ```
    ///
    /// The exact ellipse distance needs a quartic root per pixel. This is the
    /// standard first-order approximation: the sign is **exact** (it is the
    /// algebraic inside test `|p/r| < 1`), and the magnitude is exact for a circle
    /// and within a few percent for the aspect ratios boards actually contain. Only
    /// antialiasing width depends on the magnitude, so the error is sub-pixel.
    Ellipse { radii: [f32; 2] },

    /// Any simple polygon, as its vertices in item-local space.
    ///
    /// ```wgsl
    /// // v: array of N vertices, any winding
    /// fn sd_polygon(p: vec2f) -> f32 {
    ///     var d = dot(p - v[0], p - v[0]);
    ///     var s = 1.0;
    ///     var j = N - 1u;
    ///     for (var i = 0u; i < N; i = i + 1u) {
    ///         let e = v[j] - v[i];
    ///         let w = p - v[i];
    ///         let b = w - e * clamp(dot(w, e) / dot(e, e), 0.0, 1.0);
    ///         d = min(d, dot(b, b));
    ///         let c = vec3<bool>(p.y >= v[i].y, p.y < v[j].y, e.x * w.y > e.y * w.x);
    ///         if (all(c) || all(!c)) { s = -s; }
    ///         j = i;
    ///     }
    ///     return s * sqrt(d);
    /// }
    /// ```
    ///
    /// `d` accumulates the squared distance to the nearest edge; `s` accumulates a
    /// crossing-number winding test that flips the sign inside. Exact everywhere,
    /// at O(n) per fragment — for the ≤ 12 vertices these shapes have, cheaper than
    /// the vertex work the tessellated alternative would need.
    Polygon { vertices: Vec<Point> },
}

impl SdfParams {
    /// Signed distance to the shape's edge: negative inside, positive outside.
    ///
    /// `p` is item-local — origin at the item centre, y downwards.
    pub fn distance(&self, p: Point) -> f32 {
        match self {
            Self::Box { half_extent } => sd_box(p, Point::new(half_extent[0], half_extent[1])),
            Self::RoundedBox { half_extent, corner_radii } => {
                let half = Point::new(half_extent[0], half_extent[1]);
                let r = match (p.x > 0.0, p.y > 0.0) {
                    (false, false) => corner_radii[0],
                    (true, false) => corner_radii[1],
                    (true, true) => corner_radii[2],
                    (false, true) => corner_radii[3],
                };
                let q = Point::new(p.x.abs() - half.x + r, p.y.abs() - half.y + r);
                q.x.max(q.y).min(0.0) + Point::new(q.x.max(0.0), q.y.max(0.0)).length() - r
            }
            Self::Ellipse { radii } => {
                let (rx, ry) = (radii[0], radii[1]);
                if rx <= 0.0 || ry <= 0.0 {
                    return f32::INFINITY;
                }
                let k1 = Point::new(p.x / rx, p.y / ry).length();
                let k2 = Point::new(p.x / (rx * rx), p.y / (ry * ry)).length();
                // The centre is a singularity of the approximation; the distance
                // there is simply the shorter radius.
                if k2 <= f32::MIN_POSITIVE { -rx.min(ry) } else { k1 * (k1 - 1.0) / k2 }
            }
            Self::Polygon { vertices } => sd_polygon(vertices, p),
        }
    }

    /// Inside test straight from the field. Agrees with
    /// [`Outline::contains`](crate::Outline::contains) — asserted per shape in the
    /// test suite.
    pub fn contains(&self, p: Point) -> bool {
        self.distance(p) < 0.0
    }
}

fn sd_box(p: Point, half: Point) -> f32 {
    let d = Point::new(p.x.abs() - half.x, p.y.abs() - half.y);
    Point::new(d.x.max(0.0), d.y.max(0.0)).length() + d.x.max(d.y).min(0.0)
}

fn sd_polygon(v: &[Point], p: Point) -> f32 {
    if v.is_empty() {
        return f32::INFINITY;
    }
    let mut squared = (p - v[0]).dot(p - v[0]);
    let mut sign = 1.0f32;
    let mut j = v.len() - 1;
    for i in 0..v.len() {
        let e = v[j] - v[i];
        let w = p - v[i];
        let b = w - e * (w.dot(e) / e.dot(e)).clamp(0.0, 1.0);
        squared = squared.min(b.dot(b));
        let crossings = [p.y >= v[i].y, p.y < v[j].y, e.cross(w) > 0.0];
        if crossings.iter().all(|&c| c) || crossings.iter().all(|&c| !c) {
            sign = -sign;
        }
        j = i;
    }
    sign * squared.sqrt()
}

/// The corner radius a fraction resolves to, in item units.
///
/// Radii are a fraction of the **shorter** side, so a rounded rectangle keeps the
/// same visual corner however it is stretched, and `0.5` is the fully rounded
/// stadium — the same convention CSS and every design tool use.
pub(crate) fn corner_radius(fraction: f32, shorter_side: f32) -> f32 {
    fraction.clamp(0.0, 0.5) * shorter_side
}

impl Shape {
    /// Parameters for drawing this shape analytically, or `None` if it has to be
    /// tessellated. See the module docs for why the split falls where it does.
    ///
    /// `size` is the item's box in the caller's units; the returned parameters are
    /// in that same scale, centred on the item.
    pub fn sdf_params(&self, size: Size) -> Option<SdfParams> {
        let half_extent = [size.width * 0.5, size.height * 0.5];
        let shorter = size.width.min(size.height).max(0.0);
        let closed_form = match *self {
            Self::Rectangle => Some(SdfParams::Box { half_extent }),
            Self::RoundedRectangle { radius } => Some(SdfParams::RoundedBox {
                half_extent,
                corner_radii: [corner_radius(radius, shorter); 4],
            }),
            Self::Terminator => Some(SdfParams::RoundedBox {
                half_extent,
                corner_radii: [shorter * 0.5; 4],
            }),
            // Only the right-hand corners are rounded, which the four-radius form
            // expresses directly — no second formula needed.
            Self::Delay => Some(SdfParams::RoundedBox {
                half_extent,
                corner_radii: [0.0, shorter * 0.5, shorter * 0.5, 0.0],
            }),
            Self::Ellipse => Some(SdfParams::Ellipse { radii: half_extent }),
            _ => None,
        };
        if closed_form.is_some() {
            return closed_form;
        }
        self.outline(size.aspect()).as_polygon().map(|vertices| SdfParams::Polygon {
            vertices: vertices
                .iter()
                .map(|v| Point::new((v.x - 0.5) * size.width, (v.y - 0.5) * size.height))
                .collect(),
        })
    }

    /// Whether this shape can be drawn without triangles.
    pub fn is_analytic(&self) -> bool {
        self.sdf_params(Size::SQUARE).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::p;

    #[test]
    fn box_distance_is_exact_inside_and_out() {
        let sdf = SdfParams::Box { half_extent: [2.0, 1.0] };
        assert_eq!(sdf.distance(p(0.0, 0.0)), -1.0);
        assert_eq!(sdf.distance(p(2.0, 0.0)), 0.0);
        assert_eq!(sdf.distance(p(5.0, 0.0)), 3.0);
        assert!((sdf.distance(p(5.0, 4.0)) - 4.242_64).abs() < 1e-4);
    }

    #[test]
    fn rounded_box_corners_are_circular() {
        let sdf = SdfParams::RoundedBox { half_extent: [2.0, 2.0], corner_radii: [1.0; 4] };
        // The corner arc is centred at (1,1) with radius 1, so this diagonal point
        // is exactly on the edge.
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;
        let on_arc = p(1.0 + diagonal, 1.0 + diagonal);
        assert!(sdf.distance(on_arc).abs() < 1e-4, "{}", sdf.distance(on_arc));
        assert!(!sdf.contains(p(2.0, 2.0)), "the square corner is cut away");
        assert!(sdf.contains(p(0.0, 1.9)));
    }

    #[test]
    fn only_the_named_corners_of_a_delay_are_rounded() {
        let sdf = Shape::Delay.sdf_params(Size::new(4.0, 4.0)).unwrap();
        assert!(sdf.contains(p(-1.9, -1.9)), "the left corners stay square");
        assert!(!sdf.contains(p(1.9, -1.9)), "the right corners are rounded away");
    }

    #[test]
    fn ellipse_sign_is_exact_even_though_the_magnitude_is_approximate() {
        let sdf = SdfParams::Ellipse { radii: [2.0, 1.0] };
        assert!(sdf.contains(p(1.99, 0.0)));
        assert!(!sdf.contains(p(2.01, 0.0)));
        assert!(sdf.contains(p(0.0, 0.99)));
        assert!(!sdf.contains(p(0.0, 1.01)));
        assert!((sdf.distance(p(2.0, 0.0))).abs() < 1e-5);
        assert!(sdf.distance(p(0.0, 0.0)).is_finite());
    }

    #[test]
    fn polygon_distance_handles_a_concave_corner() {
        // An L, whose reflex corner is at (0,0).
        let l = SdfParams::Polygon {
            vertices: vec![p(-2.0, -2.0), p(0.0, -2.0), p(0.0, 0.0), p(2.0, 0.0), p(2.0, 2.0), p(-2.0, 2.0)],
        };
        assert!(l.contains(p(-1.0, -1.0)));
        assert!(l.contains(p(1.0, 1.0)));
        assert!(!l.contains(p(1.0, -1.0)), "the notch is outside");
        assert!((l.distance(p(1.0, -1.0)) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn rounded_rectangle_radius_is_a_fraction_of_the_shorter_side() {
        let wide = Shape::RoundedRectangle { radius: 0.25 }.sdf_params(Size::new(400.0, 100.0));
        let Some(SdfParams::RoundedBox { corner_radii, .. }) = wide else {
            panic!("a rounded rectangle must have a closed form");
        };
        assert_eq!(corner_radii, [25.0; 4]);
    }

    #[test]
    fn straight_edged_shapes_fall_through_to_the_polygon_form() {
        assert!(matches!(
            Shape::Diamond.sdf_params(Size::SQUARE),
            Some(SdfParams::Polygon { .. })
        ));
        assert!(matches!(
            Shape::star().sdf_params(Size::SQUARE),
            Some(SdfParams::Polygon { .. })
        ));
    }

    #[test]
    fn curved_and_decorated_shapes_report_no_parameters() {
        assert!(Shape::Cloud.sdf_params(Size::SQUARE).is_none());
        assert!(Shape::cylinder().sdf_params(Size::SQUARE).is_none());
        assert!(Shape::or().sdf_params(Size::SQUARE).is_none(), "detail lines are not fillable");
    }
}
