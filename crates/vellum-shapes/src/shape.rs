//! Every shape Miro offers, defined once.
//!
//! ## One enum, one geometry function
//!
//! [`Shape`] is a plain value — a kind plus its parameters, small, `Copy`, and
//! serialisable straight into the document. All of its geometry comes from
//! [`Shape::outline`], and everything else in the crate is derived from that
//! outline. Adding a shape is a variant, an arm in `outline`, an arm in `name`, an
//! entry in [`CATALOGUE`], and a probe row in `tests/shapes.rs` — the last two are
//! enforced by a test that fails if a catalogue entry has no probes.
//!
//! ## Parameters, not variants
//!
//! Pentagon, hexagon and octagon are one [`Shape::RegularPolygon`]; the four
//! rotations of an arrow are one [`Shape::Arrow`]. Miro's palette shows them as
//! separate tiles, and [`CATALOGUE`] lists them as separate entries, but the
//! geometry is written once. The same reasoning goes the other way for flowchart
//! symbols that *are* another shape — a process is a rectangle, a decision is a
//! diamond — which get named constructors ([`Shape::flowchart_process`]) rather
//! than duplicate variants, so that two identical diamonds can never drift apart.
//!
//! ## Aspect ratio
//!
//! `outline` takes the aspect ratio of the box the shape will be drawn in. Almost
//! every shape ignores it — a diamond stretches with its box and that is correct.
//! Four do not: a rounded rectangle, a terminator, a delay and a speech bubble all
//! have a feature that must stay *circular* while the box stretches, and a corner
//! radius expressed as a fraction of a stretched box is an ellipse. Passing `1.0`
//! gives the pure unit-box form.

use crate::anchor::Anchor;
use crate::error::ShapeError;
use crate::mesh::{ShapeMesh, TessellationOptions};
use crate::outline::{Contour, ContourBuilder, Outline};
use crate::sdf::corner_radius;
use crate::unit::{Bounds, Point, p};
use serde::{Deserialize, Serialize};

/// Which way a directional shape points.
///
/// Implemented as a quarter-turn of the rightward form rather than four sets of
/// coordinates: the unit box is square, so rotating it maps the box onto itself and
/// preserves both the winding and the exact bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Right,
    Down,
    Left,
    Up,
}

impl Direction {
    pub const ALL: [Self; 4] = [Self::Right, Self::Down, Self::Left, Self::Up];

    const fn quarter_turns(self) -> u8 {
        match self {
            Self::Right => 0,
            Self::Down => 1,
            Self::Left => 2,
            Self::Up => 3,
        }
    }

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
            Self::Left => "left",
            Self::Up => "up",
        }
    }
}

/// The arrow silhouettes Miro ships. All are described pointing right and rotated
/// by [`Direction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArrowForm {
    /// A shaft with one triangular head — the plain block arrow.
    Simple,
    /// A shaft with a head at both ends.
    Double,
    /// A solid pentagon with a pointed end. Miro calls this an "arrow block"; it is
    /// what process-flow strips are built from.
    Chevron,
    /// A chevron with a matching notch cut into its tail, so a row of them
    /// interlocks.
    Notched,
    /// An L: rises from the bottom-left, turns right, and heads out of the right
    /// edge.
    Bent,
}

impl ArrowForm {
    pub const ALL: [Self; 5] =
        [Self::Simple, Self::Double, Self::Chevron, Self::Notched, Self::Bent];

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Double => "double",
            Self::Chevron => "chevron",
            Self::Notched => "notched",
            Self::Bent => "bent",
        }
    }
}

