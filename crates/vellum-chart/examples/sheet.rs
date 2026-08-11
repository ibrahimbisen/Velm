//! Renders one of every chart to a single SVG, so the geometry can be *looked at*.
//!
//! Unit tests prove a bar is inside the plot and two labels do not overlap. They
//! cannot see that an axis crowds its plot, that a donut's hole swallows its ring,
//! or that a legend sits too close to the top bar. This example exists so those are
//! caught by the only instrument that catches them.
//!
//! It is also a worked example of consuming [`ChartGeometry`]: an SVG back end in
//! 200 lines, with no chart-specific knowledge beyond "draw the marks in order".
//!
//! ```text
//! cargo run -p vellum-chart --example sheet -- /tmp/charts.svg
//! cargo run -p vellum-chart --example sheet -- /tmp/charts-dark.svg dark
//! cargo run -p vellum-chart --example sheet -- /tmp/one.svg light 4 5   # a slice
//! ```
//!
//! The trailing pair is a half-open range over the gallery, for when one chart is
//! being worked on and a wall of nine is in the way.

use std::fmt::Write as _;

use vellum_chart::{
    ChartGeometry, ChartKind, ChartSpec, ChartStyle, Colour, Dataset, LabelPolicy, LegendPosition,
    Mark, MonoMetrics, PointSeries, PointSet, Rect, Series, TextAlign, Theme, build,
};

const CELL: (f32, f32) = (420.0, 300.0);
const COLUMNS: usize = 2;
const GUTTER: f32 = 24.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "/tmp/vellum-charts.svg".to_string());
    let theme = match args.next().as_deref() {
        Some("dark") => Theme::Dark,
        _ => Theme::Light,
    };

    let metrics = MonoMetrics::default();
    let all = gallery(theme);
    let first = args.next().and_then(|a| a.parse().ok()).unwrap_or(0).min(all.len());
    let last = args.next().and_then(|a| a.parse().ok()).unwrap_or(all.len()).clamp(first, all.len());
    let charts = &all[first..last];
    let rows = charts.len().div_ceil(COLUMNS);
    let width = COLUMNS as f32 * CELL.0 + (COLUMNS + 1) as f32 * GUTTER;
    let height = rows as f32 * CELL.1 + (rows + 1) as f32 * GUTTER;

    let palette = vellum_chart::Palette::new(theme);
    let mut svg = String::new();
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="ui-monospace, SFMono-Regular, Menlo, monospace">"#
    );
    let _ = write!(
        svg,
        r#"<rect width="{width}" height="{height}" fill="{}"/>"#,
        hex(if theme == Theme::Light { Colour::hex(0xE3E6E8) } else { Colour::hex(0x1C1F23) })
    );

    for (index, (title, spec)) in charts.iter().enumerate() {
        let column = index % COLUMNS;
        let row = index / COLUMNS;
        let frame = Rect::new(
            GUTTER + column as f32 * (CELL.0 + GUTTER),
            GUTTER + row as f32 * (CELL.1 + GUTTER),
            CELL.0,
            CELL.1,
        );
        // The chart card itself: `bone`, hairline `frost` border, 4px radius.
        let _ = write!(
            svg,
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="4" fill="{}" stroke="{}"/>"#,
            frame.x,
            frame.y,
            frame.width,
            frame.height,
            hex(palette.surface),
            hex(palette.grid)
        );
        let geometry = build(spec, frame.inset(8.0, 8.0), &metrics);
        render(&mut svg, &geometry, title, &palette);
    }
    svg.push_str("</svg>");

    std::fs::write(&path, svg).expect("writing the sheet");
    println!("wrote {path}");
}

