//! Vellum's connectors: the lines between things, and the bindings that keep them
//! attached.
//!
//! Miro stores a connector end as `{"point": {"x": 1, "y": 0.5}, "widgetIndex": 219}`
//! — a **normalised attachment on the target's bounds**, not a world coordinate.
//! `docs/01-architecture.md` names preserving that binding a top risk, and the
//! reason is what happens if it is lost: a connector imported as two fixed points
//! looks perfect until the first drag, at which point it detaches from the widget it
//! describes and the diagram starts lying. Everything in this crate is arranged so
//! that a connector's geometry is *derived* on demand from live bounds rather than
//! stored.
//!
//! Pure geometry — no GPU, no document, no window. The two things most likely to be
//! quietly wrong about connectors are rotated anchor resolution and obstacle
//! avoidance, and both are far easier to trust when `cargo test` can exercise them
//! on a machine with no graphics stack.
//!
//! ```
//! use vellum_connect::{
//!     Anchor, Connector, ConnectorStyle, Endpoint, Router, RoutingMode,
//!     TessellationOptions, WidgetBounds, WidgetId, Point, tessellate,
//! };
//!
//! // Two widgets, and a connector from the right edge of one to the left of the other.
//! let widgets = vec![
//!     WidgetBounds::new(Point::new(0.0, 0.0), 200.0, 100.0, 0.0),
//!     WidgetBounds::new(Point::new(600.0, 0.0), 200.0, 100.0, 0.0),
//! ];
//! let connector = Connector::new(
//!     Endpoint::bound(WidgetId(0), Anchor::RIGHT),
//!     Endpoint::bound(WidgetId(1), Anchor::LEFT),
//!     ConnectorStyle { routing: RoutingMode::Straight, ..ConnectorStyle::default() },
//! );
//!
//! let path = Router::default().route(&connector, &widgets, &[]).unwrap();
//! assert_eq!(path.start, Point::new(100.0, 0.0));
//! assert!(path.hit_test(Point::new(300.0, 0.0), 1.0));
//!
//! let mesh = tessellate(&path, &connector.style, &TessellationOptions::default()).unwrap();
//! assert!(!mesh.is_empty());
//! ```
//!
//! # What is verified and what is assumed
//!
//! The Miro mappings here were recovered from one real board, cross-checked against
//! that board's SVG export — a different Miro code path, and therefore the strongest
//! correctness signal available. [`style`] records key by key which is which. In
//! short: `lt: 1` is straight, `ls: 2` is solid, `a_end: 9` is a filled triangle
//! whose exact proportions are transcribed from the export, and the other codes in
//! each of those families are inferred rather than observed.
//!
//! # Where the geometry is approximate
//!
//! - **Obstacles are axis-aligned.** A rotated widget contributes its bounding box,
//!   so a route may keep more clearance from a turned widget than it needs. Over-
//!   reserving is the safe direction.
//! - **Jump-overs are inserted into straight segments only**, though they are
//!   detected against curves. See [`jump`].
//! - **Arrowhead scaling with thickness is an assumption.** Only `t: 2` has been
//!   observed. See [`arrow`].

pub mod anchor;
pub mod arrow;
pub mod geometry;
pub mod jump;
mod orthogonal;
pub mod route;
pub mod style;
pub mod tess;

pub use anchor::{Anchor, AnchorSide, BoundsSource, WidgetBounds, WidgetId};
pub use arrow::{ArrowDraw, ArrowheadGeometry, arrowhead};
pub use geometry::{EPSILON, Point, Polyline, Rect, Vec2};
pub use jump::{JumpOptions, apply_jump_overs, crossings, insert_jumps};
pub use route::{
    ConnectError, Connector, DEFAULT_TOLERANCE, Endpoint, ObstacleAvoidance, PathSegment,
    ResolvedEndpoint, RoutedPath, Router,
};
pub use style::{Arrowhead, ConnectorStyle, DashPattern, LineStyle, RoutingMode};
pub use tess::{Mesh, TessellationOptions, tessellate};