/// A shape, as stored on an item.
///
/// Every parameter is a fraction of the unit box, so a shape is independent of the
/// size it is drawn at. Out-of-range parameters are clamped when the outline is
/// built rather than rejected here: a corrupt radius should make a board look odd,
/// not fail to open.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Shape {
    /// The whole box.
    Rectangle,
    /// `radius` is a fraction of the **shorter** side, so `0.5` is a stadium and the
    /// corners stay circular however the box is stretched.
    RoundedRectangle { radius: f32 },
    /// Inscribed in the box; a circle when the box is square.
    Ellipse,
    /// `sides` corners on the inscribed circle, the first at `rotation_deg`
    /// (clockwise from east), then normalised to fill the box. Covers triangle,
    /// pentagon, hexagon, octagon and anything else regular.
    RegularPolygon { sides: u32, rotation_deg: f32 },
    /// Right angle at the bottom-left.
    RightTriangle,
    /// The four edge midpoints — Miro's rhombus, and the flowchart decision.
    Diamond,
    /// `slant` is how far the top edge is pushed right, as a fraction of the width.
    Parallelogram { slant: f32 },
    /// `top_inset` is how far each end of the top edge is pulled in.
    Trapezoid { top_inset: f32 },
    /// `points` tips at the inscribed circle with valleys at `inner_ratio` of it.
    Star { points: u32, inner_ratio: f32 },
    /// A plus sign; `arm` is the arm thickness as a fraction of the box.
    Cross { arm: f32 },
    /// `shaft` is the shaft thickness and `head` the head length, both fractions of
    /// the box before rotation.
    Arrow { form: ArrowForm, direction: Direction, shaft: f32, head: f32 },
    /// A rounded body with a tail at the bottom left. `radius` behaves as it does on
    /// a rounded rectangle; `tail` is the tail's height as a fraction of the box.
    SpeechBubble { radius: f32, tail: f32 },
    /// Upright can, drawn with the front rim of its top ellipse. `cap` is the
    /// ellipse's vertical radius.
    Cylinder { cap: f32 },
    /// The union of nine overlapping discs. Not parameterised: the lobe radii are a
    /// fixed, deliberately uneven table, because a cloud with equal lobes reads as
    /// a cog.
    Cloud,
    /// Two bezier lobes over a point.
    Heart,
    /// A ring sector — the band between `inner_ratio` and the full radius, from
    /// `start_deg` clockwise through `sweep_deg`, then normalised to fill the box.
    Arc { start_deg: f32, sweep_deg: f32, inner_ratio: f32 },
    /// A pie slice: the same sweep, filled to the centre.
    Wedge { start_deg: f32, sweep_deg: f32 },
    /// Flowchart terminator — a rectangle with semicircular ends.
    Terminator,
    /// Flowchart document — a rectangle with a wave along the bottom. `wave` is the
    /// wave's amplitude.
    Document { wave: f32 },
    /// `copies` documents in a stack, offset up and to the right.
    MultiDocument { copies: u32 },
    /// Flowchart manual input — a rectangle with a sloped top edge.
    ManualInput { slope: f32 },
    /// Flowchart manual operation — a trapezoid narrowing downwards.
    ManualOperation { bottom_inset: f32 },
    /// Flowchart predefined process — a rectangle with a bar down each side.
    /// `bar` is the bars' inset from the edges.
    PredefinedProcess { bar: f32 },
    /// Flowchart internal storage — a rectangle ruled once vertically and once
    /// horizontally.
    InternalStorage { bar: f32 },
    /// Flowchart direct access storage: a cylinder on its side.
    DirectData { cap: f32 },
    /// Flowchart stored data — a rectangle whose left edge curves inwards.
    StoredData { cap: f32 },
    /// Flowchart delay — a rectangle with one semicircular end.
    Delay,
    /// Flowchart display — pointed on the left, round on the right.
    Display { point: f32, cap: f32 },
    /// Flowchart or — a circle ruled with a cross.
    Or,
    /// Flowchart summing junction — a circle ruled with a saltire.
    SummingJunction,
    /// Flowchart off-page connector — a rectangle tapering to a point at the bottom.
    OffPageConnector { point: f32 },
    /// A cylinder with `layers` rims, the database symbol.
    Database { cap: f32, layers: u32 },
}

/// Wave amplitude of a document's bottom edge. Shared, because a multi-document
/// stack is documents — a stack whose pages waved differently from a single page
/// would be a bug nobody would think to look for.
const DOCUMENT_WAVE: f32 = 0.14;

impl Shape {
    // Defaults are the proportions Miro's own palette draws, checked against the
    // shapes in the reference board's SVG export.

    pub const fn rounded_rectangle() -> Self {
        Self::RoundedRectangle { radius: 0.12 }
    }

    pub const fn triangle() -> Self {
        Self::RegularPolygon { sides: 3, rotation_deg: -90.0 }
    }

    pub const fn pentagon() -> Self {
        Self::RegularPolygon { sides: 5, rotation_deg: -90.0 }
    }

    /// Vertices at east and west, edges flat on top and bottom — which is also the
    /// flowchart preparation symbol.
    pub const fn hexagon() -> Self {
        Self::RegularPolygon { sides: 6, rotation_deg: 0.0 }
    }

    /// Rotated half a step so it has flat top, bottom and sides, like a road sign.
    pub const fn octagon() -> Self {
        Self::RegularPolygon { sides: 8, rotation_deg: 22.5 }
    }

    pub const fn star() -> Self {
        Self::Star { points: 5, inner_ratio: 0.4 }
    }

    pub const fn cross() -> Self {
        Self::Cross { arm: 0.34 }
    }

    pub const fn parallelogram() -> Self {
        Self::Parallelogram { slant: 0.25 }
    }

    pub const fn trapezoid() -> Self {
        Self::Trapezoid { top_inset: 0.25 }
    }

    /// Proportions differ per form: a chevron with a block arrow's head length is
    /// all head, and a bent arrow with a block arrow's shaft is all shaft.
    pub const fn arrow(form: ArrowForm, direction: Direction) -> Self {
        let (shaft, head) = match form {
            ArrowForm::Simple => (0.5, 0.42),
            ArrowForm::Double => (0.5, 0.3),
            ArrowForm::Chevron | ArrowForm::Notched => (1.0, 0.3),
            ArrowForm::Bent => (0.32, 0.34),
        };
        Self::Arrow { form, direction, shaft, head }
    }

    pub const fn speech_bubble() -> Self {
        Self::SpeechBubble { radius: 0.14, tail: 0.24 }
    }

    pub const fn cylinder() -> Self {
        Self::Cylinder { cap: 0.12 }
    }

    pub const fn arc() -> Self {
        Self::Arc { start_deg: 180.0, sweep_deg: 180.0, inner_ratio: 0.55 }
    }

    pub const fn wedge() -> Self {
        Self::Wedge { start_deg: 0.0, sweep_deg: 270.0 }
    }