fn gallery(theme: Theme) -> Vec<(&'static str, ChartSpec)> {
    let style = || ChartStyle::with_theme(theme);
    let quarters = || {
        Dataset::new(
            ["Q1", "Q2", "Q3", "Q4"],
            [
                Series::new("Body", [18.0, 47.4, 71.1, 94.8]),
                Series::new("Chassis", [32.0, 28.0, 45.0, 61.0]),
                Series::new("Loom", [12.0, 19.0, 24.0, 38.0]),
            ],
        )
    };
    vec![
        (
            "Grouped bars",
            ChartSpec::categorical(ChartKind::bar(), quarters()).with_style(style()),
        ),
        (
            "Stacked bars",
            ChartSpec::categorical(ChartKind::stacked_bar(), quarters()).with_style(style()),
        ),
        (
            "Horizontal bars, labelled",
            ChartSpec::categorical(
                ChartKind::horizontal_bar(),
                Dataset::single(
                    ["Rear subframe", "Front crossmember", "Loom", "Interior"],
                    [1240.0, 880.0, 2310.0, 640.0],
                ),
            )
            .with_style(style().with_labels(LabelPolicy::All).with_legend(LegendPosition::None)),
        ),
        (
            "Diverging bars",
            ChartSpec::categorical(
                ChartKind::bar(),
                Dataset::single(["Jan", "Feb", "Mar", "Apr", "May"], [12.0, -8.0, 22.0, -3.0, 17.0]),
            )
            .with_style(style().with_legend(LegendPosition::None)),
        ),
        (
            "Line, extremes labelled",
            ChartSpec::categorical(ChartKind::line(), quarters())
                .with_style(style().with_labels(LabelPolicy::Extremes)),
        ),
        (
            "Stacked area",
            ChartSpec::categorical(ChartKind::Area { stacked: true }, quarters())
                .with_style(style()),
        ),
        (
            "Scatter",
            ChartSpec::scatter(PointSet::new([
                PointSeries::new(
                    "Run A",
                    [[100.0, 51.2], [110.0, 52.5], [120.0, 49.0], [130.0, 55.4], [140.0, 53.1]],
                ),
                PointSeries::new(
                    "Run B",
                    [[105.0, 44.0], [115.0, 47.2], [125.0, 41.5], [135.0, 46.9]],
                ),
            ]))
            .with_style(style()),
        ),
        (
            "Donut, shares labelled",
            ChartSpec::categorical(
                ChartKind::donut(),
                Dataset::single(["Body", "Chassis", "Interior", "Electrical"], [40.0, 30.0, 20.0, 10.0]),
            )
            .with_style(style().with_labels(LabelPolicy::All)),
        ),
        (
            "Gaps, one value, awkward range",
            ChartSpec::categorical(
                ChartKind::line(),
                Dataset::new(
                    ["a", "b", "c", "d", "e", "f"],
                    [Series::with_gaps(
                        "Sparse",
                        [Some(0.037), None, Some(0.041), Some(0.0395), None, Some(0.044)],
                    )],
                ),
            )
            .with_style(style().with_labels(LabelPolicy::Endpoints)),
        ),
    ]
}

