//! Tessellation: a routed path and its style, in, an indexed triangle mesh out.
//!
//! No GPU code lives here. The output is plain buffers that a renderer uploads
//! however it likes, which is also what makes every case in this module testable
//! with `cargo test` on a machine with no graphics stack.
//!
//! ## Vertices are relative to an origin
//!
//! `docs/01-architecture.md` requires `f64` world space rebased to camera-relative
//! `f32` for the GPU, because the reference board is already 41282 × 17515px and
//! `f32` quantises coordinates that large coarsely enough to shimmer at deep zoom.
//! A mesh emitted in absolute world coordinates would have thrown that precision
//! away before the renderer ever saw it, so [`Mesh::origin`] carries the `f64`
//! anchor and the vertices are small offsets from it.
//!
//! ## Colour is not here
//!
//! A connector is one colour, so colour belongs to the draw call rather than to
//! every vertex. Keeping it out halves the vertex size and means a recolour does not
//! re-tessellate.

use lyon::path::Path;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, LineCap, LineJoin, StrokeOptions,
    StrokeTessellator, StrokeVertex, VertexBuffers,
};

use crate::arrow::{ArrowDraw, ArrowheadGeometry, arrowhead};
use crate::geometry::{EPSILON, Point, Polyline, Rect};
use crate::route::{ConnectError, RoutedPath};
use crate::style::ConnectorStyle;

/// An indexed triangle mesh in a local frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    /// World-space position that vertex `(0, 0)` corresponds to.
    pub origin: Point,
    /// Positions relative to [`Mesh::origin`].
    pub vertices: Vec<[f32; 2]>,
    /// Triangle list, three indices per triangle.
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// World-space bounds of the tessellated geometry, or `None` when nothing was
    /// generated.
    pub fn bounds(&self) -> Option<Rect> {
        Rect::from_points(self.vertices.iter().map(|v| {
            Point::new(self.origin.x + f64::from(v[0]), self.origin.y + f64::from(v[1]))
        }))
    }
}

/// Tessellation quality.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TessellationOptions {
    /// Maximum deviation, in world px, between a curve and the triangles standing in
    /// for it.
    pub tolerance: f64,
}

impl Default for TessellationOptions {
    fn default() -> Self {
        Self { tolerance: crate::route::DEFAULT_TOLERANCE }
    }
}

/// Tessellates a routed connector: the line, its dashes, and both arrowheads.
pub fn tessellate(
    path: &RoutedPath,
    style: &ConnectorStyle,
    options: &TessellationOptions,
) -> Result<Mesh, ConnectError> {
    let thickness = style.thickness.max(0.0);
    let origin = path.start;
    let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();

    let heads = [
        // The head at the start points back the way the path came, hence the
        // reversed tangent; the one at the end points the way it was going.
        path.start_tangent()
            .and_then(|t| arrowhead(style.start_arrow, path.start, -t, thickness)),
        path.end_tangent().and_then(|t| arrowhead(style.end_arrow, path.end(), t, thickness)),
    ];

    let line = path.flatten(options.tolerance).trimmed(
        heads[0].as_ref().map_or(0.0, |h| h.trim),
        heads[1].as_ref().map_or(0.0, |h| h.trim),
    );

    if thickness > EPSILON {
        let runs = match style.line.dash_pattern(thickness) {
            Some(pattern) => pattern.split(&line),
            None => vec![line],
        };
        for run in &runs {
            stroke_polyline(run, thickness, style, origin, options, &mut buffers)?;
        }
        for head in heads.iter().flatten() {
            tessellate_head(head, thickness, origin, options, &mut buffers)?;
        }
    }

    Ok(Mesh { origin, vertices: buffers.vertices, indices: buffers.indices })
}

