//! Vellum's data charts — bar, line, area, scatter and pie — as pure geometry.
//!
//! `docs/features/README.md` §2 calls for "bar/line/pie generated from a data
//! table". This crate is the half of that which has no pixels in it: it turns a
//! table of numbers and a rectangle into bars, polylines, arcs, axes, gridlines, a
//! legend and value labels, all positioned. No GPU, no windowing, no fonts — the
//! same split `vellum-shapes` and `vellum-ink` make, and for the same reason: this
//! is the part that is worth unit-testing exhaustively, and it tests in
//! milliseconds on a machine with no graphics stack.
//!
//! ```
//! use vellum_chart::{ChartKind, ChartSpec, Dataset, MonoMetrics, Rect, Series, build};
//!
//! let data = Dataset::new(
//!     ["Q1", "Q2", "Q3", "Q4"],
//!     [Series::new("Hours", [18.0, 47.4, 71.1, 94.8])],
//! );
//! let chart = ChartSpec::categorical(ChartKind::bar(), data);
//! let geometry = build(&chart, Rect::new(0.0, 0.0, 480.0, 240.0), &MonoMetrics::default());
//!
//! // The axis reads in round numbers, not in the data's own awkward ones.
//! let ticks: Vec<f64> = geometry.axes[0].ticks.iter().map(|tick| tick.value).collect();
//! assert_eq!(ticks, vec![0.0, 25.0, 50.0, 75.0, 100.0]);
//!
//! // Four bars, all inside the plot, all measured from the same baseline.
//! assert_eq!(geometry.marks.len(), 4);
//! assert!(geometry.is_finite());
//! ```
//!
//! # What the crate is opinionated about
//!
//! Three things decide whether a chart looks considered or generated, and all three
//! are decisions this crate makes rather than options it exposes.
//!
//! **Axes carry nice numbers.** `0, 25, 50, 75, 100`, never `0, 23.7, 47.4`. This is
//! the most visible tell there is, and [`mod@scale`] is the crate's most heavily
//! tested module because of it — every degenerate input a spreadsheet can produce
//! (one row, a column of identical values, all negatives, a range straddling zero,
//! magnitudes at both ends of what an `f64` holds, nothing at all) resolves to a
//! readable axis rather than a panic or a `NaN`.
//!
//! **Colour comes from the design language.** Vellum has exactly two accents and a
//! deliberately flat set of neutrals; a chart may not invent a palette to get around
//! that. [`mod@palette`] documents how a six-slot categorical sequence is derived
//! from those tokens, what it is validated against, and the two checks it knowingly
//! fails and why. It re-derives and re-checks itself on every `cargo test`.
//!
//! **Labels never collide.** A number that overlaps another number, or is clipped by
//! its own bar, is worse than no number. [`mod@label`] places them by first fit in a
//! stable order and *drops* what does not fit, reporting the count.
//!
//! # Layout
//!
//! | module | what it owns |
//! |---|---|
//! | [`mod@data`] | series of numbers, and where `NaN` stops being a number |
//! | [`mod@scale`] | nice ticks, and the map from data to pixels |
//! | [`mod@axis`] | ticks positioned, labelled and thinned; gridlines |
//! | [`mod@label`] | text boxes, placement policy, collision avoidance |
//! | [`mod@legend`] | keys, wrapped and positioned |
//! | [`mod@mark`] | the drawable marks themselves |
//! | [`mod@palette`] | the derived colour sequence, and its proof |
//! | [`mod@chart`] | the spec, and the one `build` that assembles all of it |
//!
//! # What this crate does not do
//!
//! No text shaping: widths come from a [`TextMetrics`] the caller supplies, so
//! `vellum-text` can answer exactly while the tests answer cheaply. No tessellation:
//! arcs come back as [`Arc`] parameters plus [`Arc::flatten`], and everything else is
//! already rectangles and polylines that `lyon` or an SDF shader can take directly.
//! No interaction: hit-testing a mark is the scene layer's job, and every mark
//! carries the series and category index it needs to be identified by.

pub mod axis;
pub mod bars;
pub mod chart;
pub mod colour;
pub mod data;
pub mod format;
pub mod geom;
pub mod label;
pub mod legend;
pub mod lines;
pub mod mark;
pub mod palette;
pub mod pies;
pub mod scale;
pub mod style;
pub mod text;

pub use axis::{Axis, AxisRole, AxisSide, Tick};
pub use chart::{
    BuildNotes, ChartData, ChartGeometry, ChartKind, ChartSpec, Grouping, Orientation, build,
};
pub use colour::Colour;
pub use data::{Dataset, PointSeries, PointSet, Series};
pub use format::{NumberStyle, format_value};
pub use geom::{Arc, Point, Polyline, Rect, Segment, pt};
pub use label::{Label, LabelPolicy, Placement, TextAlign, ValueLabel};
pub use legend::{Legend, LegendEntry};
pub use mark::{Area, Bar, BarEnd, Dot, Line, Mark, Slice};
pub use palette::{Palette, Theme};
pub use scale::{BandScale, TickOptions, ValueScale};
pub use style::{ChartStyle, LegendPosition};
pub use text::{MonoMetrics, TextMetrics};