    pub const fn document() -> Self {
        Self::Document { wave: DOCUMENT_WAVE }
    }

    pub const fn multi_document() -> Self {
        Self::MultiDocument { copies: 3 }
    }

    pub const fn manual_input() -> Self {
        Self::ManualInput { slope: 0.2 }
    }

    pub const fn manual_operation() -> Self {
        Self::ManualOperation { bottom_inset: 0.18 }
    }

    pub const fn predefined_process() -> Self {
        Self::PredefinedProcess { bar: 0.12 }
    }

    pub const fn internal_storage() -> Self {
        Self::InternalStorage { bar: 0.16 }
    }

    pub const fn direct_data() -> Self {
        Self::DirectData { cap: 0.12 }
    }

    pub const fn stored_data() -> Self {
        Self::StoredData { cap: 0.16 }
    }

    pub const fn display() -> Self {
        Self::Display { point: 0.18, cap: 0.22 }
    }

    pub const fn or() -> Self {
        Self::Or
    }

    pub const fn off_page_connector() -> Self {
        Self::OffPageConnector { point: 0.3 }
    }

    pub const fn database() -> Self {
        Self::Database { cap: 0.1, layers: 3 }
    }

    // Flowchart symbols that are geometrically another shape. Named so a palette
    // and a document can say what they mean, without a second copy of the geometry.

    /// The flowchart process box: a plain rectangle.
    pub const fn flowchart_process() -> Self {
        Self::Rectangle
    }

    /// The flowchart decision: a diamond.
    pub const fn flowchart_decision() -> Self {
        Self::Diamond
    }

    /// The flowchart data symbol: a parallelogram.
    pub const fn flowchart_data() -> Self {
        Self::parallelogram()
    }

    /// The flowchart preparation symbol: a hexagon.
    pub const fn flowchart_preparation() -> Self {
        Self::hexagon()
    }

    /// The flowchart merge symbol: a downward triangle.
    pub const fn flowchart_merge() -> Self {
        Self::RegularPolygon { sides: 3, rotation_deg: 90.0 }
    }

