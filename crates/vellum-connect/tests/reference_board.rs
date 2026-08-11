//! End-to-end check against a real Miro connector, with Miro's own SVG export as
//! the oracle.
//!
//! `docs/02-miro-formats.md` §3 makes the case for this style of test: the clipboard
//! payload and the SVG export come out of *different* Miro code paths, so agreement
//! between them is the strongest correctness signal available. Object 221 of the
//! reference board's clipboard payload is
//!
//! ```json
//! { "primary":   { "point": {"x":1,"y":0.5}, "widgetIndex": 219 },
//!   "secondary": { "point": {"x":0,"y":0.5}, "widgetIndex": 220 },
//!   "style": "{\"lc\":3355443,\"ls\":2,\"t\":2,\"lt\":1,\"a_start\":0,\"a_end\":9,\"jump\":0}" }
//! ```
//!
//! and objects 219 and 220 are `199 × 228` stickies at `scale: 1.85`. Miro's SVG
//! export draws that connector as
//!
//! ```svg
//! <g width="368.15px" height="0px" transform="translate(27775.63, 4287.97) …">
//!   <path stroke-linecap="butt" stroke="#333333" stroke-width="2" fill="transparent"
//!         d="M 0 0 L 357.9681818181778 0"/>
//!   <use xlink:href="#LineHeadArrow2" fill="#333333"
//!        transform="translate(368.1499999999978, 0) rotate(0)"/>
//! </g>
//! ```
//!
//! Every number this file asserts is one of those, reconstructed from the clipboard
//! side alone.

use vellum_connect::{
    Anchor, Arrowhead, Connector, ConnectorStyle, Endpoint, LineStyle, Point, Router, RoutingMode,
    TessellationOptions, WidgetBounds, WidgetId, arrowhead, tessellate,
};

/// `width="368.15px"` on the connector's group.
const SVG_CONNECTOR_SPAN: f64 = 368.15;
/// `d="M 0 0 L 357.9681818181778 0"` — where Miro stops the line short of the tip.
const SVG_DRAWN_LINE_LENGTH: f64 = 357.968_181_818_177_8;
/// `M-12.727,-7.545 … L-12.727,7.545` — the arrowhead's full width.
const SVG_ARROWHEAD_WIDTH: f64 = 2.0 * 7.545_454_545_454_545;

/// The two stickies connector 221 binds to, with `size` and `scale` as the clipboard
/// stores them.
fn reference_widgets() -> Vec<WidgetBounds> {
    (0..221)
        .map(|i| match i {
            219 => WidgetBounds::from_miro(
                Point::new(6748.319280161753, -4696.375103507173),
                199.0,
                228.0,
                1.85,
                0.0,
            ),
            220 => WidgetBounds::from_miro(
                Point::new(7484.619280161751, -4696.375103507173),
                199.0,
                228.0,
                1.85,
                0.0,
            ),
            // The rest of the board's objects are irrelevant here, but the array is
            // kept dense because Miro references objects by position.
            _ => WidgetBounds::new(Point::ORIGIN, 0.0, 0.0, 0.0),
        })
        .collect()
}

/// Connector 221's style, decoded from the compact keys rather than hand-built.
fn reference_style() -> ConnectorStyle {
    ConnectorStyle {
        routing: RoutingMode::from_miro(1),
        line: LineStyle::from_miro(2),
        thickness: 2.0,
        start_arrow: Arrowhead::from_miro(0),
        end_arrow: Arrowhead::from_miro(9),
        jump_overs: false,
    }
}

fn reference_connector() -> Connector {
    Connector::new(
        Endpoint::bound(WidgetId(219), Anchor::RIGHT),
        Endpoint::bound(WidgetId(220), Anchor::LEFT),
        reference_style(),
    )
}

#[test]
fn the_style_decodes_to_what_the_export_draws() {
    let style = reference_style();
    assert_eq!(style.routing, RoutingMode::Straight, "the export has no curve or elbow");
    assert_eq!(style.line, LineStyle::Solid, "the export has no stroke-dasharray");
    assert_eq!(style.start_arrow, Arrowhead::None, "only one LineHeadArrow2 per connector");
    assert_eq!(style.end_arrow, Arrowhead::FilledTriangle, "and it is filled");
}

