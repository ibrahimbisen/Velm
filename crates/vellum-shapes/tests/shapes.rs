//! Every shape in the catalogue, checked the same way.
//!
//! The table below is the point of this file. Each shape gets a point that must be
//! **inside** it and a point that must be inside its bounding box but **outside the
//! shape** — the probe that a bounding-box hit test would get wrong. That second
//! column is what stops a concave shape from silently degrading into its box: a
//! star, a cross, a notched arrow and a stored-data symbol all pass a box test in
//! places where the user can plainly see empty space.
//!
//! A shape with no entry here fails `every_catalogue_shape_is_probed`, so a new
//! shape cannot arrive untested.

use vellum_shapes::mesh::TessellationOptions;
use vellum_shapes::{ArrowForm, Bounds, CATALOGUE, Direction, Point, Shape, Size, p};

struct Probe {
    shape: Shape,
    /// Must be inside the shape.
    inside: Point,
    /// Must be inside the bounding box and outside the shape. `None` only for the
    /// shapes that genuinely fill their box, where no such point exists.
    outside: Option<Point>,
    /// Interior detail lines the shape carries — the flowchart rules and rims that
    /// are stroked but never filled.
    details: usize,
}

const fn probe(shape: Shape, inside: Point, outside: Option<Point>) -> Probe {
    Probe { shape, inside, outside, details: 0 }
}

const fn decorated(shape: Shape, inside: Point, outside: Option<Point>, details: usize) -> Probe {
    Probe { shape, inside, outside, details }
}

#[rustfmt::skip]
const PROBES: &[Probe] = &[
    // Basic shapes.
    probe(Shape::Rectangle, p(0.5, 0.5), None),
    probe(Shape::rounded_rectangle(), p(0.5, 0.5), Some(p(0.005, 0.005))),
    probe(Shape::Ellipse, p(0.5, 0.5), Some(p(0.02, 0.02))),
    probe(Shape::triangle(), p(0.5, 0.8), Some(p(0.05, 0.05))),
    probe(Shape::RightTriangle, p(0.2, 0.8), Some(p(0.9, 0.1))),
    probe(Shape::Diamond, p(0.5, 0.5), Some(p(0.05, 0.05))),
    probe(Shape::parallelogram(), p(0.5, 0.5), Some(p(0.05, 0.05))),
    probe(Shape::trapezoid(), p(0.5, 0.5), Some(p(0.05, 0.05))),
    probe(Shape::pentagon(), p(0.5, 0.6), Some(p(0.02, 0.02))),
    probe(Shape::hexagon(), p(0.5, 0.5), Some(p(0.02, 0.02))),
    probe(Shape::octagon(), p(0.5, 0.5), Some(p(0.01, 0.01))),
    probe(Shape::star(), p(0.5, 0.5), Some(p(0.05, 0.05))),
    probe(Shape::cross(), p(0.5, 0.5), Some(p(0.05, 0.05))),
    probe(Shape::arrow(ArrowForm::Simple, Direction::Right), p(0.3, 0.5), Some(p(0.05, 0.05))),
    probe(Shape::arrow(ArrowForm::Simple, Direction::Up), p(0.5, 0.7), Some(p(0.05, 0.95))),
    probe(Shape::arrow(ArrowForm::Double, Direction::Right), p(0.5, 0.5), Some(p(0.02, 0.02))),
    probe(Shape::arrow(ArrowForm::Chevron, Direction::Right), p(0.3, 0.5), Some(p(0.98, 0.02))),
    probe(Shape::arrow(ArrowForm::Notched, Direction::Right), p(0.5, 0.5), Some(p(0.02, 0.5))),
    probe(Shape::arrow(ArrowForm::Bent, Direction::Right), p(0.16, 0.8), Some(p(0.8, 0.8))),
    probe(Shape::speech_bubble(), p(0.5, 0.4), Some(p(0.9, 0.95))),
    decorated(Shape::cylinder(), p(0.5, 0.5), Some(p(0.02, 0.02)), 1),
    probe(Shape::Cloud, p(0.5, 0.5), Some(p(0.02, 0.02))),
    probe(Shape::Heart, p(0.5, 0.5), Some(p(0.5, 0.02))),
    probe(Shape::arc(), p(0.5, 0.2), Some(p(0.5, 0.9))),
    probe(Shape::wedge(), p(0.3, 0.7), Some(p(0.8, 0.2))),

    // Flowchart symbols.
    probe(Shape::Terminator, p(0.5, 0.5), Some(p(0.02, 0.02))),
    probe(Shape::document(), p(0.5, 0.5), Some(p(0.98, 0.98))),
    probe(Shape::multi_document(), p(0.4, 0.6), Some(p(0.02, 0.02))),
    probe(Shape::manual_input(), p(0.5, 0.5), Some(p(0.02, 0.02))),
    probe(Shape::manual_operation(), p(0.5, 0.5), Some(p(0.02, 0.98))),
    decorated(Shape::predefined_process(), p(0.5, 0.5), None, 2),
    decorated(Shape::internal_storage(), p(0.5, 0.5), None, 2),
    decorated(Shape::direct_data(), p(0.5, 0.5), Some(p(0.02, 0.02)), 1),
    probe(Shape::stored_data(), p(0.5, 0.5), Some(p(0.02, 0.5))),
    probe(Shape::Delay, p(0.5, 0.5), Some(p(0.95, 0.05))),
    probe(Shape::display(), p(0.5, 0.5), Some(p(0.02, 0.02))),
    probe(Shape::flowchart_merge(), p(0.5, 0.3), Some(p(0.05, 0.95))),
    decorated(Shape::Or, p(0.5, 0.5), Some(p(0.02, 0.02)), 2),
    decorated(Shape::SummingJunction, p(0.5, 0.5), Some(p(0.02, 0.02)), 2),
    probe(Shape::off_page_connector(), p(0.5, 0.5), Some(p(0.02, 0.98))),
    decorated(Shape::database(), p(0.5, 0.5), Some(p(0.02, 0.02)), 3),
];