fn stroke_polyline(
    line: &Polyline,
    thickness: f64,
    style: &ConnectorStyle,
    origin: Point,
    options: &TessellationOptions,
    buffers: &mut VertexBuffers<[f32; 2], u32>,
) -> Result<(), ConnectError> {
    // A single point has no direction and therefore no stroke. That is the
    // zero-length connector, which renders as nothing rather than as a panic.
    if line.len() < 2 {
        return Ok(());
    }
    let mut builder = Path::builder();
    builder.begin(local(line.points[0], origin));
    for p in &line.points[1..] {
        builder.line_to(local(*p, origin));
    }
    builder.end(false);

    // Butt caps on a solid line, matching Miro: its export writes
    // `stroke-linecap="butt"` on every connector. Dashes get round caps, because a
    // butt-capped dot is a square.
    let cap = if style.line.dash_pattern(thickness).is_some() {
        LineCap::Round
    } else {
        LineCap::Butt
    };

    StrokeTessellator::new().tessellate_path(
        &builder.build(),
        &StrokeOptions::default()
            .with_line_width(thickness as f32)
            .with_line_cap(cap)
            // Round joins: a mitre on the near-180° reversal an elbow route can
            // produce shoots off to an arbitrary distance.
            .with_line_join(LineJoin::Round)
            .with_tolerance(options.tolerance.max(EPSILON) as f32),
        &mut BuffersBuilder::new(buffers, |v: StrokeVertex| v.position().to_array()),
    )?;
    Ok(())
}

fn tessellate_head(
    head: &ArrowheadGeometry,
    thickness: f64,
    origin: Point,
    options: &TessellationOptions,
    buffers: &mut VertexBuffers<[f32; 2], u32>,
) -> Result<(), ConnectError> {
    if head.vertices.len() < 2 {
        return Ok(());
    }
    let closed = head.draw != ArrowDraw::OpenStroke;
    let mut builder = Path::builder();
    builder.begin(local(head.vertices[0], origin));
    for p in &head.vertices[1..] {
        builder.line_to(local(*p, origin));
    }
    builder.end(closed);
    let path = builder.build();
    let tolerance = options.tolerance.max(EPSILON) as f32;

    match head.draw {
        ArrowDraw::Filled => {
            FillTessellator::new().tessellate_path(
                &path,
                &FillOptions::default().with_tolerance(tolerance),
                &mut BuffersBuilder::new(buffers, |v: FillVertex| v.position().to_array()),
            )?;
        }
        ArrowDraw::ClosedStroke | ArrowDraw::OpenStroke => {
            StrokeTessellator::new().tessellate_path(
                &path,
                &StrokeOptions::default()
                    .with_line_width(thickness as f32)
                    .with_line_cap(LineCap::Butt)
                    .with_line_join(LineJoin::Miter)
                    .with_tolerance(tolerance),
                &mut BuffersBuilder::new(buffers, |v: StrokeVertex| v.position().to_array()),
            )?;
        }
    }
    Ok(())
}