#[test]
fn the_route_is_exactly_as_long_as_the_svg_group_is_wide() {
    let path = Router::default()
        .route(&reference_connector(), &reference_widgets(), &[])
        .unwrap();

    assert_eq!(path.segments.len(), 1, "a straight connector is one segment");
    assert!((path.length() - SVG_CONNECTOR_SPAN).abs() < 1e-9, "{}", path.length());
    // `height="0px"`: the export draws it perfectly horizontal.
    assert!((path.start.y - path.end().y).abs() < 1e-9);
    assert!(path.end().x > path.start.x, "primary is the left-hand sticky");
}

#[test]
fn the_line_stops_where_miro_stops_it() {
    let connector = reference_connector();
    let path = Router::default().route(&connector, &reference_widgets(), &[]).unwrap();

    let head = arrowhead(
        connector.style.end_arrow,
        path.end(),
        path.end_tangent().unwrap(),
        connector.style.thickness,
    )
    .unwrap();

    let drawn = path.flatten(0.01).trimmed(0.0, head.trim);
    assert!(
        (drawn.length() - SVG_DRAWN_LINE_LENGTH).abs() < 1e-9,
        "drawn {} vs SVG {SVG_DRAWN_LINE_LENGTH}",
        drawn.length()
    );
}

#[test]
fn the_tessellated_connector_matches_the_exports_dimensions() {
    let connector = reference_connector();
    let path = Router::default().route(&connector, &reference_widgets(), &[]).unwrap();
    let mesh =
        tessellate(&path, &connector.style, &TessellationOptions::default()).unwrap();

    let bounds = mesh.bounds().expect("the connector produces geometry");
    // The head reaches the tip even though the line stops short of it.
    assert!((bounds.width() - SVG_CONNECTOR_SPAN).abs() < 1e-3, "{bounds:?}");
    // The widest thing is the arrowhead, not the 2px line.
    assert!((bounds.height() - SVG_ARROWHEAD_WIDTH).abs() < 1e-3, "{bounds:?}");
    assert!(mesh.triangle_count() >= 3, "a quad for the line plus the head");
}

/// The binding is the point of the exercise: moving a widget must move the
/// connector, without anything being re-imported or re-bound.
#[test]
fn dragging_a_bound_widget_re_routes_the_connector() {
    let router = Router::default();
    let connector = reference_connector();
    let mut widgets = reference_widgets();

    let before = router.route(&connector, &widgets, &[]).unwrap();

    widgets[220].center = Point::new(widgets[220].center.x, widgets[220].center.y - 500.0);
    let after = router.route(&connector, &widgets, &[]).unwrap();

    assert_eq!(after.start, before.start, "the other end has not moved");
    assert!((after.end().y - (before.end().y - 500.0)).abs() < 1e-9);
    assert!(after.length() > before.length());
}

/// Rotating a widget must move the anchor around it, not merely spin the widget
/// under a connector that stays put. This is the failure that survives a screenshot.
#[test]
fn rotating_a_bound_widget_moves_the_anchor_around_it() {
    let router = Router::default();
    let connector = reference_connector();
    let mut widgets = reference_widgets();

    let before = router.route(&connector, &widgets, &[]).unwrap();
    widgets[219].rotation_degrees = 90.0;
    let after = router.route(&connector, &widgets, &[]).unwrap();

    // The right-edge anchor of a 368.15 × 421.8 sticky sits 184.075 to the right of
    // its centre. Turned 90° clockwise, that same anchor sits 184.075 *below* it —
    // the offset rotates with the widget rather than staying on the right.
    let centre = widgets[219].center;
    let (flat, turned) = (before.start - centre, after.start - centre);
    assert!((flat.x - 184.075).abs() < 1e-9 && flat.y.abs() < 1e-9, "{flat:?}");
    assert!((turned.y - 184.075).abs() < 1e-9 && turned.x.abs() < 1e-9, "{turned:?}");
    assert!(before.start.distance_to(after.start) > 200.0);
}