    /// Stable name for the document format, the UI and logs. Parameters are not
    /// part of it — a five- and a six-pointed star are both `star`.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Rectangle => "rectangle",
            Self::RoundedRectangle { .. } => "rounded_rectangle",
            Self::Ellipse => "ellipse",
            Self::RegularPolygon { .. } => "regular_polygon",
            Self::RightTriangle => "right_triangle",
            Self::Diamond => "diamond",
            Self::Parallelogram { .. } => "parallelogram",
            Self::Trapezoid { .. } => "trapezoid",
            Self::Star { .. } => "star",
            Self::Cross { .. } => "cross",
            Self::Arrow { .. } => "arrow",
            Self::SpeechBubble { .. } => "speech_bubble",
            Self::Cylinder { .. } => "cylinder",
            Self::Cloud => "cloud",
            Self::Heart => "heart",
            Self::Arc { .. } => "arc",
            Self::Wedge { .. } => "wedge",
            Self::Terminator => "terminator",
            Self::Document { .. } => "document",
            Self::MultiDocument { .. } => "multi_document",
            Self::ManualInput { .. } => "manual_input",
            Self::ManualOperation { .. } => "manual_operation",
            Self::PredefinedProcess { .. } => "predefined_process",
            Self::InternalStorage { .. } => "internal_storage",
            Self::DirectData { .. } => "direct_data",
            Self::StoredData { .. } => "stored_data",
            Self::Delay => "delay",
            Self::Display { .. } => "display",
            Self::Or => "or",
            Self::SummingJunction => "summing_junction",
            Self::OffPageConnector { .. } => "off_page_connector",
            Self::Database { .. } => "database",
        }
    }

    /// The shape's geometry in the unit box.
    ///
    /// `aspect` is `width / height` of the box it will be drawn in; pass `1.0` for
    /// the plain unit-box form. The result always fills the unit box exactly.
    ///
    /// This allocates. Callers doing several queries on one shape — bounds, a hit
    /// test and anchors, say — should build the outline once and use it directly;
    /// the convenience methods below rebuild it each time.
    pub fn outline(&self, aspect: f32) -> Outline {
        let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.0 };
        let outline = match *self {
            Self::Rectangle => Outline::polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)]),

            Self::RoundedRectangle { radius } => {
                let (rx, ry) = circular_radii(corner_radius(radius, aspect.min(1.0)), aspect);
                Outline::from_contour(rounded_rect_contour(rx, ry, 0.0, 1.0))
            }

            Self::Ellipse => Outline::from_contour(ellipse_contour(p(0.5, 0.5), 0.5, 0.5)),

            Self::RegularPolygon { sides, rotation_deg } => {
                Outline::polygon(&regular_polygon_points(sides, rotation_deg))
            }

            Self::RightTriangle => Outline::polygon(&[p(0.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)]),

            Self::Diamond => {
                Outline::polygon(&[p(0.5, 0.0), p(1.0, 0.5), p(0.5, 1.0), p(0.0, 0.5)])
            }

            Self::Parallelogram { slant } => {
                let s = slant.clamp(0.0, 0.9);
                Outline::polygon(&[p(s, 0.0), p(1.0, 0.0), p(1.0 - s, 1.0), p(0.0, 1.0)])
            }

            Self::Trapezoid { top_inset } => {
                let t = top_inset.clamp(0.0, 0.49);
                Outline::polygon(&[p(t, 0.0), p(1.0 - t, 0.0), p(1.0, 1.0), p(0.0, 1.0)])
            }

            Self::Star { points, inner_ratio } => {
                Outline::polygon(&star_points(points, inner_ratio))
            }

            Self::Cross { arm } => {
                let a = (1.0 - arm.clamp(0.05, 0.95)) * 0.5;
                Outline::polygon(&[
                    p(a, 0.0),
                    p(1.0 - a, 0.0),
                    p(1.0 - a, a),
                    p(1.0, a),
                    p(1.0, 1.0 - a),
                    p(1.0 - a, 1.0 - a),
                    p(1.0 - a, 1.0),
                    p(a, 1.0),
                    p(a, 1.0 - a),
                    p(0.0, 1.0 - a),
                    p(0.0, a),
                    p(a, a),
                ])
            }

            Self::Arrow { form, direction, shaft, head } => {
                rotated(arrow_outline(form, shaft, head), direction.quarter_turns())
            }

            Self::SpeechBubble { radius, tail } => {
                let tail = tail.clamp(0.05, 0.6);
                let body = 1.0 - tail;
                let (rx, ry) = circular_radii(corner_radius(radius, aspect.min(1.0)), aspect);
                Outline::from_contour(speech_bubble_contour(rx, ry.min(body * 0.5), body))
            }

            Self::Cylinder { cap } => cylinder_outline(cap),

            Self::Cloud => Outline::from_contour(cloud_contour()),

            Self::Heart => Outline::from_contour(heart_contour()),

            Self::Arc { start_deg, sweep_deg, inner_ratio } => {
                let (start, sweep) = normalise_sweep(start_deg, sweep_deg);
                let inner = 0.5 * inner_ratio.clamp(0.02, 0.95);
                let centre = p(0.5, 0.5);
                Outline::from_contour(closed(polar(centre, 0.5, 0.5, start), |b| {
                    b.arc(centre, 0.5, 0.5, start, start + sweep)
                        .line_to(polar(centre, inner, inner, start + sweep))
                        .arc(centre, inner, inner, start + sweep, start);
                }))
            }

            Self::Wedge { start_deg, sweep_deg } => {
                let (start, sweep) = normalise_sweep(start_deg, sweep_deg);
                let centre = p(0.5, 0.5);
                Outline::from_contour(closed(centre, |b| {
                    b.line_to(polar(centre, 0.5, 0.5, start))
                        .arc(centre, 0.5, 0.5, start, start + sweep);
                }))
            }

            Self::Terminator => {
                let (rx, ry) = circular_radii(aspect.min(1.0) * 0.5, aspect);
                Outline::from_contour(rounded_rect_contour(rx, ry, 0.0, 1.0))
            }

            Self::Document { wave } => Outline::from_contour(document_contour(wave)),

            Self::MultiDocument { copies } => {
                let copies = copies.clamp(2, 6);
                // Enough offset that the stack is legible at sticky-note size,
                // small enough that three copies still leave a usable front page.
                let offset = 0.09;
                let span = 1.0 - offset * (copies - 1) as f32;
                let base = document_contour(DOCUMENT_WAVE);
                // Back to front: the backmost copy sits top-right, the front one
                // bottom-left, which is the direction Miro stacks them.
                Outline::new(
                    (0..copies)
                        .map(|i| {
                            let origin =
                                p(offset * (copies - 1 - i) as f32, offset * i as f32);
                            base.map(&|q| p(origin.x + q.x * span, origin.y + q.y * span))
                        })
                        .collect(),
                )
            }

            Self::ManualInput { slope } => {
                let s = slope.clamp(0.0, 0.9);
                Outline::polygon(&[p(0.0, s), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)])
            }

            Self::ManualOperation { bottom_inset } => {
                let t = bottom_inset.clamp(0.0, 0.49);
                Outline::polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0 - t, 1.0), p(t, 1.0)])
            }

            Self::PredefinedProcess { bar } => {
                let b = bar.clamp(0.02, 0.45);
                Outline::polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)]).with_details(
                    vec![
                        Contour::line(p(b, 0.0), p(b, 1.0)),
                        Contour::line(p(1.0 - b, 0.0), p(1.0 - b, 1.0)),
                    ],
                )
            }

            Self::InternalStorage { bar } => {
                let b = bar.clamp(0.02, 0.45);
                Outline::polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)]).with_details(
                    vec![
                        Contour::line(p(b, 0.0), p(b, 1.0)),
                        Contour::line(p(0.0, b), p(1.0, b)),
                    ],
                )
            }

            Self::DirectData { cap } => rotated(cylinder_outline(cap), 1),

            Self::StoredData { cap } => {
                let c = cap.clamp(0.02, 0.45);
                Outline::from_contour(closed(p(0.0, 0.0), |b| {
                    b.line_to(p(1.0 - c, 0.0))
                        .arc(p(1.0 - c, 0.5), c, 0.5, -90.0, 90.0)
                        .line_to(p(0.0, 1.0))
                        // The left edge bulges the same way as the right one, which
                        // is what makes the symbol read as a stack seen edge-on.
                        .arc(p(0.0, 0.5), c, 0.5, 90.0, -90.0);
                }))
            }

            Self::Delay => {
                let (rx, ry) = circular_radii(aspect.min(1.0) * 0.5, aspect);
                Outline::from_contour(closed(p(0.0, 0.0), |b| {
                    b.line_to(p(1.0 - rx, 0.0))
                        .arc(p(1.0 - rx, ry), rx, ry, -90.0, 0.0)
                        .line_to(p(1.0, 1.0 - ry))
                        .arc(p(1.0 - rx, 1.0 - ry), rx, ry, 0.0, 90.0)
                        .line_to(p(0.0, 1.0));
                }))
            }

            Self::Display { point, cap } => {
                let (pt, c) = (point.clamp(0.0, 0.45), cap.clamp(0.02, 0.45));
                Outline::from_contour(closed(p(pt, 0.0), |b| {
                    b.line_to(p(1.0 - c, 0.0))
                        .arc(p(1.0 - c, 0.5), c, 0.5, -90.0, 90.0)
                        .line_to(p(pt, 1.0))
                        .line_to(p(0.0, 0.5));
                }))
            }

            Self::Or => Outline::from_contour(ellipse_contour(p(0.5, 0.5), 0.5, 0.5)).with_details(
                vec![
                    Contour::line(p(0.0, 0.5), p(1.0, 0.5)),
                    Contour::line(p(0.5, 0.0), p(0.5, 1.0)),
                ],
            ),

            Self::SummingJunction => {
                // The saltire meets the circle at 45°, at half a radius times √2.
                let d = 0.5 - 0.5 * std::f32::consts::FRAC_1_SQRT_2;
                Outline::from_contour(ellipse_contour(p(0.5, 0.5), 0.5, 0.5)).with_details(vec![
                    Contour::line(p(d, d), p(1.0 - d, 1.0 - d)),
                    Contour::line(p(1.0 - d, d), p(d, 1.0 - d)),
                ])
            }

            Self::OffPageConnector { point } => {
                let t = 1.0 - point.clamp(0.05, 0.9);
                Outline::polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, t), p(0.5, 1.0), p(0.0, t)])
            }

            Self::Database { cap, layers } => {
                let c = cap.clamp(0.02, 0.3);
                let mut outline = cylinder_outline(c);
                // Extra rims below the first, spaced far enough apart to stay
                // distinct without reaching the bottom cap.
                let step = c * 1.7;
                for i in 1..layers.clamp(1, 5) {
                    let y = c + step * i as f32;
                    outline.details.push(open(p(0.0, y), |b| {
                        b.arc(p(0.5, y), 0.5, c, 180.0, 0.0);
                    }));
                }
                outline
            }
        };
        outline.normalised()
    }

    /// Exact bounds. Always the unit box — shapes are normalised to fill it — but
    /// exposed so a caller can assert that rather than assume it.
    pub fn bounds(&self, aspect: f32) -> Bounds {
        self.outline(aspect).bounds()
    }

    /// Hit test in unit-box coordinates. Accurate for concave shapes: a point in
    /// the corner of a star's box is a miss.
    pub fn contains(&self, point: Point, aspect: f32) -> bool {
        self.outline(aspect).contains(point)
    }

    /// The silhouette as a `lyon` path. Interior detail lines are not included —
    /// filling them would be wrong; [`Outline::stroke_path`] has both.
    pub fn to_path(&self, aspect: f32) -> lyon::path::Path {
        self.outline(aspect).fill_path()
    }

    /// Fill and stroke meshes in unit-box coordinates.
    pub fn tessellate(
        &self,
        aspect: f32,
        options: TessellationOptions,
    ) -> Result<ShapeMesh, ShapeError> {
        self.outline(aspect).tessellate(options)
    }

    /// The eight connector attachment points, resolved onto the shape.
    pub fn anchors(&self, aspect: f32) -> [Anchor; 8] {
        self.outline(aspect).anchors()
    }
}