fn render(svg: &mut String, geometry: &ChartGeometry, title: &str, palette: &vellum_chart::Palette) {
    // Gridlines and rules first: chrome sits under the data.
    for axis in &geometry.axes {
        for tick in &axis.ticks {
            if let Some(line) = tick.gridline {
                let _ = write!(
                    svg,
                    r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{}" stroke-width="1"/>"#,
                    line.from.x, line.from.y, line.to.x, line.to.y, hex(axis.colour)
                );
            }
        }
        if let Some(rule) = axis.rule {
            let _ = write!(
                svg,
                r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{}" stroke-width="1"/>"#,
                rule.from.x, rule.from.y, rule.to.x, rule.to.y, hex(axis.colour)
            );
        }
    }
    if let Some(baseline) = geometry.baseline {
        let _ = write!(
            svg,
            r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{}" stroke-width="1"/>"#,
            baseline.from.x,
            baseline.from.y,
            baseline.to.x,
            baseline.to.y,
            hex(geometry.baseline_colour)
        );
    }

    for mark in &geometry.marks {
        match mark {
            Mark::Bar(bar) => {
                // The 4px round belongs to the value end only; SVG has no per-corner
                // radius, so the rounded rect is drawn as a path.
                let _ = write!(svg, r#"<path d="{}" fill="{}"/>"#, bar_path(bar), hex(bar.colour));
            }
            Mark::Line(line) => {
                let _ = write!(
                    svg,
                    r#"<polyline points="{}" fill="none" stroke="{}" stroke-width="{}" stroke-linejoin="round" stroke-linecap="round"/>"#,
                    points(&line.path.points),
                    hex(line.colour),
                    line.width
                );
            }
            Mark::Area(area) => {
                let _ = write!(
                    svg,
                    r#"<polygon points="{}" fill="{}" fill-opacity="{:.3}"/>"#,
                    points(&area.outline.points),
                    hex(area.fill),
                    area.fill.a as f32 / 255.0
                );
            }
            Mark::Dot(dot) => {
                let _ = write!(
                    svg,
                    r#"<circle cx="{}" cy="{}" r="{}" fill="{}" stroke="{}" stroke-width="{}"/>"#,
                    dot.centre.x,
                    dot.centre.y,
                    dot.radius,
                    hex(dot.colour),
                    hex(dot.ring_colour),
                    dot.ring_width
                );
            }
            Mark::Slice(slice) => {
                let _ = write!(
                    svg,
                    r#"<polygon points="{}" fill="{}" stroke="{}" stroke-width="2"/>"#,
                    points(&slice.arc.flatten(0.2)),
                    hex(slice.colour),
                    hex(geometry.surface)
                );
            }
        }
    }

    for axis in &geometry.axes {
        for label in axis.labels() {
            text(svg, label);
        }
    }
    for label in &geometry.labels {
        if let Some(leader) = label.leader {
            let _ = write!(
                svg,
                r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{}" stroke-width="1"/>"#,
                leader.from.x, leader.from.y, leader.to.x, leader.to.y, hex(palette.text_faint)
            );
        }
        text(svg, &label.label);
    }
    if let Some(legend) = &geometry.legend {
        for entry in &legend.entries {
            let _ = write!(
                svg,
                r#"<rect x="{}" y="{}" width="{}" height="{}" rx="2" fill="{}"/>"#,
                entry.swatch.x,
                entry.swatch.y,
                entry.swatch.width,
                entry.swatch.height,
                hex(entry.colour)
            );
            text(svg, &entry.label);
        }
    }

    // The card's own title, in the frame's top-left — not part of the geometry, so
    // it is drawn here the way a real widget would draw it.
    let _ = write!(
        svg,
        r#"<text x="{}" y="{}" font-size="11" fill="{}" letter-spacing="0.6" text-transform="uppercase">{}</text>"#,
        geometry.frame.x,
        geometry.frame.y - 6.0,
        hex(palette.text_muted),
        title.to_uppercase()
    );
}

fn text(svg: &mut String, label: &vellum_chart::Label) {
    let (x, anchor) = match label.align {
        TextAlign::Start => (label.rect.left(), "start"),
        TextAlign::Centre => (label.rect.centre_x(), "middle"),
        TextAlign::End => (label.rect.right(), "end"),
    };
    let _ = write!(
        svg,
        r#"<text x="{x}" y="{}" font-size="{}" fill="{}" text-anchor="{anchor}" dominant-baseline="central">{}</text>"#,
        label.rect.centre_y(),
        label.size,
        hex(label.colour),
        escape(&label.text)
    );
}

/// A bar as a path, rounded on its value end only.
fn bar_path(bar: &vellum_chart::Bar) -> String {
    use vellum_chart::BarEnd;
    let r = bar.radius.min(bar.rect.width * 0.5).min(bar.rect.height * 0.5);
    let (l, t, right, b) = (bar.rect.left(), bar.rect.top(), bar.rect.right(), bar.rect.bottom());
    if r <= 0.0 || bar.rounded_end == BarEnd::None {
        return format!("M{l} {t}H{right}V{b}H{l}Z");
    }
    match bar.rounded_end {
        BarEnd::Top => format!(
            "M{l} {b}V{} A{r} {r} 0 0 1 {} {t} H{} A{r} {r} 0 0 1 {right} {} V{b} Z",
            t + r,
            l + r,
            right - r,
            t + r
        ),
        BarEnd::Bottom => format!(
            "M{l} {t}V{} A{r} {r} 0 0 0 {} {b} H{} A{r} {r} 0 0 0 {right} {} V{t} Z",
            b - r,
            l + r,
            right - r,
            b - r
        ),
        BarEnd::Right => format!(
            "M{l} {t}H{} A{r} {r} 0 0 1 {right} {} V{} A{r} {r} 0 0 1 {} {b} H{l} Z",
            right - r,
            t + r,
            b - r,
            right - r
        ),
        BarEnd::Left => format!(
            "M{right} {t}H{} A{r} {r} 0 0 0 {l} {} V{} A{r} {r} 0 0 0 {} {b} H{right} Z",
            l + r,
            t + r,
            b - r,
            l + r
        ),
        BarEnd::None => unreachable!("handled above"),
    }
}

fn points(points: &[vellum_chart::Point]) -> String {
    points.iter().map(|p| format!("{},{}", p.x, p.y)).collect::<Vec<_>>().join(" ")
}

fn hex(colour: Colour) -> String {
    format!("#{:02x}{:02x}{:02x}", colour.r, colour.g, colour.b)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
