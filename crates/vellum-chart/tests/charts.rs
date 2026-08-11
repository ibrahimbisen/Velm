//! Every chart kind against every broken dataset, checked for the same things.
//!
//! The per-module tests assert what each form *does*. This file asserts what none of
//! them may do, whatever they are given: escape their frame, overlap their own
//! labels, emit a `NaN`, or panic. It is a matrix — nine forms by a dozen datasets,
//! in both themes — because the failures worth catching here are the combinations
//! nobody thinks to write a test for: a stacked bar of one negative value, a donut
//! of a single category, a scatter of one point, a line of nothing at all.
//!
//! `NaN` is singled out because of how it fails. A non-finite vertex does not panic
//! and does not warn — it silently deletes a triangle, so the bug surfaces as "one
//! bar is sometimes missing" weeks later. Every case here runs
//! [`ChartGeometry::is_finite`].

use vellum_chart::{
    ChartData, ChartGeometry, ChartKind, ChartSpec, ChartStyle, Dataset, Grouping, LabelPolicy,
    LegendPosition, Mark, MonoMetrics, Orientation, Palette, PointSeries, PointSet, Rect, Series,
    Theme, build,
};

fn kinds() -> Vec<ChartKind> {
    vec![
        ChartKind::bar(),
        ChartKind::stacked_bar(),
        ChartKind::horizontal_bar(),
        ChartKind::Bar { orientation: Orientation::Horizontal, grouping: Grouping::Stacked },
        ChartKind::line(),
        ChartKind::Line { markers: false },
        ChartKind::area(),
        ChartKind::Area { stacked: true },
        ChartKind::pie(),
        ChartKind::donut(),
    ]
}

/// The datasets that break charts, each named for what it is.
fn awkward_datasets() -> Vec<(&'static str, Dataset)> {
    vec![
        ("empty", Dataset::default()),
        ("no categories", Dataset::new(Vec::<String>::new(), [Series::new("s", [])])),
        ("one value", Dataset::single(["only"], [42.0])),
        ("one zero", Dataset::single(["only"], [0.0])),
        ("all zero", Dataset::single(["a", "b", "c"], [0.0, 0.0, 0.0])),
        ("all equal", Dataset::single(["a", "b", "c"], [7.0, 7.0, 7.0])),
        ("all negative", Dataset::single(["a", "b", "c"], [-4.0, -9.0, -1.0])),
        ("spanning zero", Dataset::single(["a", "b", "c"], [-4.0, 9.0, -1.0])),
        ("all gaps", Dataset::new(["a", "b"], [Series::with_gaps("s", [None, None])])),
        (
            "gappy",
            Dataset::new(
                ["a", "b", "c", "d"],
                [Series::with_gaps("s", [Some(1.0), None, Some(3.0), None])],
            ),
        ),
        ("huge", Dataset::single(["a", "b"], [1e300, 2e300])),
        ("tiny", Dataset::single(["a", "b"], [1e-300, 2e-300])),
        ("mixed magnitudes", Dataset::single(["a", "b"], [1e-6, 1e9])),
        (
            "nine series",
            Dataset::new(
                ["a", "b"],
                (0..9).map(|i| Series::new(format!("s{i}"), [i as f64 + 1.0, 9.0 - i as f64])),
            ),
        ),
        (
            "long names",
            Dataset::single(
                ["Rear subframe assembly", "Front suspension crossmember", "Loom"],
                [3.0, 4.0, 5.0],
            ),
        ),
    ]
}

fn styles() -> Vec<ChartStyle> {
    vec![
        ChartStyle::default(),
        ChartStyle::with_theme(Theme::Dark).with_labels(LabelPolicy::All),
        ChartStyle::default().with_labels(LabelPolicy::Extremes).with_legend(LegendPosition::Right),
        ChartStyle::default().with_labels(LabelPolicy::Endpoints).with_legend(LegendPosition::None),
        ChartStyle { gridlines: false, ..ChartStyle::default() },
    ]
}