/// Every shape, in its default proportions — the palette, and the list the test
/// suite walks. Parametric variants appear once per form Miro shows as its own
/// tile.
pub const CATALOGUE: &[Shape] = &[
    // Basic
    Shape::Rectangle,
    Shape::rounded_rectangle(),
    Shape::Ellipse,
    Shape::triangle(),
    Shape::RightTriangle,
    Shape::Diamond,
    Shape::parallelogram(),
    Shape::trapezoid(),
    Shape::pentagon(),
    Shape::hexagon(),
    Shape::octagon(),
    Shape::star(),
    Shape::cross(),
    Shape::arrow(ArrowForm::Simple, Direction::Right),
    Shape::arrow(ArrowForm::Simple, Direction::Up),
    Shape::arrow(ArrowForm::Double, Direction::Right),
    Shape::arrow(ArrowForm::Chevron, Direction::Right),
    Shape::arrow(ArrowForm::Notched, Direction::Right),
    Shape::arrow(ArrowForm::Bent, Direction::Right),
    Shape::speech_bubble(),
    Shape::cylinder(),
    Shape::Cloud,
    Shape::Heart,
    Shape::arc(),
    Shape::wedge(),
    // Flowchart
    Shape::Terminator,
    Shape::document(),
    Shape::multi_document(),
    Shape::manual_input(),
    Shape::manual_operation(),
    Shape::predefined_process(),
    Shape::internal_storage(),
    Shape::direct_data(),
    Shape::stored_data(),
    Shape::Delay,
    Shape::display(),
    Shape::flowchart_merge(),
    Shape::Or,
    Shape::SummingJunction,
    Shape::off_page_connector(),
    Shape::database(),
];