/// Aspect ratios every shape is checked at: a tall box, a square, and a wide one.
/// The handful of shapes that read the aspect ratio must still fill their box and
/// still contain their interior probe at all three.
const ASPECTS: [f32; 3] = [0.4, 1.0, 3.0];

#[test]
fn every_catalogue_shape_is_probed() {
    for shape in CATALOGUE {
        assert!(
            PROBES.iter().any(|probe| probe.shape == *shape),
            "{} ({shape:?}) is in the catalogue but has no probes",
            shape.name()
        );
    }
}

#[test]
fn every_probe_is_for_a_catalogued_shape() {
    for probe in PROBES {
        assert!(
            CATALOGUE.contains(&probe.shape),
            "{:?} is probed but not in the catalogue",
            probe.shape
        );
    }
}

/// A fill path with an unclosed sub-path is a bug: the tessellator would close it
/// implicitly along a straight line, silently drawing something else.
#[test]
fn every_silhouette_is_closed() {
    use lyon::path::PathEvent;
    for probe in PROBES {
        let outline = probe.shape.outline(1.0);
        assert!(!outline.contours.is_empty(), "{}", probe.shape.name());
        for contour in &outline.contours {
            assert!(contour.closed, "{}", probe.shape.name());
        }
        let mut ends = 0;
        for event in probe.shape.to_path(1.0).iter() {
            if let PathEvent::End { close, .. } = event {
                assert!(close, "{} has an open sub-path", probe.shape.name());
                ends += 1;
            }
        }
        assert_eq!(ends, outline.contours.len(), "{}", probe.shape.name());
    }
}

/// Shapes are normalised to fill the unit box, so a caller can map the box onto an
/// item rect and get exactly the size it asked for.
#[test]
fn every_shape_fills_its_box_at_every_aspect() {
    for probe in PROBES {
        for aspect in ASPECTS {
            let bounds = probe.shape.bounds(aspect);
            let tolerance = 1e-4;
            assert!(
                (bounds.min.x - Bounds::UNIT.min.x).abs() < tolerance
                    && (bounds.min.y - Bounds::UNIT.min.y).abs() < tolerance
                    && (bounds.max.x - Bounds::UNIT.max.x).abs() < tolerance
                    && (bounds.max.y - Bounds::UNIT.max.y).abs() < tolerance,
                "{} at aspect {aspect} has bounds {bounds:?}",
                probe.shape.name()
            );
        }
    }
}

/// The concave-shape test. The second probe is inside the bounding box, so a box
/// hit test would report a hit; the shape must not.
#[test]
fn hit_testing_follows_the_shape_not_its_box() {
    for probe in PROBES {
        let outline = probe.shape.outline(1.0);
        assert!(
            outline.contains(probe.inside),
            "{} should contain {:?}",
            probe.shape.name(),
            probe.inside
        );
        let Some(outside) = probe.outside else { continue };
        assert!(
            outline.bounds().contains(outside),
            "{}: the outside probe {outside:?} must be inside the bounding box, or it \
             proves nothing",
            probe.shape.name()
        );
        assert!(
            !outline.contains(outside),
            "{} wrongly contains {outside:?} — a bounding-box hit test would agree, \
             which is the bug this probe exists to catch",
            probe.shape.name()
        );
    }
}

