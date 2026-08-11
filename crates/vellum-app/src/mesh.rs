//! Triangles for the strokes a quad cannot express.
//!
//! The renderer draws axis-aligned rectangles very cheaply and everything else through
//! the mesh batch. A polyline is the "everything else" that keeps coming up — a chart's
//! line series, a mind map's branches — and it is the same ribbon each time, so it is
//! written once here rather than once per widget crate.
//!
//! Deliberately generic over `[f32; 2]` rather than over any one crate's `Point`:
//! `vellum-chart` and `vellum-mindmap` each define their own geometry vocabulary, for
//! the layering reason those crates record, and neither should have to learn the
//! other's in order to be stroked.

use vellum_shapes::Mesh;

/// A polyline as a triangle strip of the given width.
///
/// Segment quads with round-ish joins left implicit: at the widths a chart series or a
/// mind-map branch is drawn with, the gap at a join is sub-pixel, and mitring it
/// properly means solving the outer corner for every vertex. `vellum-ink` does that for
/// freehand strokes, where the width is large enough for it to show.
///
/// Degenerate input — fewer than two points, a repeated point, a non-positive width —
/// produces no triangles rather than `NaN`s. A repeated point in a data series is
/// ordinary data, not a bug to crash on.
pub fn ribbon(points: &[[f32; 2]], width: f32) -> Mesh {
    if points.len() < 2 || !width.is_finite() || width <= 0.0 {
        return Mesh::default();
    }
    let half = width / 2.0;
    let mut vertices = Vec::with_capacity((points.len() - 1) * 4);
    let mut indices = Vec::with_capacity((points.len() - 1) * 6);

    for pair in points.windows(2) {
        let ([ax, ay], [bx, by]) = (pair[0], pair[1]);
        let (dx, dy) = (bx - ax, by - ay);
        let length = dx.hypot(dy);
        if length <= f32::EPSILON {
            continue;
        }
        // The segment's normal, scaled to half the stroke.
        let (nx, ny) = (-dy / length * half, dx / length * half);
        let base = vertices.len() as u32;
        vertices.push([ax + nx, ay + ny]);
        vertices.push([ax - nx, ay - ny]);
        vertices.push([bx + nx, by + ny]);
        vertices.push([bx - nx, by - ny]);
        indices.extend_from_slice(&[base, base + 1, base + 2]);
        indices.extend_from_slice(&[base + 1, base + 3, base + 2]);
    }
    Mesh { vertices, indices }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line's ribbon is the stroke width across, whichever way the segment runs.
    #[test]
    fn a_polyline_becomes_a_ribbon_of_the_right_width() {
        let mesh = ribbon(&[[0.0, 0.0], [100.0, 0.0]], 4.0);
        assert_eq!(mesh.triangle_count(), 2, "one segment is two triangles");

        let ys: Vec<f32> = mesh.vertices.iter().map(|[_, y]| *y).collect();
        let (min, max) = (
            ys.iter().copied().fold(f32::MAX, f32::min),
            ys.iter().copied().fold(f32::MIN, f32::max),
        );
        assert!((max - min - 4.0).abs() < 1e-3, "ribbon was {} wide", max - min);
    }

    #[test]
    fn degenerate_input_produces_nothing_rather_than_nans() {
        assert_eq!(ribbon(&[[5.0, 5.0], [5.0, 5.0]], 4.0).triangle_count(), 0);
        assert_eq!(ribbon(&[], 4.0).triangle_count(), 0);
        assert_eq!(ribbon(&[[0.0, 0.0]], 4.0).triangle_count(), 0);
        assert_eq!(ribbon(&[[0.0, 0.0], [10.0, 0.0]], 0.0).triangle_count(), 0);
        assert_eq!(ribbon(&[[0.0, 0.0], [10.0, 0.0]], f32::NAN).triangle_count(), 0);
    }

    /// A corner is two independent segments, not a shared join — which is what makes
    /// the sub-pixel gap at the outer corner the documented limit rather than a bug.
    #[test]
    fn a_corner_is_two_segments() {
        let mesh = ribbon(&[[0.0, 0.0], [50.0, 0.0], [50.0, 50.0]], 3.0);
        assert_eq!(mesh.triangle_count(), 4);
        assert_eq!(mesh.vertices.len(), 8);
    }
}
