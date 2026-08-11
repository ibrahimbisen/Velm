//! The view onto the board: where we are looking and how far in.
//!
//! # Why the camera owns the f64 → f32 conversion
//!
//! World space is `f64` and the GPU is `f32`. The naive bridge — cast world
//! coordinates to `f32` and let the vertex shader subtract the camera — is exactly
//! the bug that makes browser canvas apps shimmer when you zoom in far. `f32` has a
//! 24-bit mantissa, so at the reference board's extent (41282 px) consecutive
//! representable values are 2⁻⁸ ≈ 0.0039 world px apart. At 32× zoom that is an
//! eighth of a screen pixel of quantisation on every vertex, and it *moves* as you
//! pan, which the eye reads as jitter.
//!
//! [`Camera::to_camera_relative`] subtracts the camera origin in `f64` first, so the
//! value that reaches `f32` is bounded by half a viewport in world units. Precision
//! then scales with the zoom level instead of with the board's size, which is the
//! property we actually want. `precision_holds_at_board_extent` in this module's
//! tests pins the difference down numerically.

use crate::geometry::{ScreenPoint, ScreenSize, WorldPoint, WorldRect};

/// How far out the user may zoom. 1% matches Miro's own floor and is enough to fit
/// the 41282 px reference board in a laptop window.
pub const MIN_ZOOM: f64 = 0.01;

/// How far in the user may zoom. Beyond ~64× the camera-relative `f32` conversion is
/// still exact, but a sticky note is wider than the screen and there is nothing
/// useful to see.
pub const MAX_ZOOM: f64 = 64.0;

/// Maps camera-relative world pixels (or, for overlays, raw screen pixels) onto
/// normalised device coordinates: `clip = position * scale + translate`.
///
/// This is the only piece of rendering math in the scene layer, and it lives here
/// because it is pure arithmetic derived from the camera — testable on a machine
/// with no GPU, which is the whole reason to keep it out of the renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipTransform {
    pub scale: [f32; 2],
    pub translate: [f32; 2],
}

impl ClipTransform {
    /// Maps positions already expressed in physical screen pixels (origin top-left,
    /// `+y` down) onto clip space. Used for HUD and chrome overlays, which must not
    /// move when the camera does.
    pub fn screen_pixels(viewport: ScreenSize) -> Self {
        Self {
            scale: [
                (2.0 / viewport.width.max(1.0)) as f32,
                (-2.0 / viewport.height.max(1.0)) as f32,
            ],
            translate: [-1.0, 1.0],
        }
    }

    /// Applies the transform on the CPU. Exists so the tests can check the same
    /// arithmetic the vertex shader performs.
    pub fn apply(&self, position: [f32; 2]) -> [f32; 2] {
        [
            position[0] * self.scale[0] + self.translate[0],
            position[1] * self.scale[1] + self.translate[1],
        ]
    }
}

/// A pan/zoom view onto world space. No rotation: Miro boards are axis-aligned and
/// a rotating camera would force every culling query through an oriented-box test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// The world point sitting under the centre of the viewport. Also the origin
    /// that camera-relative coordinates are measured from.
    center: WorldPoint,
    /// Screen pixels per world pixel. 1.0 is 100%.
    zoom: f64,
    /// Viewport size in physical pixels.
    viewport: ScreenSize,
}

impl Camera {
    /// A camera at 100% looking at the world origin.
    pub fn new(viewport: ScreenSize) -> Self {
        Self {
            center: WorldPoint::ORIGIN,
            zoom: 1.0,
            viewport,
        }
    }