#[test]
fn interior_probes_hold_at_every_aspect() {
    for probe in PROBES {
        for aspect in ASPECTS {
            assert!(
                probe.shape.contains(probe.inside, aspect),
                "{} at aspect {aspect} lost its interior point",
                probe.shape.name()
            );
        }
    }
}

/// Consistent winding is what makes overlapping contours — a multi-document stack —
/// fill as a union rather than punching holes in each other.
#[test]
fn every_contour_is_wound_clockwise_on_screen() {
    for probe in PROBES {
        for contour in &probe.shape.outline(1.0).contours {
            assert!(
                contour.signed_area() > 0.0,
                "{} has an anticlockwise contour",
                probe.shape.name()
            );
        }
    }
}

#[test]
fn decorated_shapes_carry_their_detail_lines() {
    for probe in PROBES {
        let outline = probe.shape.outline(1.0);
        assert_eq!(
            outline.details.len(),
            probe.details,
            "{} detail count",
            probe.shape.name()
        );
        // Detail lines decorate the interior, so they never escape the silhouette's
        // box, and they never change the hit test.
        for detail in &outline.details {
            let bounds = detail.bounds();
            assert!(
                outline.bounds().contains(bounds.min) && outline.bounds().contains(bounds.max),
                "{} has a detail line outside the shape",
                probe.shape.name()
            );
        }
    }
}

#[test]
fn every_shape_tessellates_into_a_usable_mesh() {
    for probe in PROBES {
        let mesh = probe
            .shape
            .tessellate(1.0, TessellationOptions::default())
            .unwrap_or_else(|e| panic!("{} failed to tessellate: {e}", probe.shape.name()));
        assert!(mesh.fill.triangle_count() > 0, "{}", probe.shape.name());
        assert!(!mesh.stroke.indices.is_empty(), "{}", probe.shape.name());
        assert!(
            mesh.fill.indices.iter().all(|&i| (i as usize) < mesh.fill.vertices.len()),
            "{} fill index out of range",
            probe.shape.name()
        );
        assert!(
            mesh.stroke.indices.iter().all(|&i| (i as usize) < mesh.stroke.vertices.len()),
            "{} stroke index out of range",
            probe.shape.name()
        );
        for v in &mesh.fill.vertices {
            assert!(
                v[0].is_finite() && v[1].is_finite(),
                "{} produced a non-finite fill vertex",
                probe.shape.name()
            );
        }
        // Shapes whose parameters collapse a segment to zero length — a terminator
        // at aspect 1 is all corner and no straight edge — are the ones that would
        // produce a degenerate join.
        for v in &mesh.stroke.vertices {
            assert!(
                v.position[0].is_finite()
                    && v.position[1].is_finite()
                    && v.normal[0].is_finite()
                    && v.normal[1].is_finite(),
                "{} produced a non-finite stroke vertex: {v:?}",
                probe.shape.name()
            );
        }
    }
}

/// A connector binds to a normalised point; if that point is not on the shape, the
/// connector ends in mid-air. Every anchor must land on the silhouette.
#[test]
fn every_anchor_lands_on_the_shape() {
    for probe in PROBES {
        for aspect in ASPECTS {
            let outline = probe.shape.outline(aspect);
            for anchor in outline.anchors() {
                let distance = outline.nearest_point(anchor.point).distance(anchor.point);
                assert!(
                    distance < 1e-3,
                    "{} anchor {} at aspect {aspect} is {distance} off the outline ({:?})",
                    probe.shape.name(),
                    anchor.kind.tag(),
                    anchor.point
                );
                // Anchors are normalised coordinates, so they belong in the box —
                // give or take the rounding of an intersection computed at a corner.
                let slack = 1e-4;
                assert!(
                    anchor.point.x >= -slack
                        && anchor.point.x <= 1.0 + slack
                        && anchor.point.y >= -slack
                        && anchor.point.y <= 1.0 + slack,
                    "{} anchor {} escaped the box at {:?}",
                    probe.shape.name(),
                    anchor.kind.tag(),
                    anchor.point
                );
            }
        }
    }
}