// ---------------------------------------------------------------------------
// Construction helpers. Everything below is private: the only geometry the crate
// exposes is an `Outline`.
// ---------------------------------------------------------------------------

/// Builds a closed contour, with the closing edge back to `start` implied.
fn closed(start: Point, build: impl FnOnce(&mut ContourBuilder)) -> Contour {
    let mut builder = Contour::builder(start);
    build(&mut builder);
    builder.close()
}

/// Builds an open contour, for interior detail strokes.
fn open(start: Point, build: impl FnOnce(&mut ContourBuilder)) -> Contour {
    let mut builder = Contour::builder(start);
    build(&mut builder);
    builder.end_open()
}

/// A point on an ellipse, at `degrees` clockwise from east.
fn polar(centre: Point, rx: f32, ry: f32, degrees: f32) -> Point {
    let a = degrees.to_radians();
    p(centre.x + rx * a.cos(), centre.y + ry * a.sin())
}

/// Splits a radius given in units of the box's **height** into the x and y radii
/// that keep it circular once the unit box is stretched to `aspect`.
fn circular_radii(radius: f32, aspect: f32) -> (f32, f32) {
    ((radius / aspect).clamp(0.0, 0.5), radius.clamp(0.0, 0.5))
}

/// Turns the unit box a quarter turn clockwise, `turns` times. Rotation maps the
/// square onto itself, so bounds and winding are preserved exactly.
fn rotated(outline: Outline, turns: u8) -> Outline {
    (0..turns % 4).fold(outline, |o, _| o.map(|q| p(1.0 - q.y, q.x)))
}

fn rounded_rect_contour(rx: f32, ry: f32, top: f32, bottom: f32) -> Contour {
    closed(p(rx, top), |b| {
        b.line_to(p(1.0 - rx, top))
            .arc(p(1.0 - rx, top + ry), rx, ry, -90.0, 0.0)
            .line_to(p(1.0, bottom - ry))
            .arc(p(1.0 - rx, bottom - ry), rx, ry, 0.0, 90.0)
            .line_to(p(rx, bottom))
            .arc(p(rx, bottom - ry), rx, ry, 90.0, 180.0)
            .line_to(p(0.0, top + ry))
            .arc(p(rx, top + ry), rx, ry, 180.0, 270.0);
    })
}

fn ellipse_contour(centre: Point, rx: f32, ry: f32) -> Contour {
    closed(polar(centre, rx, ry, 0.0), |b| {
        b.arc(centre, rx, ry, 0.0, 360.0);
    })
}

fn regular_polygon_points(sides: u32, rotation_deg: f32) -> Vec<Point> {
    let n = sides.max(3);
    (0..n)
        .map(|i| polar(p(0.5, 0.5), 0.5, 0.5, rotation_deg + 360.0 * i as f32 / n as f32))
        .collect()
}

fn star_points(points: u32, inner_ratio: f32) -> Vec<Point> {
    let n = points.max(3);
    let inner = 0.5 * inner_ratio.clamp(0.05, 0.95);
    (0..2 * n)
        .map(|i| {
            let radius = if i % 2 == 0 { 0.5 } else { inner };
            polar(p(0.5, 0.5), radius, radius, -90.0 + 180.0 * i as f32 / n as f32)
        })
        .collect()
}

fn arrow_outline(form: ArrowForm, shaft: f32, head: f32) -> Outline {
    let head = head.clamp(0.05, 0.95);
    let shaft = shaft.clamp(0.05, 1.0);
    let edge = (1.0 - shaft) * 0.5;
    match form {
        ArrowForm::Simple => Outline::polygon(&[
            p(0.0, edge),
            p(1.0 - head, edge),
            p(1.0 - head, 0.0),
            p(1.0, 0.5),
            p(1.0 - head, 1.0),
            p(1.0 - head, 1.0 - edge),
            p(0.0, 1.0 - edge),
        ]),
        ArrowForm::Double => {
            let head = head.min(0.49);
            Outline::polygon(&[
                p(0.0, 0.5),
                p(head, 0.0),
                p(head, edge),
                p(1.0 - head, edge),
                p(1.0 - head, 0.0),
                p(1.0, 0.5),
                p(1.0 - head, 1.0),
                p(1.0 - head, 1.0 - edge),
                p(head, 1.0 - edge),
                p(head, 1.0),
            ])
        }
        ArrowForm::Chevron => Outline::polygon(&[
            p(0.0, 0.0),
            p(1.0 - head, 0.0),
            p(1.0, 0.5),
            p(1.0 - head, 1.0),
            p(0.0, 1.0),
        ]),
        ArrowForm::Notched => Outline::polygon(&[
            p(0.0, 0.0),
            p(1.0 - head, 0.0),
            p(1.0, 0.5),
            p(1.0 - head, 1.0),
            p(0.0, 1.0),
            p(head, 0.5),
        ]),
        ArrowForm::Bent => {
            // The head is twice the arm thick, so it reads as a head; that fixes
            // the arm's position at half its own thickness below the top edge.
            let arm = shaft.min(0.45);
            Outline::polygon(&[
                p(0.0, 1.0),
                p(0.0, arm * 0.5),
                p(1.0 - head, arm * 0.5),
                p(1.0 - head, 0.0),
                p(1.0, arm),
                p(1.0 - head, arm * 2.0),
                p(1.0 - head, arm * 1.5),
                p(arm, arm * 1.5),
                p(arm, 1.0),
            ])
        }
    }
}

