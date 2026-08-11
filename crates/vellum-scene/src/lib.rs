//! Vellum's spatial layer: where things are, what you can see, and what you clicked.
//!
//! Pure logic — no GPU, no window, no document. That separation is deliberate: the
//! two things most likely to be silently wrong in a canvas app are the camera
//! transforms and the culling query, and both are far easier to trust when they can
//! be exercised by `cargo test` on a machine with no graphics stack at all.
//!
//! The layer rests on two decisions from `docs/01-architecture.md`:
//!
//! - **`f64` world space, camera-relative `f32` for the GPU.** Boards reach tens of
//!   thousands of pixels across (the reference board is 41282 × 17515), and casting
//!   those straight to `f32` quantises them coarsely enough to shimmer at deep zoom.
//!   [`Camera::to_camera_relative`] moves the origin first. See [`camera`].
//! - **An R-tree index.** Frame cost scales with what is on screen rather than with
//!   board size — the single biggest reason Miro degrades on large boards. See
//!   [`scene`].
//!
//! ```
//! use vellum_scene::{Camera, Scene, SceneItem, ScreenSize, WorldPoint, WorldRect};
//!
//! let mut scene = Scene::new();
//! scene.insert(SceneItem::solid_quad(
//!     1,
//!     WorldRect::from_origin_size(WorldPoint::new(0.0, 0.0), 200.0, 120.0),
//!     0,
//!     [1.0, 0.97, 0.62, 1.0],
//! ));
//!
//! let camera = Camera::new(ScreenSize::new(1280.0, 720.0));
//! assert_eq!(scene.query_viewport(&camera).count(), 1);
//! assert_eq!(scene.hit_test(WorldPoint::new(10.0, 10.0)), Some(1));
//! ```

pub mod camera;
pub mod geometry;
pub mod item;
pub mod scene;

pub use camera::{Camera, ClipTransform, MAX_ZOOM, MIN_ZOOM};
pub use geometry::{ScreenPoint, ScreenSize, WorldPoint, WorldRect};
pub use item::{ItemId, RenderPayload, SceneItem};
pub use scene::Scene;