/// Every invariant that must hold for every chart, whatever it was built from.
fn check(context: &str, geometry: &ChartGeometry) {
    assert!(geometry.is_finite(), "{context}: non-finite geometry");

    // The plot never escapes the frame it was given.
    assert!(
        geometry.plot.is_inside(&geometry.frame),
        "{context}: plot {:?} escapes frame {:?}",
        geometry.plot,
        geometry.frame
    );

    // Nor does any mark.
    for mark in &geometry.marks {
        match mark {
            Mark::Bar(bar) => assert!(
                bar.rect.is_inside(&geometry.plot),
                "{context}: bar {:?} escapes {:?}",
                bar.rect,
                geometry.plot
            ),
            Mark::Line(line) => {
                for point in &line.path.points {
                    assert!(
                        geometry.plot.inset(-0.5, -0.5).contains(*point),
                        "{context}: line point {point:?} escapes {:?}",
                        geometry.plot
                    );
                }
            }
            Mark::Area(area) => {
                for point in &area.outline.points {
                    assert!(
                        geometry.plot.inset(-0.5, -0.5).contains(*point),
                        "{context}: area point {point:?} escapes {:?}",
                        geometry.plot
                    );
                }
            }
            Mark::Dot(dot) => assert!(
                geometry.plot.inset(-0.5, -0.5).contains(dot.centre),
                "{context}: dot {:?} escapes {:?}",
                dot.centre,
                geometry.plot
            ),
            Mark::Slice(slice) => {
                let radius = slice.arc.outer_radius;
                let bounds = Rect::from_edges(
                    slice.arc.centre.x - radius,
                    slice.arc.centre.y - radius,
                    slice.arc.centre.x + radius,
                    slice.arc.centre.y + radius,
                );
                assert!(
                    bounds.is_inside(&geometry.frame),
                    "{context}: slice {bounds:?} escapes {:?}",
                    geometry.frame
                );
                assert!(slice.arc.sweep() >= 0.0, "{context}: a slice sweeps backwards");
            }
        }
    }

    // No two pieces of text overlap: value labels, axis labels and legend keys are
    // one population, and the whole point of the placer is that they stay apart.
    let mut boxes: Vec<Rect> = geometry.labels.iter().map(|label| label.label.rect).collect();
    boxes.extend(geometry.axes.iter().flat_map(|axis| axis.labels().map(|label| label.rect)));
    if let Some(legend) = &geometry.legend {
        boxes.extend(legend.entries.iter().map(|entry| entry.label.rect));
    }
    // The hundredth of a pixel is deliberate: legend rows tile exactly, and two
    // boxes that share an edge can be a float ulp apart. That is touching, not
    // overlapping, and a reader cannot see it.
    for (index, a) in boxes.iter().enumerate() {
        let a = a.inset(0.01, 0.01);
        for b in &boxes[index + 1..] {
            assert!(!a.intersects(&b.inset(0.01, 0.01)), "{context}: text {a:?} overlaps {b:?}");
        }
    }

    // Every piece of text is inside the chart. Axis labels included: they are the
    // ones that hang half outside the plot, and the overhang has to have been
    // reserved rather than allowed to run off the card.
    for label in &geometry.labels {
        assert!(
            label.label.rect.is_inside(&geometry.frame),
            "{context}: value label {:?} escapes the frame",
            label.label.rect
        );
    }
    for axis in &geometry.axes {
        for label in axis.labels() {
            assert!(
                label.rect.is_inside(&geometry.frame),
                "{context}: axis label {:?} escapes the frame {:?}",
                label.rect,
                geometry.frame
            );
        }
    }

    // The legend keeps to its own band.
    if let Some(legend) = &geometry.legend {
        assert!(
            legend.bounds.is_inside(&geometry.frame),
            "{context}: legend {:?} escapes {:?}",
            legend.bounds,
            geometry.frame
        );
    }

    // "Empty" means what it says.
    assert_eq!(geometry.notes.empty, geometry.marks.is_empty(), "{context}: notes.empty is wrong");
}

#[test]
fn every_kind_survives_every_awkward_dataset() {
    let metrics = MonoMetrics::default();
    let frame = Rect::new(12.0, 34.0, 480.0, 320.0);
    let mut checked = 0;
    for kind in kinds() {
        for (name, data) in awkward_datasets() {
            for style in styles() {
                let spec = ChartSpec::new(kind, ChartData::Categorical(data.clone()))
                    .with_style(style.clone());
                let geometry = build(&spec, frame, &metrics);
                check(&format!("{kind:?} / {name} / {:?}", style.theme), &geometry);
                checked += 1;
            }
        }
    }
    assert_eq!(checked, kinds().len() * awkward_datasets().len() * styles().len());
}