    pub fn center(&self) -> WorldPoint {
        self.center
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn viewport(&self) -> ScreenSize {
        self.viewport
    }

    /// Resizes the viewport, keeping the centred world point centred.
    ///
    /// Anchoring on the centre rather than the top-left means dragging a window edge
    /// grows the visible area symmetrically instead of sliding the board sideways.
    pub fn set_viewport(&mut self, viewport: ScreenSize) {
        self.viewport = viewport;
    }

    pub fn set_center(&mut self, center: WorldPoint) {
        self.center = center;
    }

    pub fn world_to_screen(&self, p: WorldPoint) -> ScreenPoint {
        ScreenPoint::new(
            (p.x - self.center.x) * self.zoom + self.viewport.width / 2.0,
            (p.y - self.center.y) * self.zoom + self.viewport.height / 2.0,
        )
    }

    pub fn screen_to_world(&self, p: ScreenPoint) -> WorldPoint {
        WorldPoint::new(
            (p.x - self.viewport.width / 2.0) / self.zoom + self.center.x,
            (p.y - self.viewport.height / 2.0) / self.zoom + self.center.y,
        )
    }

    /// Drags the board by a screen-space delta, as a trackpad two-finger scroll or a
    /// mouse drag would.
    ///
    /// The delta is the distance the *content* should appear to travel, so dragging
    /// right by 100 px moves the camera 100/zoom world units to the left. Getting
    /// this sign backwards is the classic inverted-pan bug, hence the explicit test.
    pub fn pan_by_screen_delta(&mut self, dx: f64, dy: f64) {
        self.center.x -= dx / self.zoom;
        self.center.y -= dy / self.zoom;
    }

    /// Multiplies the zoom, keeping whatever world point sits under `anchor` exactly
    /// under `anchor` afterwards.
    ///
    /// This is what makes pinch-zoom feel attached to the fingers rather than to the
    /// window. The clamp is applied *before* the centre is recomputed, so hitting a
    /// zoom limit still leaves the anchor pinned instead of drifting.
    pub fn zoom_by(&mut self, factor: f64, anchor: ScreenPoint) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        self.set_zoom_about(self.zoom * factor, anchor);
    }

    /// Sets an absolute zoom level about a screen anchor. Values outside
    /// [`MIN_ZOOM`]..=[`MAX_ZOOM`] are clamped.
    pub fn set_zoom_about(&mut self, zoom: f64, anchor: ScreenPoint) {
        if !zoom.is_finite() {
            return;
        }
        let world_under_anchor = self.screen_to_world(anchor);
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);

