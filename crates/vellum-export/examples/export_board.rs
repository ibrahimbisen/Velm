//! Writes one small board out in all four formats, side by side.
//!
//! ```text
//! cargo run -p vellum-export --example export_board -- /tmp/out
//! ```
//!
//! It exists to be *looked at*. Unit tests can prove that a `<use>` element carries
//! the right `xlink:href`; only a pair of eyes can tell you the arrowhead is on the
//! wrong end or the text is sitting outside its sticky. Four files from one scene
//! also make the writers directly comparable, which is the property the crate is
//! built around.

use std::path::PathBuf;
use vellum_export::text::{Align, FontSpec, TextBlock, VAlign};
use vellum_export::{
    Color, CpuRasteriser, EndCap, Geometry, Item, ItemId, Kind, Path, RasterRequest, Rect, Scene,
    Scope, Snapshot, Stroke, Style, SubPath, csv, pdf, pt, raster, svg,
};

const YELLOW: &str = "#fff79e";
const RED: &str = "#e65b58";
const INK: &str = "#1a1d1f";
const FROST: &str = "#dde2e5";

fn sticky(id: u64, rect: Rect, text: &str) -> Item {
    Item::new(id, Kind::Sticky, Geometry::rect(rect))
        .in_frame(ItemId(1))
        .with_style(Style::filled(Color::from_hex(YELLOW).unwrap()))
        .with_text(
            TextBlock::plain(text, FontSpec::new("Inter", 15.0), Color::from_hex(INK).unwrap())
                .with_align(Align::Center)
                .with_valign(VAlign::Middle)
                .with_padding(10.0),
        )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out: PathBuf = std::env::args().nth(1).unwrap_or_else(|| "export-demo".into()).into();
    std::fs::create_dir_all(&out)?;

    let items = vec![
        Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 640.0, 420.0)))
            .with_name("Coolant System")
            .with_style(
                Style::filled(Color::WHITE)
                    .with_stroke(Stroke::new(Color::from_hex(FROST).unwrap(), 1.0)),
            ),
        sticky(2, Rect::new(40.0, 60.0, 150.0, 150.0), "Radiator"),
        sticky(3, Rect::new(240.0, 60.0, 150.0, 150.0), "Oil cooler"),
        Item::new(4, Kind::Sticky, Geometry::rect(Rect::new(440.0, 60.0, 150.0, 150.0)))
            .in_frame(ItemId(1))
            .with_style(Style::filled(Color::from_hex("#ff9e9e").unwrap()))
            .with_text(
                TextBlock::plain(
                    "Thermostat\nhousing",
                    FontSpec::new("Inter", 15.0),
                    Color::from_hex(INK).unwrap(),
                )
                .with_align(Align::Center)
                .with_valign(VAlign::Middle)
                .with_padding(10.0),
            ),
        // Two connectors chaining the stickies left to right.
        connector(5, (190.0, 135.0), (240.0, 135.0)),
        connector(6, (390.0, 135.0), (440.0, 135.0)),
        // A shape and a piece of ink underneath them.
        Item::new(
            7,
            Kind::Shape { name: "cylinder" },
            Geometry::Path(Path::from_outline(
                &vellum_shapes::Shape::cylinder().outline(1.5),
                Rect::new(40.0, 260.0, 180.0, 120.0),
            )),
        )
        .in_frame(ItemId(1))
        .with_style(
            Style::filled(Color::from_hex(FROST).unwrap())
                .with_stroke(Stroke::new(Color::from_hex(INK).unwrap(), 1.5)),
        ),
        Item::new(8, Kind::Ink, Geometry::Path(wave()))
            .in_frame(ItemId(1))
            .with_style(Style::NONE.with_stroke(Stroke::ink(Color::from_hex(RED).unwrap(), 3.0))),
    ];

    let board = Snapshot::new(items).with_title("Export demo");
    let scene = Scene::collect(&board, &Scope::Board)?;

    svg::write_file(&scene, &svg::SvgOptions::default(), out.join("board.svg"))?;
    pdf::write_file(&scene, &pdf::PdfOptions::new(), out.join("board.pdf"))?;
    csv::write_file(&scene, &csv::CsvOptions::default(), out.join("board.csv"))?;

    let request =
        RasterRequest::scene(&scene, 2.0).with_background(Color::from_hex("#e3e6e8").unwrap());
    raster::write_file(&mut CpuRasteriser, &scene, &request, out.join("board.png"))?;

    println!("wrote board.svg, board.pdf, board.csv and board.png to {}", out.display());
    Ok(())
}

fn connector(id: u64, from: (f64, f64), to: (f64, f64)) -> Item {
    let path = Path::new(vec![
        SubPath::polyline(&[pt(from.0, from.1), pt(to.0, to.1)], false).unwrap(),
    ]);
    Item::new(id, Kind::Connector { start: EndCap::None, end: EndCap::Arrow }, Geometry::Path(path))
        .in_frame(ItemId(1))
        .with_style(Style::NONE.with_stroke(Stroke::new(Color::from_hex(INK).unwrap(), 2.0)))
}

fn wave() -> Path {
    let points: Vec<_> = (0..=120)
        .map(|i| {
            let t = f64::from(i);
            pt(280.0 + t * 2.8, 320.0 + (t * 0.09).sin() * 34.0)
        })
        .collect();
    Path::new(vec![SubPath::polyline(&points, false).unwrap()])
}