#[test]
fn every_kind_survives_every_frame_size() {
    let metrics = MonoMetrics::default();
    let data = Dataset::new(
        ["Q1", "Q2", "Q3"],
        [Series::new("a", [4.0, -9.0, 15.0]), Series::new("b", [7.0, 3.0, -2.0])],
    );
    for kind in kinds() {
        for (width, height) in [
            (0.0, 0.0),
            (1.0, 1.0),
            (16.0, 16.0),
            (40.0, 24.0),
            (120.0, 90.0),
            (480.0, 320.0),
            (4000.0, 2400.0),
            (2000.0, 40.0),
            (40.0, 2000.0),
        ] {
            let spec = ChartSpec::new(kind, ChartData::Categorical(data.clone()))
                .with_style(ChartStyle::default().with_labels(LabelPolicy::All));
            let geometry = build(&spec, Rect::new(0.0, 0.0, width, height), &metrics);
            check(&format!("{kind:?} at {width}x{height}"), &geometry);
        }
    }
}

#[test]
fn scatters_survive_every_awkward_point_set() {
    let metrics = MonoMetrics::default();
    let sets = [
        ("empty", PointSet::default()),
        ("one empty series", PointSet::new([PointSeries::new("a", [])])),
        ("one point", PointSet::new([PointSeries::new("a", [[3.0, 4.0]])])),
        (
            "identical points",
            PointSet::new([PointSeries::new("a", [[1.0, 1.0], [1.0, 1.0], [1.0, 1.0]])]),
        ),
        (
            "a vertical line of points",
            PointSet::new([PointSeries::new("a", [[5.0, 1.0], [5.0, 2.0], [5.0, 3.0]])]),
        ),
        (
            "extreme magnitudes",
            PointSet::new([PointSeries::new("a", [[1e-9, 1e9], [2e-9, 2e9]])]),
        ),
        (
            "seven series",
            PointSet::new(
                (0..7).map(|i| PointSeries::new(format!("s{i}"), [[i as f64, i as f64 * 2.0]])),
            ),
        ),
    ];
    for (name, points) in sets {
        for style in styles() {
            let spec = ChartSpec::scatter(points.clone()).with_style(style.clone());
            let geometry = build(&spec, Rect::new(0.0, 0.0, 480.0, 320.0), &metrics);
            check(&format!("scatter / {name} / {:?}", style.theme), &geometry);
        }
    }
}

/// Past the palette's capacity the sequence must not restart — a seventh series
/// wearing the first one's colour is a lie about which series it is.
#[test]
fn a_seventh_series_shares_the_overflow_neutral_and_is_reported() {
    let data = Dataset::new(
        ["a"],
        (0..9).map(|i| Series::new(format!("s{i}"), [i as f64 + 1.0])),
    );
    let geometry = build(
        &ChartSpec::categorical(ChartKind::bar(), data),
        Rect::new(0.0, 0.0, 600.0, 400.0),
        &MonoMetrics::default(),
    );
    let palette = Palette::new(Theme::Light);
    assert_eq!(geometry.notes.series_over_capacity, 3);
    for mark in &geometry.marks {
        if mark.series() >= Palette::CAPACITY {
            assert_eq!(mark.colour(), palette.overflow());
            assert_ne!(mark.colour(), palette.series(0), "the sequence must not cycle");
        }
    }
}

/// Colour follows the entity, not the row number: a filtered chart must not repaint
/// the series that survived.
#[test]
fn pinned_slots_keep_colours_stable_when_a_series_is_filtered_out() {
    let metrics = MonoMetrics::default();
    let frame = Rect::new(0.0, 0.0, 480.0, 320.0);
    let full = Dataset::new(
        ["a", "b"],
        [
            Series::new("Body", [1.0, 2.0]).with_slot(0),
            Series::new("Chassis", [3.0, 4.0]).with_slot(1),
            Series::new("Loom", [5.0, 6.0]).with_slot(2),
        ],
    );
    let filtered = Dataset::new(
        ["a", "b"],
        [
            Series::new("Body", [1.0, 2.0]).with_slot(0),
            Series::new("Loom", [5.0, 6.0]).with_slot(2),
        ],
    );

    let colour_of = |data: Dataset, name: &str| {
        let geometry = build(
            &ChartSpec::categorical(ChartKind::bar(), data.clone()),
            frame,
            &metrics,
        );
        let index = data.series().iter().position(|s| s.name == name).unwrap();
        geometry
            .marks
            .iter()
            .find(|mark| mark.series() == index)
            .map(|mark| mark.colour())
            .unwrap()
    };

    assert_eq!(colour_of(full.clone(), "Loom"), colour_of(filtered.clone(), "Loom"));
    assert_eq!(colour_of(full, "Body"), colour_of(filtered, "Body"));
}

