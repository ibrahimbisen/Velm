//! The board's own surface: its colour and its grid.
//!
//! Everything else this client draws is an item. This is what is *underneath* them, and the
//! reason it is worth porting rather than leaving out is that the two applications are
//! compared side by side: a Velm board without its dots does not read as the same board with
//! fewer features, it reads as a different program.
//!
//! # Which grid, and from where
//!
//! Two sources, and the split is the user's own instruction recorded in `CLAUDE.md`
//! feedback 31: **the pattern and the grid's colour are one setting across every board**,
//! held in the desktop app's library sidecar, while the background *colour* is per board and
//! lives in the document. A browser has no sidecar, so it draws the board's own
//! [`vellum_doc::Background`] — which is precisely the fallback the desktop app uses when no
//! global choice has been made, so the two agree by default rather than by coincidence.

use vellum_doc::{Background, Pattern};
// ⚠ Not defined here. `vellum_project::look` owns every one of these and `draw.rs` reads the
// same values — which is exactly the point. The browser's dot shipped at the `1.5` of
// feedback 11 while its colour came from feedback 27, which reduced the dot to 1.0 in the
// same round: two numbers from two different moments, drawing a 3x3 black square where the
// desktop app draws a single pixel.
use vellum_project::look::{GRID_DOT, MAX_GRID_DOTS, grid_step};
use vellum_render::{DrawList, QuadInstance, Rgba};
use vellum_scene::{Camera, WorldPoint};


/// The board's colour, or the theme's if it never chose one.
pub fn clear_colour(background: &Background, theme_canvas: Rgba) -> Rgba {
    background.color.map_or(theme_canvas, vellum_project::theme::convert)
}

/// Draw the board's grid into the **screen** view.
///
/// Screen rather than board, and it is not an optimisation: a dot sized in world units is a
/// smear at a fitted 4% and a disc at 8×, which is the same argument `push_dashed` makes for
/// a guide's dashes and `push_grid` makes natively. The spacing is in world units; the ink
/// is not.
pub fn push_grid(list: &mut DrawList, camera: &Camera, background: &Background, grid: Rgba) {
    let pattern = background.pattern;
    if pattern == Pattern::Plain || grid.a <= 0.0 {
        return;
    }
    let viewport = camera.viewport();
    let Some(step) = grid_step(camera.zoom()) else { return };

    let visible = camera.visible_world_rect();
    let first_x = (visible.min.x / step).floor() * step;
    let first_y = (visible.min.y / step).floor() * step;
    let columns = ((visible.max.x - first_x) / step).ceil() as i64 + 1;
    let rows = ((visible.max.y - first_y) / step).ceil() as i64 + 1;
    if columns <= 0 || rows <= 0 {
        return;
    }

    // Rounded, not merely clamped: a dot lands on whole device pixels below, and a whole
    // number of them is the only size that lands on them exactly.
    let dot = (GRID_DOT * viewport.height.max(1.0) as f32 / 900.0)
        .clamp(1.0, 3.0)
        .round()
        .max(1.0);
    let (width, height) = (viewport.width as f32, viewport.height as f32);

    if pattern == Pattern::Lines {
        for column in 0..columns {
            let world = WorldPoint::new(first_x + column as f64 * step, first_y);
            let x = camera.world_to_screen(world).x as f32;
            if x >= 0.0 && x <= width {
                list.push_quad(QuadInstance::solid([x - dot * 0.5, 0.0], [dot, height], grid));
            }
        }
        for row in 0..rows {
            let world = WorldPoint::new(first_x, first_y + row as f64 * step);
            let y = camera.world_to_screen(world).y as f32;
            if y >= 0.0 && y <= height {
                list.push_quad(QuadInstance::solid([0.0, y - dot * 0.5], [width, dot], grid));
            }
        }
        return;
    }

    if columns * rows > MAX_GRID_DOTS {
        return;
    }
    for row in 0..rows {
        for column in 0..columns {
            let world = WorldPoint::new(
                first_x + column as f64 * step,
                first_y + row as f64 * step,
            );
            let point = camera.world_to_screen(world);
            let (x, y) = (point.x as f32, point.y as f32);
            if x < 0.0 || x > width || y < 0.0 || y > height {
                continue;
            }
            // Snapped to whole pixels, which is the other half of feedback 11: an unsnapped
            // dot is composited across two of them and comes out lighter than asked for.
            list.push_quad(QuadInstance::solid(
                [(x - dot * 0.5).round(), (y - dot * 0.5).round()],
                [dot, dot],
                grid,
            ));
        }
    }
}