fn speech_bubble_contour(rx: f32, ry: f32, body: f32) -> Contour {
    closed(p(rx, 0.0), |b| {
        b.line_to(p(1.0 - rx, 0.0))
            .arc(p(1.0 - rx, ry), rx, ry, -90.0, 0.0)
            .line_to(p(1.0, body - ry))
            .arc(p(1.0 - rx, body - ry), rx, ry, 0.0, 90.0)
            .line_to(p(0.42, body))
            .line_to(p(0.18, 1.0))
            .line_to(p(0.24, body))
            .line_to(p(rx, body))
            .arc(p(rx, body - ry), rx, ry, 90.0, 180.0)
            .line_to(p(0.0, ry))
            .arc(p(rx, ry), rx, ry, 180.0, 270.0);
    })
}

/// The silhouette of an upright can, plus the front rim of its top ellipse — the
/// one line that makes it read as a cylinder rather than a rounded rectangle.
fn cylinder_outline(cap: f32) -> Outline {
    let c = cap.clamp(0.02, 0.45);
    let (top, bottom) = (c, 1.0 - c);
    let silhouette = closed(p(0.0, top), |b| {
        b.arc(p(0.5, top), 0.5, c, 180.0, 360.0)
            .line_to(p(1.0, bottom))
            .arc(p(0.5, bottom), 0.5, c, 0.0, 180.0);
    });
    let rim = open(p(0.0, top), |b| {
        b.arc(p(0.5, top), 0.5, c, 180.0, 0.0);
    });
    Outline::from_contour(silhouette).with_details(vec![rim])
}

/// A rectangle whose bottom edge is one symmetric wave: both ends at the same
/// height, one control point pulling the curve below them and the other pulling it
/// above by the same amount. The outline is normalised afterwards, so the wave's
/// trough lands exactly on the bottom of the box rather than somewhere short of it.
fn document_contour(wave: f32) -> Contour {
    let w = wave.clamp(0.02, 0.45);
    closed(p(0.0, 0.0), |b| {
        b.line_to(p(1.0, 0.0))
            .line_to(p(1.0, 1.0 - w))
            .cubic_to(p(0.75, 1.0), p(0.25, 1.0 - 2.0 * w), p(0.0, 1.0 - w));
    })
}

/// Lobe radii, starting from the top and going clockwise. Deliberately uneven and
/// larger above than below: a cloud with equal lobes reads as a cog.
///
/// Each must be large enough to overlap its neighbours on the ring below — around
/// two thirds of the centre spacing — or the union stops being one shape.
const CLOUD_LOBES: [f32; 9] = [0.145, 0.115, 0.130, 0.110, 0.120, 0.110, 0.130, 0.120, 0.140];

/// The ellipse the lobe centres sit on.
const CLOUD_RING: (f32, f32) = (0.30, 0.18);

/// A cloud is the boundary of a union of overlapping discs: each lobe contributes
/// the outer arc between its intersections with its two neighbours, and the cusp
/// where two arcs meet is what makes the lobes read as lobes. Drawing a smooth bump
/// between fixed valley points instead — the obvious approach — gives a blob,
/// because the valleys have to be cut deep before they show, and by then the shape
/// has pinched.
fn cloud_contour() -> Contour {
    let centre = p(0.5, 0.5);
    let n = CLOUD_LOBES.len();
    let lobe = |i: usize| polar(centre, CLOUD_RING.0, CLOUD_RING.1, -90.0 + 360.0 * i as f32 / n as f32);
    // Where lobe `i` meets lobe `i + 1`, on the outside.
    let joint = |i: usize| {
        outer_intersection(lobe(i), CLOUD_LOBES[i], lobe((i + 1) % n), CLOUD_LOBES[(i + 1) % n], centre)
    };
    let bearing = |from: Point, to: Point| (to - from).y.atan2((to - from).x).to_degrees();
    closed(joint(n - 1), |b| {
        for (i, &radius) in CLOUD_LOBES.iter().enumerate() {
            let (entry, exit) = (joint((i + n - 1) % n), joint(i));
            let start = bearing(lobe(i), entry);
            // Clockwise from where the previous lobe left off to where the next one
            // takes over, which is always the arc on the outside of the union.
            let sweep = (bearing(lobe(i), exit) - start).rem_euclid(360.0);
            b.arc(lobe(i), radius, radius, start, start + sweep);
        }
    })
}