/// Without pins the default is positional, which is the documented behaviour and
/// the reason `with_slot` exists at all.
#[test]
fn without_pins_colours_follow_position() {
    let palette = Palette::new(Theme::Light);
    let data = Dataset::new(
        ["a"],
        [Series::new("first", [1.0]), Series::new("second", [2.0])],
    );
    let geometry = build(
        &ChartSpec::categorical(ChartKind::bar(), data),
        Rect::new(0.0, 0.0, 480.0, 320.0),
        &MonoMetrics::default(),
    );
    assert_eq!(geometry.marks[0].colour(), palette.series(0));
    assert_eq!(geometry.marks[1].colour(), palette.series(1));
}

/// A legend for two or more series, none for one — identity is never carried by
/// colour alone, and a one-swatch box only restates the title.
#[test]
fn the_legend_appears_exactly_when_it_earns_its_space() {
    let metrics = MonoMetrics::default();
    let frame = Rect::new(0.0, 0.0, 480.0, 320.0);
    let one = Dataset::new(["a", "b"], [Series::new("only", [1.0, 2.0])]);
    let two = Dataset::new(
        ["a", "b"],
        [Series::new("one", [1.0, 2.0]), Series::new("two", [3.0, 4.0])],
    );
    assert!(
        build(&ChartSpec::categorical(ChartKind::line(), one), frame, &metrics)
            .legend
            .is_none()
    );
    assert!(
        build(&ChartSpec::categorical(ChartKind::line(), two), frame, &metrics)
            .legend
            .is_some()
    );
    // A pie keys its categories, so a single series still gets a legend.
    let pie = Dataset::single(["Body", "Chassis"], [1.0, 2.0]);
    let geometry = build(&ChartSpec::categorical(ChartKind::pie(), pie), frame, &metrics);
    assert_eq!(geometry.legend.unwrap().entries.len(), 2);
}

/// Both themes must resolve, and neither may leave a mark that cannot be seen
/// against its own surface.
#[test]
fn every_mark_clears_three_to_one_against_the_surface_in_both_themes() {
    let metrics = MonoMetrics::default();
    let data = Dataset::new(
        ["a", "b"],
        (0..6).map(|i| Series::new(format!("s{i}"), [i as f64 + 1.0, 6.0 - i as f64])),
    );
    for theme in [Theme::Light, Theme::Dark] {
        let spec = ChartSpec::categorical(ChartKind::bar(), data.clone())
            .with_style(ChartStyle::with_theme(theme));
        let geometry = build(&spec, Rect::new(0.0, 0.0, 600.0, 400.0), &metrics);
        for mark in &geometry.marks {
            let ratio = mark.colour().contrast_ratio(geometry.surface);
            assert!(ratio >= 3.0, "{theme:?}: {:?} is {ratio:.2}:1 on the surface", mark.colour());
        }
    }
}

/// A chart spec is stored in the document, so it has to survive a round trip
/// through the on-disk form byte for byte.
#[test]
fn a_spec_round_trips_through_serde() {
    let spec = ChartSpec::categorical(
        ChartKind::donut(),
        Dataset::new(
            ["Body", "Chassis"],
            [Series::with_gaps("Hours", [Some(12.5), None]).with_slot(3)],
        ),
    )
    .with_style(ChartStyle::with_theme(Theme::Dark).with_labels(LabelPolicy::Extremes));

    let json = serde_json::to_string(&spec).unwrap();
    let restored: ChartSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(spec, restored);

    // And the geometry it produces is identical, which is the property that
    // actually matters: a reopened board draws the same chart.
    let frame = Rect::new(0.0, 0.0, 480.0, 320.0);
    let metrics = MonoMetrics::default();
    assert_eq!(build(&spec, frame, &metrics), build(&restored, frame, &metrics));
}

/// Building the same spec twice must give the same answer — the layout runs a
/// measurement pass and a placement pass, and any hidden state between them would
/// show up as a chart that changes when it is redrawn.
#[test]
fn building_is_deterministic() {
    let metrics = MonoMetrics::default();
    let frame = Rect::new(0.0, 0.0, 480.0, 320.0);
    for kind in kinds() {
        for (_, data) in awkward_datasets() {
            let spec = ChartSpec::new(kind, ChartData::Categorical(data))
                .with_style(ChartStyle::default().with_labels(LabelPolicy::All));
            assert_eq!(build(&spec, frame, &metrics), build(&spec, frame, &metrics));
        }
    }
}