        // Solve `world_to_screen(world_under_anchor) == anchor` for the new centre.
        self.center = WorldPoint::new(
            world_under_anchor.x - (anchor.x - self.viewport.width / 2.0) / self.zoom,
            world_under_anchor.y - (anchor.y - self.viewport.height / 2.0) / self.zoom,
        );
    }

    /// The world-space rectangle currently on screen. This is the culling query.
    pub fn visible_world_rect(&self) -> WorldRect {
        WorldRect::from_corners(
            self.screen_to_world(ScreenPoint::new(0.0, 0.0)),
            self.screen_to_world(ScreenPoint::new(self.viewport.width, self.viewport.height)),
        )
    }

    /// Moves and scales the camera so `rect` fills the viewport with a little slack.
    /// `margin` is a fraction of the rect, so 0.05 leaves a 5% border.
    pub fn fit_to_rect(&mut self, rect: WorldRect, margin: f64) {
        self.center = rect.center();
        let width = rect.width().max(f64::EPSILON) * (1.0 + margin * 2.0);
        let height = rect.height().max(f64::EPSILON) * (1.0 + margin * 2.0);
        let fit = (self.viewport.width / width).min(self.viewport.height / height);
        self.zoom = fit.clamp(MIN_ZOOM, MAX_ZOOM);
    }

    /// Converts a world point into the `f32` space the GPU sees. See the module
    /// docs for why this subtraction has to happen in `f64`.
    pub fn to_camera_relative(&self, p: WorldPoint) -> [f32; 2] {
        [(p.x - self.center.x) as f32, (p.y - self.center.y) as f32]
    }

    /// The transform the renderer hands the vertex shader alongside camera-relative
    /// instance positions.
    ///
    /// Derivation: `screen = rel * zoom + viewport/2`, and clip is
    /// `(2*screen/size - 1)` on x and `(1 - 2*screen/size)` on y (wgpu's NDC has `+y`
    /// up). The `viewport/2` terms cancel, leaving a pure scale.
    pub fn clip_transform(&self) -> ClipTransform {
        ClipTransform {
            scale: [
                (2.0 * self.zoom / self.viewport.width.max(1.0)) as f32,
                (-2.0 * self.zoom / self.viewport.height.max(1.0)) as f32,
            ],
            translate: [0.0, 0.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> Camera {
        Camera::new(ScreenSize::new(1600.0, 900.0))
    }

    fn assert_close(a: f64, b: f64, tol: f64, what: &str) {
        assert!((a - b).abs() <= tol, "{what}: {a} vs {b}");
    }

    #[test]
    fn centre_of_the_viewport_shows_the_camera_centre() {
        let mut cam = camera();
        cam.set_center(WorldPoint::new(1234.5, -678.25));
        let s = cam.world_to_screen(cam.center());
        assert_close(s.x, 800.0, 1e-9, "x");
        assert_close(s.y, 450.0, 1e-9, "y");
    }

    /// The round trip is the contract every other transform depends on. Exercised
    /// across zoom levels and far-from-origin centres because that is where a
    /// misplaced `viewport/2` shows up.
    #[test]
    fn world_to_screen_round_trips() {
        for zoom in [0.01, 0.37, 1.0, 3.0, 64.0] {
            for center in [
                WorldPoint::ORIGIN,
                WorldPoint::new(41282.89, 17515.36),
                WorldPoint::new(-98765.4, 54321.0),
            ] {
                let mut cam = camera();
                cam.set_center(center);
                cam.set_zoom_about(zoom, ScreenPoint::new(800.0, 450.0));
                cam.set_center(center);

                for p in [
                    WorldPoint::ORIGIN,
                    WorldPoint::new(center.x + 137.5, center.y - 42.0),
                    WorldPoint::new(center.x - 9000.0, center.y + 9000.0),
                ] {
                    let back = cam.screen_to_world(cam.world_to_screen(p));
                    assert_close(back.x, p.x, 1e-6, "round-trip x");
                    assert_close(back.y, p.y, 1e-6, "round-trip y");
                }
            }
        }
    }

    #[test]
    fn screen_to_world_round_trips() {
        let mut cam = camera();
        cam.set_center(WorldPoint::new(20000.0, -5000.0));
        cam.set_zoom_about(2.5, ScreenPoint::new(0.0, 0.0));

        for s in [
            ScreenPoint::new(0.0, 0.0),
            ScreenPoint::new(1600.0, 900.0),
            ScreenPoint::new(733.25, 118.75),
        ] {
            let back = cam.world_to_screen(cam.screen_to_world(s));
            assert_close(back.x, s.x, 1e-6, "round-trip x");
            assert_close(back.y, s.y, 1e-6, "round-trip y");
        }
    }

    /// The property that makes zoom feel attached to the cursor: whatever world
    /// point was under the anchor must still be under it afterwards.
    #[test]
    fn zoom_keeps_the_anchor_point_fixed() {
        for anchor in [
            ScreenPoint::new(0.0, 0.0),
            ScreenPoint::new(1600.0, 900.0),
            ScreenPoint::new(37.0, 811.5),
            ScreenPoint::new(800.0, 450.0),
        ] {
            for factor in [1.1, 0.9, 4.0, 0.25] {
                let mut cam = camera();
                cam.set_center(WorldPoint::new(41282.89, 17515.36));
                let before = cam.screen_to_world(anchor);

                cam.zoom_by(factor, anchor);

                let after = cam.screen_to_world(anchor);
                assert_close(after.x, before.x, 1e-6, "anchor x");
                assert_close(after.y, before.y, 1e-6, "anchor y");
            }
        }
    }

    /// Repeated zooming must not let the anchor creep — each step re-derives the
    /// centre from the anchor, so error cannot accumulate.
    #[test]
    fn repeated_zoom_does_not_drift_the_anchor() {
        let anchor = ScreenPoint::new(1200.0, 300.0);
        let mut cam = camera();
        cam.set_center(WorldPoint::new(-31000.0, 7000.0));
        let before = cam.screen_to_world(anchor);

        for _ in 0..500 {
            cam.zoom_by(1.02, anchor);
        }
        for _ in 0..500 {
            cam.zoom_by(1.0 / 1.02, anchor);
        }

        let after = cam.screen_to_world(anchor);
        assert_close(after.x, before.x, 1e-6, "anchor x after 1000 steps");
        assert_close(after.y, before.y, 1e-6, "anchor y after 1000 steps");
    }

    /// Hitting a zoom limit must still pin the anchor, not slide the board.
    #[test]
    fn clamped_zoom_still_pins_the_anchor() {
        let anchor = ScreenPoint::new(100.0, 800.0);
        let mut cam = camera();
        let before = cam.screen_to_world(anchor);

        cam.zoom_by(1e9, anchor);
        assert_eq!(cam.zoom(), MAX_ZOOM);
        let after = cam.screen_to_world(anchor);
        assert_close(after.x, before.x, 1e-6, "anchor x at max zoom");
        assert_close(after.y, before.y, 1e-6, "anchor y at max zoom");

        cam.zoom_by(1e-9, anchor);
        assert_eq!(cam.zoom(), MIN_ZOOM);
        let after = cam.screen_to_world(anchor);
        assert_close(after.x, before.x, 1e-6, "anchor x at min zoom");
        assert_close(after.y, before.y, 1e-6, "anchor y at min zoom");
    }

    #[test]
    fn nonsense_zoom_factors_are_ignored() {
        let mut cam = camera();
        let anchor = ScreenPoint::new(10.0, 10.0);
        let before = cam;

        cam.zoom_by(f64::NAN, anchor);
        cam.zoom_by(0.0, anchor);
        cam.zoom_by(-2.0, anchor);
        cam.set_zoom_about(f64::INFINITY, anchor);

        assert_eq!(cam, before);
    }

    #[test]
    fn panning_moves_the_content_with_the_gesture() {
        let mut cam = camera();
        let world_under_cursor = cam.screen_to_world(ScreenPoint::new(400.0, 400.0));

        // Drag 100 px right: the point that was at x=400 should now be at x=500.
        cam.pan_by_screen_delta(100.0, 0.0);

        let now = cam.world_to_screen(world_under_cursor);
        assert_close(now.x, 500.0, 1e-9, "dragged x");
        assert_close(now.y, 400.0, 1e-9, "dragged y");
    }

    #[test]
    fn panning_scales_with_zoom() {
        let mut cam = camera();
        cam.set_zoom_about(4.0, ScreenPoint::new(800.0, 450.0));
        cam.pan_by_screen_delta(400.0, 0.0);
        // 400 screen px at 4× is 100 world px.
        assert_close(cam.center().x, -100.0, 1e-9, "centre x");
    }

    #[test]
    fn visible_rect_matches_the_viewport_corners() {
        let mut cam = camera();
        cam.set_center(WorldPoint::new(500.0, -200.0));
        cam.set_zoom_about(2.0, ScreenPoint::new(800.0, 450.0));
        cam.set_center(WorldPoint::new(500.0, -200.0));

        let r = cam.visible_world_rect();
        assert_close(r.width(), 1600.0 / 2.0, 1e-9, "visible width");
        assert_close(r.height(), 900.0 / 2.0, 1e-9, "visible height");
        assert_close(r.center().x, 500.0, 1e-9, "visible centre x");
        assert_close(r.center().y, -200.0, 1e-9, "visible centre y");
    }

    #[test]
    fn fit_to_rect_brings_the_whole_rect_on_screen() {
        let mut cam = camera();
        let board = WorldRect::from_origin_size(WorldPoint::new(-1000.0, -500.0), 41282.89, 17515.36);
        cam.fit_to_rect(board, 0.05);

        let visible = cam.visible_world_rect();
        assert!(visible.min.x <= board.min.x, "left edge cut off");
        assert!(visible.max.x >= board.max.x, "right edge cut off");
        assert!(visible.min.y <= board.min.y, "top edge cut off");
        assert!(visible.max.y >= board.max.y, "bottom edge cut off");
    }

    /// The reason `to_camera_relative` exists, measured rather than asserted in
    /// prose. `f32` quantisation grows with the magnitude of the coordinate, so the
    /// naive cast gets worse the further from the origin the user works; subtracting
    /// the camera first makes the error depend only on how much board fits on screen.
    ///
    /// Returns `(naive_error, camera_relative_error)` in world pixels.
    fn quantisation_error(point: f64, camera_center: f64) -> (f64, f64) {
        let mut cam = camera();
        cam.set_center(WorldPoint::new(camera_center, camera_center));
        let p = WorldPoint::new(point, point);

        let naive = (point as f32) as f64;
        let relative = cam.to_camera_relative(p)[0] as f64 + cam.center().x;

        ((naive - point).abs(), (relative - point).abs())
    }

    #[test]
    fn precision_holds_at_board_extent() {
        // The far corner of the real reference board (41282.89 x 17515.36).
        let (naive, relative) = quantisation_error(41_282.89, 41_000.0);

        // f32 near 4.1e4 has an ulp of 2^-8 ≈ 0.0039 world px.
        assert!(naive > 1e-4, "naive cast unexpectedly exact: {naive}");
        assert!(
            relative < naive / 10.0,
            "camera-relative was not decisively better: {relative} vs {naive}"
        );
    }

    /// Far from the origin — a user who has panned a long way, which an infinite
    /// canvas invites — the naive cast quantises to whole pixels and the board
    /// visibly snaps as you pan. Camera-relative is unaffected, because the value
    /// that reaches `f32` is still only a few hundred.
    #[test]
    fn precision_survives_far_from_the_origin() {
        let (naive, relative) = quantisation_error(10_000_000.3, 10_000_000.0);

        // f32 near 1e7 has an ulp of exactly 1.0.
        assert!(naive > 0.1, "expected whole-pixel error from the naive cast, got {naive}");
        assert!(relative < 1e-5, "camera-relative error grew to {relative}");
    }

    #[test]
    fn clip_transform_maps_the_viewport_corners_to_ndc() {
        let mut cam = camera();
        cam.set_center(WorldPoint::new(9000.0, -400.0));
        cam.set_zoom_about(3.0, ScreenPoint::new(800.0, 450.0));

        let t = cam.clip_transform();
        let visible = cam.visible_world_rect();

        // Top-left of the viewport is (-1, +1) in NDC; bottom-right is (+1, -1).
        let tl = t.apply(cam.to_camera_relative(visible.min));
        let br = t.apply(cam.to_camera_relative(visible.max));
        assert_close(tl[0] as f64, -1.0, 1e-4, "top-left ndc x");
        assert_close(tl[1] as f64, 1.0, 1e-4, "top-left ndc y");
        assert_close(br[0] as f64, 1.0, 1e-4, "bottom-right ndc x");
        assert_close(br[1] as f64, -1.0, 1e-4, "bottom-right ndc y");

        // The camera centre is the middle of the screen.
        let mid = t.apply(cam.to_camera_relative(cam.center()));
        assert_close(mid[0] as f64, 0.0, 1e-6, "centre ndc x");
        assert_close(mid[1] as f64, 0.0, 1e-6, "centre ndc y");
    }

    #[test]
    fn screen_space_clip_transform_maps_pixels_to_ndc() {
        let t = ClipTransform::screen_pixels(ScreenSize::new(1600.0, 900.0));
        assert_eq!(t.apply([0.0, 0.0]), [-1.0, 1.0]);
        assert_eq!(t.apply([1600.0, 900.0]), [1.0, -1.0]);
        assert_eq!(t.apply([800.0, 450.0]), [0.0, 0.0]);
    }

    /// A zero-sized viewport happens for one frame when a window is minimised.
    /// The transform must stay finite rather than producing NaN vertices.
    #[test]
    fn degenerate_viewport_does_not_produce_nan() {
        let cam = Camera::new(ScreenSize::new(0.0, 0.0));
        let t = cam.clip_transform();
        assert!(t.scale[0].is_finite() && t.scale[1].is_finite());
        assert!(ClipTransform::screen_pixels(ScreenSize::new(0.0, 0.0)).scale[0].is_finite());
    }
}