/// World point to the mesh's local `f32` frame.
fn local(p: Point, origin: Point) -> lyon::math::Point {
    lyon::math::point((p.x - origin.x) as f32, (p.y - origin.y) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::ObstacleAvoidance;
    use crate::style::{Arrowhead, LineStyle, RoutingMode};

    fn straight(length: f64) -> RoutedPath {
        RoutedPath::from_points(
            [Point::ORIGIN, Point::new(length, 0.0)],
            ObstacleAvoidance::NotAttempted,
        )
    }

    fn plain() -> ConnectorStyle {
        ConnectorStyle::default()
    }

    #[test]
    fn a_plain_line_tessellates_to_a_quad() {
        let mesh = tessellate(&straight(100.0), &plain(), &TessellationOptions::default()).unwrap();
        assert_eq!(mesh.triangle_count(), 2, "{mesh:?}");
        assert_eq!(mesh.origin, Point::ORIGIN);
        assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.vertices.len()));
    }

    #[test]
    fn the_mesh_is_as_thick_as_the_style_says() {
        for thickness in [1.0, 2.0, 9.5] {
            let style = ConnectorStyle { thickness, ..plain() };
            let mesh = tessellate(&straight(100.0), &style, &TessellationOptions::default())
                .unwrap();
            let bounds = mesh.bounds().unwrap();
            assert!((bounds.height() - thickness).abs() < 1e-4, "{thickness}: {bounds:?}");
            assert!((bounds.width() - 100.0).abs() < 1e-4);
        }
    }

    /// Vertices are offsets from `origin`, not absolute world coordinates — the
    /// property that keeps deep zoom stable on a board tens of thousands of px wide.
    #[test]
    fn vertices_are_local_to_the_meshs_origin() {
        let far = RoutedPath::from_points(
            [Point::new(41_000.0, -17_000.0), Point::new(41_100.0, -17_000.0)],
            ObstacleAvoidance::NotAttempted,
        );
        let mesh = tessellate(&far, &plain(), &TessellationOptions::default()).unwrap();

        assert_eq!(mesh.origin, Point::new(41_000.0, -17_000.0));
        assert!(mesh.vertices.iter().all(|v| v[0].abs() <= 101.0 && v[1].abs() <= 2.0), "{mesh:?}");
        // And it still lands in the right place once the origin is reapplied.
        let bounds = mesh.bounds().unwrap();
        assert!((bounds.min.x - 41_000.0).abs() < 1e-3, "{bounds:?}");
    }

    #[test]
    fn a_dashed_line_produces_more_pieces_than_a_solid_one() {
        let solid = tessellate(&straight(200.0), &plain(), &TessellationOptions::default())
            .unwrap();
        let dashed = tessellate(
            &straight(200.0),
            &ConnectorStyle { line: LineStyle::Dashed, ..plain() },
            &TessellationOptions::default(),
        )
        .unwrap();

        assert!(dashed.triangle_count() > solid.triangle_count(), "{dashed:?}");
        // Dashes cover less of the line but still reach both ends. They overshoot by
        // exactly the round caps the solid line does not have.
        let (a, b) = (solid.bounds().unwrap(), dashed.bounds().unwrap());
        assert!(b.min.x <= a.min.x + 1e-3 && b.max.x >= a.max.x - 1e-3, "{b:?} vs {a:?}");
        assert!(b.width() <= a.width() + plain().thickness + 1e-3, "{b:?} vs {a:?}");
    }

    #[test]
    fn a_dotted_line_is_denser_still() {
        let dashed = tessellate(
            &straight(200.0),
            &ConnectorStyle { line: LineStyle::Dashed, ..plain() },
            &TessellationOptions::default(),
        )
        .unwrap();
        let dotted = tessellate(
            &straight(200.0),
            &ConnectorStyle { line: LineStyle::Dotted, ..plain() },
            &TessellationOptions::default(),
        )
        .unwrap();
        assert!(dotted.triangle_count() > dashed.triangle_count());
    }

    #[test]
    fn an_arrowhead_adds_geometry_and_shortens_the_line() {
        let bare = tessellate(&straight(100.0), &plain(), &TessellationOptions::default())
            .unwrap();
        let tipped = tessellate(
            &straight(100.0),
            &ConnectorStyle { end_arrow: Arrowhead::FilledTriangle, ..plain() },
            &TessellationOptions::default(),
        )
        .unwrap();

        assert!(tipped.triangle_count() > bare.triangle_count());
        // The head still reaches the endpoint even though the line stops short.
        let bounds = tipped.bounds().unwrap();
        assert!((bounds.max.x - 100.0).abs() < 1e-3, "{bounds:?}");
        // And it is wider than the line, so the head is really there.
        assert!(bounds.height() > 2.0 * 3.0, "{bounds:?}");
    }

    #[test]
    fn arrowheads_at_both_ends_both_appear() {
        let style = ConnectorStyle {
            start_arrow: Arrowhead::FilledTriangle,
            end_arrow: Arrowhead::FilledTriangle,
            ..plain()
        };
        let mesh = tessellate(&straight(100.0), &style, &TessellationOptions::default()).unwrap();
        let bounds = mesh.bounds().unwrap();
        assert!((bounds.min.x - 0.0).abs() < 1e-3 && (bounds.max.x - 100.0).abs() < 1e-3);
        assert!(bounds.height() > 6.0, "{bounds:?}");
    }

    #[test]
    fn every_arrowhead_form_tessellates() {
        for kind in [
            Arrowhead::LineArrow,
            Arrowhead::FilledTriangle,
            Arrowhead::OpenTriangle,
            Arrowhead::Circle,
            Arrowhead::FilledCircle,
            Arrowhead::Diamond,
            Arrowhead::FilledDiamond,
        ] {
            let style = ConnectorStyle { end_arrow: kind, ..plain() };
            let mesh = tessellate(&straight(100.0), &style, &TessellationOptions::default())
                .unwrap();
            assert!(!mesh.is_empty(), "{kind:?}");
            assert!(mesh.vertices.iter().all(|v| v[0].is_finite() && v[1].is_finite()), "{kind:?}");
        }
    }

    /// A head whose trim exceeds the line's own length must not produce an inverted
    /// stub: the head is drawn, the line is not.
    #[test]
    fn a_connector_shorter_than_its_arrowhead_still_renders() {
        let style = ConnectorStyle {
            end_arrow: Arrowhead::FilledTriangle,
            thickness: 8.0,
            ..plain()
        };
        let mesh = tessellate(&straight(4.0), &style, &TessellationOptions::default()).unwrap();
        assert!(!mesh.is_empty());
        let bounds = mesh.bounds().unwrap();
        assert!(bounds.width() > 0.0 && bounds.height() > 0.0, "{bounds:?}");
    }

    #[test]
    fn a_zero_length_connector_tessellates_to_nothing_rather_than_panicking() {
        let mesh = tessellate(
            &RoutedPath::degenerate(Point::new(7.0, -3.0)),
            &ConnectorStyle { end_arrow: Arrowhead::FilledTriangle, ..plain() },
            &TessellationOptions::default(),
        )
        .unwrap();
        assert!(mesh.is_empty(), "{mesh:?}");
        assert_eq!(mesh.origin, Point::new(7.0, -3.0));
    }

    #[test]
    fn a_zero_thickness_connector_tessellates_to_nothing() {
        let mesh = tessellate(
            &straight(100.0),
            &ConnectorStyle { thickness: 0.0, ..plain() },
            &TessellationOptions::default(),
        )
        .unwrap();
        assert!(mesh.is_empty());
    }

    #[test]
    fn a_curved_connector_tessellates_along_its_curve() {
        let router = crate::Router::default();
        let start = crate::ResolvedEndpoint {
            point: Point::ORIGIN,
            normal: Some(crate::Vec2::new(0.0, -1.0)),
            bounds: None,
        };
        let end = crate::ResolvedEndpoint {
            point: Point::new(200.0, 0.0),
            normal: Some(crate::Vec2::new(0.0, -1.0)),
            bounds: None,
        };
        let path = router.route_resolved(&start, &end, RoutingMode::Curved, &[]);
        let mesh = tessellate(&path, &plain(), &TessellationOptions::default()).unwrap();

        // The bulge is real: the mesh is far taller than the 2px line.
        let bounds = mesh.bounds().unwrap();
        assert!(bounds.height() > 40.0, "{bounds:?}");
        assert!(mesh.triangle_count() > 10);
    }

    #[test]
    fn indices_are_always_in_range_across_multiple_sub_paths() {
        let style = ConnectorStyle {
            line: LineStyle::Dashed,
            start_arrow: Arrowhead::Circle,
            end_arrow: Arrowhead::FilledDiamond,
            ..plain()
        };
        let path = RoutedPath::from_points(
            [
                Point::new(0.0, 0.0),
                Point::new(120.0, 0.0),
                Point::new(120.0, 90.0),
            ],
            ObstacleAvoidance::Cleared,
        );
        let mesh = tessellate(&path, &style, &TessellationOptions::default()).unwrap();
        assert_eq!(mesh.indices.len() % 3, 0);
        assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.vertices.len()));
        assert!(mesh.triangle_count() > 4);
    }
}