/// Anchors keep the side of the shape they were asked for: an east anchor must not
/// resolve to the west half. On a shape as concave as a star this is not automatic.
#[test]
fn anchors_stay_on_the_side_they_name() {
    for probe in PROBES {
        let outline = probe.shape.outline(1.0);
        for anchor in outline.anchors() {
            let requested = anchor.kind.box_point();
            let resolved = anchor.point;
            if requested.x > 0.5 {
                assert!(resolved.x >= 0.5 - 1e-3, "{} {:?}", probe.shape.name(), anchor);
            }
            if requested.x < 0.5 {
                assert!(resolved.x <= 0.5 + 1e-3, "{} {:?}", probe.shape.name(), anchor);
            }
            if requested.y > 0.5 {
                assert!(resolved.y >= 0.5 - 1e-3, "{} {:?}", probe.shape.name(), anchor);
            }
            if requested.y < 0.5 {
                assert!(resolved.y <= 0.5 + 1e-3, "{} {:?}", probe.shape.name(), anchor);
            }
        }
    }
}

/// The two output paths must agree about where the shape's edge is. A shader that
/// disagrees with the hit test by a few percent is the kind of bug that is only
/// ever found by eye, months later, and never reproduced.
#[test]
fn analytic_and_tessellated_silhouettes_agree() {
    const GRID: usize = 17;
    for probe in PROBES {
        for size in [Size::new(200.0, 200.0), Size::new(300.0, 120.0), Size::new(80.0, 260.0)] {
            let Some(sdf) = probe.shape.sdf_params(size) else { continue };
            let outline = probe.shape.outline(size.aspect());
            let skin = 0.02 * size.width.min(size.height);
            for row in 0..GRID {
                for column in 0..GRID {
                    let unit = p(column as f32 / (GRID - 1) as f32, row as f32 / (GRID - 1) as f32);
                    let local =
                        p((unit.x - 0.5) * size.width, (unit.y - 0.5) * size.height);
                    let distance = sdf.distance(local);
                    // Points within a hair of the edge are allowed to disagree:
                    // one side is a flattened polyline, the other is exact.
                    if distance.abs() < skin {
                        continue;
                    }
                    assert_eq!(
                        distance < 0.0,
                        outline.contains(unit),
                        "{} at {size:?}: the field says {distance} at {unit:?} but the \
                         outline disagrees",
                        probe.shape.name()
                    );
                }
            }
        }
    }
}

/// Which shapes are analytic is a design decision, not an accident, and it is worth
/// a test so that a change to the outline code cannot quietly move a shape from one
/// pipeline to the other.
#[test]
fn the_analytic_set_is_the_one_the_architecture_expects() {
    let analytic: Vec<&str> = PROBES
        .iter()
        .filter(|probe| probe.shape.is_analytic())
        .map(|probe| probe.shape.name())
        .collect();
    for expected in [
        "rectangle",
        "rounded_rectangle",
        "ellipse",
        "terminator",
        "delay",
        "diamond",
        "star",
        "cross",
        "arrow",
        "regular_polygon",
        "trapezoid",
        "parallelogram",
        "off_page_connector",
    ] {
        assert!(analytic.contains(&expected), "{expected} should be analytic");
    }
    // Curves that are not a whole ellipse, and anything with interior detail, must
    // tessellate.
    for expected in [
        "cloud", "heart", "cylinder", "document", "multi_document", "speech_bubble", "arc",
        "wedge", "or", "summing_junction", "database", "stored_data", "display", "direct_data",
        "predefined_process", "internal_storage",
    ] {
        assert!(!analytic.contains(&expected), "{expected} should not be analytic");
    }
}

#[test]
fn shapes_round_trip_through_the_document_format() {
    for shape in CATALOGUE {
        let json = serde_json::to_string(shape).unwrap();
        let back: Shape = serde_json::from_str(&json).unwrap();
        assert_eq!(*shape, back, "{json}");
    }
}

/// The catalogue is what a shape palette is built from, so a duplicate would show
/// the same tile twice.
#[test]
fn the_catalogue_has_no_duplicates() {
    for (i, shape) in CATALOGUE.iter().enumerate() {
        assert!(
            !CATALOGUE[..i].contains(shape),
            "{shape:?} appears twice in the catalogue"
        );
    }
}

#[test]
fn the_catalogue_covers_both_miro_shape_categories() {
    let names: Vec<&str> = CATALOGUE.iter().map(Shape::name).collect();
    for basic in ["rectangle", "ellipse", "star", "cloud", "heart", "cylinder", "arrow"] {
        assert!(names.contains(&basic), "missing basic shape {basic}");
    }
    for flowchart in [
        "terminator",
        "document",
        "manual_input",
        "predefined_process",
        "off_page_connector",
        "database",
        "summing_junction",
    ] {
        assert!(names.contains(&flowchart), "missing flowchart shape {flowchart}");
    }
}