/// The intersection of two overlapping circles that lies further from `inside`.
///
/// Circles that do not reach each other yield the point on the line between them,
/// which keeps a mistyped lobe radius as a visibly wrong cloud rather than a `NaN`
/// that takes the tessellator down with it.
fn outer_intersection(c0: Point, r0: f32, c1: Point, r1: f32, inside: Point) -> Point {
    let axis = c1 - c0;
    let distance = axis.length();
    if distance <= f32::EPSILON {
        return c0;
    }
    let along = ((distance * distance + r0 * r0 - r1 * r1) / (2.0 * distance)).clamp(-r0, r0);
    let across = (r0 * r0 - along * along).max(0.0).sqrt();
    let base = c0 + axis * (along / distance);
    let offset = p(-axis.y, axis.x) * (across / distance);
    let (a, b) = (base + offset, base - offset);
    if a.distance(inside) >= b.distance(inside) { a } else { b }
}

/// Bottom point, up the left lobe, down into the notch, up the right lobe. That is
/// clockwise on screen — from six o'clock the clockwise neighbour is nine o'clock,
/// on the left — so the winding matches every other silhouette.
fn heart_contour() -> Contour {
    closed(p(0.5, 1.0), |b| {
        b.cubic_to(p(0.20, 0.78), p(0.0, 0.56), p(0.0, 0.36))
            .cubic_to(p(0.0, 0.16), p(0.14, 0.0), p(0.30, 0.0))
            .cubic_to(p(0.40, 0.0), p(0.47, 0.07), p(0.50, 0.16))
            .cubic_to(p(0.53, 0.07), p(0.60, 0.0), p(0.70, 0.0))
            .cubic_to(p(0.86, 0.0), p(1.0, 0.16), p(1.0, 0.36))
            .cubic_to(p(1.0, 0.56), p(0.80, 0.78), p(0.5, 1.0));
    })
}

/// Clamps a sweep to a single positive turn, flipping a negative one onto its
/// equivalent forward sweep so every contour still comes out clockwise.
fn normalise_sweep(start_deg: f32, sweep_deg: f32) -> (f32, f32) {
    let sweep = sweep_deg.clamp(-360.0, 360.0);
    if sweep < 0.0 { (start_deg + sweep, -sweep) } else { (start_deg, sweep.max(1.0)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_polygons_fill_the_box_and_start_where_they_are_told() {
        let triangle = Shape::triangle().outline(1.0);
        let corners = triangle.as_polygon().unwrap();
        assert_eq!(corners.len(), 3);
        assert!(corners[0].distance(p(0.5, 0.0)) < 1e-5, "apex at the top: {corners:?}");
        // A hexagon has a vertex east and west, and flat edges top and bottom.
        let hexagon = Shape::hexagon().outline(1.0).as_polygon().unwrap();
        assert!(hexagon[0].distance(p(1.0, 0.5)) < 1e-5, "{hexagon:?}");
    }

    #[test]
    fn a_stretched_rounded_rectangle_keeps_circular_corners() {
        // In a 4:1 box the corner must be four times narrower in unit x than in
        // unit y for it to come out circular on screen.
        let (rx, ry) = circular_radii(corner_radius(0.25, 1.0_f32.min(4.0)), 4.0);
        assert!((ry / rx - 4.0).abs() < 1e-4, "rx {rx}, ry {ry}");
    }

    #[test]
    fn rotating_an_arrow_moves_its_tip_but_not_its_bounds() {
        let right = Shape::arrow(ArrowForm::Simple, Direction::Right).outline(1.0);
        let down = Shape::arrow(ArrowForm::Simple, Direction::Down).outline(1.0);
        assert!(right.contains(p(0.9, 0.5)) && !right.contains(p(0.5, 0.9)));
        assert!(down.contains(p(0.5, 0.9)) && !down.contains(p(0.9, 0.5)));
        assert_eq!(right.bounds(), down.bounds());
    }

    #[test]
    fn a_negative_sweep_becomes_the_same_arc_traced_forwards() {
        assert_eq!(normalise_sweep(90.0, -60.0), (30.0, 60.0));
        assert_eq!(normalise_sweep(90.0, 60.0), (90.0, 60.0));
    }

    #[test]
    fn parameters_are_clamped_rather_than_rejected() {
        let absurd = Shape::Star { points: 0, inner_ratio: -5.0 };
        assert_eq!(absurd.outline(1.0).as_polygon().unwrap().len(), 6);
        let bad_aspect = Shape::rounded_rectangle().outline(f32::NAN);
        assert_eq!(bad_aspect.bounds(), Bounds::UNIT);
    }

    #[test]
    fn flowchart_aliases_are_the_shape_they_claim_to_be() {
        assert_eq!(Shape::flowchart_process(), Shape::Rectangle);
        assert_eq!(Shape::flowchart_decision(), Shape::Diamond);
        assert_eq!(Shape::flowchart_preparation(), Shape::hexagon());
        let merge = Shape::flowchart_merge().outline(1.0).as_polygon().unwrap();
        assert!(merge[0].distance(p(0.5, 1.0)) < 1e-5, "the point is at the bottom: {merge:?}");
    }

    #[test]
    fn every_catalogue_entry_has_a_name() {
        for shape in CATALOGUE {
            assert!(!shape.name().is_empty(), "{shape:?}");
        }
    }
}
