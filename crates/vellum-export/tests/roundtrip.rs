//! The export, measured by somebody else's parser.
//!
//! `vellum_import::svg::read` was written to count what is in **Miro's** SVG export,
//! against the 36MB export of the reference board. It knows nothing about this
//! crate. Pointing it at output written here is therefore a genuine cross-check
//! rather than a restatement: if the exporter and the importer agree on how many
//! stickies, frames, shapes, previews, embeds, connectors, images and text lines a
//! board has, both are very likely right about the format, and if they disagree the
//! `SvgInventory::compare` output says which type is wrong.
//!
//! `docs/01-architecture.md` §8 makes the same argument for the import direction:
//! "the two formats come from different Miro code paths, which makes this the
//! strongest correctness signal available". This is that argument, run backwards.
//!
//! The board built below is a miniature of the real one — the same widget mix, at a
//! scale that can be asserted exactly.

use std::collections::BTreeMap;
use vellum_export::{
    Color, Geometry, ImageData, Item, ItemId, Kind, Path, Rasteriser, Rect, Scene, Scope,
    Snapshot, Stroke, Style, SubPath, csv, pdf, pt, raster, svg,
};
use vellum_export::text::{FontSpec, TextBlock};
use vellum_import::svg::{SvgInventory, read};

// The counts the fixture is built to have. Every one of them is asserted from the
// *other* side of the round trip.
const FRAMES: usize = 2;
const STICKIES: usize = 5;
const YELLOW_STICKIES: usize = 4;
const SHAPES: usize = 3;
const LINK_PREVIEWS: usize = 2;
const EMBEDS: usize = 1;
const CONNECTORS: usize = 2;
const IMAGES: usize = 1;
const INK_STROKES: usize = 2;

/// A frame at `rect`, in presentation order.
fn frame(id: u64, order: u32, rect: Rect, name: &str) -> Item {
    Item::new(id, Kind::frame(order), Geometry::rect(rect)).with_name(name)
}

fn sticky(id: u64, rect: Rect, hex: &str, frame: u64, text: &str) -> Item {
    Item::new(id, Kind::Sticky, Geometry::rect(rect))
        .in_frame(ItemId(frame))
        .with_style(Style::filled(Color::from_hex(hex).expect("a hex colour")))
        .with_text(
            TextBlock::plain(text, FontSpec::new("Noto Sans", 12.0), Color::BLACK)
                .with_padding(4.0),
        )
}

/// A card, preview or embed. Its heading goes in the text block, not in `name`:
/// the writers draw text, so a title left only in `name` would reach the CSV and
/// nothing else.
fn card(id: u64, kind: Kind, rect: Rect, title: &str) -> Item {
    Item::new(id, kind, Geometry::rounded(rect, 4.0))
        .in_frame(ItemId(200))
        .with_style(Style::filled(Color::WHITE))
        .with_name(title)
        .with_text(
            TextBlock::plain(title, FontSpec::new("Noto Sans", 13.0), Color::BLACK)
                .with_padding(8.0),
        )
}

/// A long freehand stroke.
///
/// Deliberately long: `vellum_import::svg` recognises ink by a `d` attribute of at
/// least 500 characters, because on the real board that cleanly separates 134 ink
/// paths from ~96 short decorative ones. A three-point test stroke would be
/// invisible to the oracle — which is correct behaviour, and is asserted separately
/// in `short_ink_is_below_the_oracles_floor`.
fn ink(id: u64, origin: (f64, f64), frame: u64) -> Item {
    let points: Vec<_> = (0..90)
        .map(|i| {
            let t = f64::from(i) * 0.2;
            pt(origin.0 + t * 1.7, origin.1 + (t * 0.7).sin() * 9.0)
        })
        .collect();
    Item::new(
        id,
        Kind::Ink,
        Geometry::Path(Path::new(vec![SubPath::polyline(&points, false).expect("points")])),
    )
    .in_frame(ItemId(frame))
    .with_style(Style::NONE.with_stroke(Stroke::ink(Color::from_hex("#1a1d1f").unwrap(), 2.0)))
}

fn connector(id: u64, from: (f64, f64), to: (f64, f64), frame: u64) -> Item {
    let path = Path::new(vec![
        SubPath::polyline(&[pt(from.0, from.1), pt(to.0, to.1)], false).expect("two points"),
    ]);
    Item::new(
        id,
        // One head, not two: the oracle counts arrowheads, and `compare` holds them
        // against the connector count, so a double-headed connector would read as
        // two connectors. That is a property of the format, not a bug, and
        // `two_headed_connectors_read_as_two_arrowheads` pins it.
        Kind::Connector { start: vellum_export::EndCap::None, end: vellum_export::EndCap::Arrow },
        Geometry::Path(path),
    )
    .in_frame(ItemId(frame))
    .with_style(Style::NONE.with_stroke(Stroke::new(Color::from_hex("#5c656b").unwrap(), 2.0)))
}

/// A miniature of the reference board: two frames, both filled with the widget
/// types the oracle can see.
fn board() -> Snapshot {
    let items = vec![
        frame(100, 1, Rect::new(0.0, 0.0, 600.0, 400.0), "Coolant System"),
        frame(200, 0, Rect::new(700.0, 0.0, 600.0, 400.0), "ECU"),
        // Four yellow stickies and one red, matching the reference board's split of
        // 43 `#fff79e` to 1 `#ff9e9e`.
        sticky(1, Rect::new(20.0, 20.0, 120.0, 120.0), "#fff79e", 100, "Radiator"),
        sticky(2, Rect::new(160.0, 20.0, 120.0, 120.0), "#fff79e", 100, "Oil cooler"),
        sticky(3, Rect::new(300.0, 20.0, 120.0, 120.0), "#fff79e", 100, "Thermostat"),
        sticky(4, Rect::new(720.0, 20.0, 120.0, 120.0), "#fff79e", 200, "Standalone\nECU"),
        sticky(5, Rect::new(860.0, 20.0, 120.0, 120.0), "#ff9e9e", 200, "Wiring risk"),
        // Three shapes, one of each geometry the writer can take.
        Item::new(
            6,
            Kind::Shape { name: "rectangle" },
            Geometry::rounded(Rect::new(20.0, 180.0, 160.0, 80.0), 4.0),
        )
        .in_frame(ItemId(100))
        .with_style(
            Style::filled(Color::WHITE)
                .with_stroke(Stroke::new(Color::from_hex("#e65b58").unwrap(), 2.0)),
        ),
        Item::new(7, Kind::Shape { name: "ellipse" }, Geometry::Ellipse {
            rect: Rect::new(220.0, 180.0, 160.0, 80.0),
        })
        .in_frame(ItemId(100))
        .with_style(Style::filled(Color::from_hex("#e3e6e8").unwrap())),
        Item::new(
            8,
            Kind::Shape { name: "diamond" },
            Geometry::Path(Path::from_outline(
                &vellum_shapes::Shape::Diamond.outline(2.0),
                Rect::new(420.0, 180.0, 160.0, 80.0),
            )),
        )
        .in_frame(ItemId(100))
        .with_style(Style::filled(Color::WHITE)),
        // Two link previews and an embed, as on the real board.
        card(9, Kind::LinkPreview, Rect::new(720.0, 180.0, 200.0, 90.0), "KV16")
            .with_link("https://example.com/products/kv16"),
        card(10, Kind::LinkPreview, Rect::new(940.0, 180.0, 200.0, 90.0), "Syvecs S12")
            .with_link("https://example.com/product/s12/"),
        card(11, Kind::Embed, Rect::new(720.0, 290.0, 200.0, 90.0), "Wiring walkthrough"),
        // One image.
        Item::new(12, Kind::Image, Geometry::rect(Rect::new(940.0, 290.0, 200.0, 90.0)))
            .in_frame(ItemId(200))
            .with_image(vellum_export::ImageRef::new("blake3:radiator")),
        // Two ink strokes and two connectors.
        ink(13, (30.0, 320.0), 100),
        ink(14, (300.0, 320.0), 100),
        connector(15, (180.0, 220.0), (220.0, 220.0), 100),
        connector(16, (380.0, 220.0), (420.0, 220.0), 100),
    ];
    Snapshot::new(items)
        .with_title("Reference Board")
        .with_image(
            "blake3:radiator",
            ImageData::Rgba8 { width: 2, height: 2, pixels: vec![200; 16] },
        )
}

fn scene(scope: Scope) -> Scene {
    Scene::collect(&board(), &scope).expect("the fixture has content")
}

fn inventory(scene: &Scene) -> SvgInventory {
    let svg = svg::write(scene, &svg::SvgOptions::default()).expect("an SVG");
    read(svg.as_bytes()).expect("our own SVG parses").inventory
}

/// Per-type counts as `vellum-import` reports them, so `SvgInventory::compare` can
/// be used exactly as it is on the import side.
fn expected_counts() -> BTreeMap<String, usize> {
    BTreeMap::from([
        ("sticky".to_string(), STICKIES),
        ("frame".to_string(), FRAMES),
        ("link_preview".to_string(), LINK_PREVIEWS),
        ("embed".to_string(), EMBEDS),
        ("connector".to_string(), CONNECTORS),
        ("image".to_string(), IMAGES),
        ("ink".to_string(), INK_STROKES),
    ])
}

/// The headline check: the importer's own comparison, run against the exporter.
#[test]
fn an_exported_board_reads_back_with_the_counts_it_started_with() {
    let scene = scene(Scope::Board);
    let inventory = inventory(&scene);
    let discrepancies = inventory.compare(&expected_counts());
    assert!(
        discrepancies.is_empty(),
        "the SVG oracle disagrees with the exporter:\n{}",
        discrepancies.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    );
}

/// `compare` covers six types; shapes and text are checked here, and the scene's own
/// counts are checked against the same numbers so a fixture drift fails loudly.
#[test]
fn shapes_and_text_survive_the_round_trip_too() {
    let scene = scene(Scope::Board);
    let inventory = inventory(&scene);
    // `shape_element_rects`, not `shapes` — the field was renamed in
    // `vellum-import` when the "Miro shapes do not import" defect was withdrawn
    // as a phantom (it counts Miro's generic rect primitive, which on the
    // reference board is widget chrome rather than shape widgets). This test
    // never followed the rename and had not compiled since.
    assert_eq!(inventory.shape_element_rects, SHAPES);

    // Every sticky's text, plus the second line of the two-line one, plus both
    // preview titles, the embed's title, and the two frame captions.
    let sticky_lines = STICKIES + 1;
    let card_titles = LINK_PREVIEWS + EMBEDS;
    assert_eq!(inventory.text_strings, sticky_lines + card_titles + FRAMES);

    let counts = scene.counts();
    assert_eq!(counts.get("sticky"), Some(&STICKIES));
    assert_eq!(counts.get("shape"), Some(&SHAPES));
    assert_eq!(counts.get("frame"), Some(&FRAMES));
}

/// Colour survives losslessly through both formats, which makes it the sharpest
/// single check available — the same argument `vellum_import::svg` makes for the
/// import direction.
#[test]
fn sticky_colours_come_back_exactly() {
    let inventory = inventory(&scene(Scope::Board));
    assert_eq!(inventory.sticky_colors.get("#fff79e"), Some(&YELLOW_STICKIES));
    assert_eq!(inventory.sticky_colors.get("#ff9e9e"), Some(&(STICKIES - YELLOW_STICKIES)));
    assert_eq!(inventory.sticky_colors.values().sum::<usize>(), STICKIES);
}

#[test]
fn the_boards_extent_comes_back_from_the_root_element() {
    let scene = scene(Scope::Board);
    // Frame captions are drawn above their frames, so with them on the view is
    // taller than the painted bounds by exactly the caption strip. With them off it
    // is the bounds, to the decimal.
    let options = svg::SvgOptions { frame_titles: false, ..svg::SvgOptions::default() };
    let svg = svg::write(&scene, &options).unwrap();
    let export = read(svg.as_bytes()).unwrap();
    assert!((export.extent.width - scene.bounds.width).abs() < 0.01, "{:?}", export.extent);
    assert!((export.extent.height - scene.bounds.height).abs() < 0.01, "{:?}", export.extent);

    let captioned = read(
        svg::write(&scene, &svg::SvgOptions::default()).unwrap().as_bytes(),
    )
    .unwrap();
    assert_eq!(captioned.extent.width, export.extent.width, "width is unchanged");
    assert!(
        captioned.extent.height > export.extent.height,
        "the captions must fit inside the view box: {:?}",
        captioned.extent
    );
}

/// Exporting one frame must produce that frame's contents and nothing else — the
/// oracle counts it independently of the scene that produced it.
#[test]
fn a_single_frame_export_contains_only_that_frame() {
    let inventory = inventory(&scene(Scope::Frame(ItemId(200))));
    assert_eq!(inventory.frames, 1);
    assert_eq!(inventory.stickies, 2);
    assert_eq!(inventory.link_previews, LINK_PREVIEWS);
    assert_eq!(inventory.embeds, EMBEDS);
    assert_eq!(inventory.shape_element_rects, 0, "every shape is in the other frame");
    assert_eq!(inventory.ink_paths, 0);
}

#[test]
fn a_selection_export_contains_only_the_selection() {
    let inventory = inventory(&scene(Scope::selection([ItemId(1), ItemId(5)])));
    assert_eq!(inventory.stickies, 2);
    assert_eq!(inventory.frames, 0);
    assert_eq!(inventory.sticky_colors.get("#fff79e"), Some(&1));
    assert_eq!(inventory.sticky_colors.get("#ff9e9e"), Some(&1));
}

/// The oracle's ink threshold is a floor, and this documents which side of it a
/// short stroke falls on. Getting this wrong in the other direction — inflating
/// `ink_paths` with short decorative paths — is what the threshold exists to
/// prevent.
#[test]
fn short_ink_is_below_the_oracles_floor() {
    let path = Path::new(vec![
        SubPath::polyline(&[pt(0.0, 0.0), pt(10.0, 10.0)], false).unwrap(),
    ]);
    let items = vec![
        Item::new(1, Kind::Ink, Geometry::Path(path))
            .with_style(Style::NONE.with_stroke(Stroke::ink(Color::BLACK, 2.0))),
    ];
    let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
    assert_eq!(inventory(&scene).ink_paths, 0, "a two-point stroke is under 500 characters");

    // And `compare` treats that as acceptable rather than as a discrepancy, because
    // the import may legitimately hold more ink than the SVG shows.
    let counts = BTreeMap::from([("ink".to_string(), 1)]);
    assert!(inventory(&scene).compare(&counts).is_empty());
}

/// One shape on its own board, for the shape-level checks below.
fn shape_scene(shape: vellum_shapes::Shape, rect: Rect) -> Scene {
    let items = vec![
        Item::new(
            1,
            Kind::Shape { name: shape.name() },
            Geometry::Path(Path::from_outline(&shape.outline((rect.width / rect.height) as f32), rect)),
        )
        .with_style(Style::filled(Color::WHITE)),
    ];
    Scene::collect(&Snapshot::new(items), &Scope::Board).expect("a shape")
}

/// A straight-edged silhouette is written as `<polygon>`, so however large it is it
/// can never be mistaken for a stroke.
#[test]
fn a_polygonal_shape_is_never_mistaken_for_ink() {
    for shape in [
        vellum_shapes::Shape::Diamond,
        vellum_shapes::Shape::star(),
        vellum_shapes::Shape::cross(),
        vellum_shapes::Shape::octagon(),
    ] {
        let inventory = inventory(&shape_scene(shape, Rect::new(0.0, 0.0, 4000.0, 4000.0)));
        assert_eq!(inventory.shape_element_rects, 1, "{}", shape.name());
        assert_eq!(inventory.ink_paths, 0, "{} was counted as ink", shape.name());
    }
}

/// The documented limit of the oracle's ink heuristic.
///
/// A curved silhouette has no form shorter than a path, so a large one crosses the
/// reader's 500-character ink threshold and is counted as ink *as well as* as a
/// shape. The shape count stays exact; only the ink floor is inflated. This is
/// pinned rather than worked around, because the alternative — flattening curves to
/// keep the attribute short — would trade real fidelity for a heuristic's comfort.
#[test]
fn a_curved_shape_outline_can_cross_the_oracles_ink_threshold() {
    let inventory = inventory(&shape_scene(
        vellum_shapes::Shape::Cloud,
        Rect::new(0.0, 0.0, 400.0, 400.0),
    ));
    assert_eq!(inventory.shape_element_rects, 1, "the shape count is unaffected");
    assert_eq!(inventory.ink_paths, 1, "and this is the known overlap");
}

/// Every shape in the catalogue exports, and every one of them reads back as
/// exactly one shape. This is the check that a new shape cannot quietly break the
/// export.
#[test]
fn every_catalogue_shape_exports_and_reads_back_as_one_shape() {
    for shape in vellum_shapes::CATALOGUE {
        let inventory = inventory(&shape_scene(*shape, Rect::new(0.0, 0.0, 300.0, 200.0)));
        assert_eq!(inventory.shape_element_rects, 1, "{} exported as {} shapes", shape.name(), inventory.shape_element_rects);
    }
}

#[test]
fn two_headed_connectors_read_as_two_arrowheads() {
    use vellum_export::EndCap;
    let path = Path::new(vec![
        SubPath::polyline(&[pt(0.0, 0.0), pt(100.0, 0.0)], false).unwrap(),
    ]);
    let items = vec![
        Item::new(1, Kind::Connector { start: EndCap::Arrow, end: EndCap::Arrow }, Geometry::Path(path))
            .with_style(Style::NONE.with_stroke(Stroke::new(Color::BLACK, 2.0))),
    ];
    let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
    assert_eq!(inventory(&scene).connector_arrowheads, 2);
}

/// Turning image embedding off must remove the payload and nothing else.
#[test]
fn images_omitted_are_absent_from_the_read_back() {
    let scene = scene(Scope::Board);
    let options = svg::SvgOptions { embed_images: false, ..svg::SvgOptions::default() };
    let svg = svg::write(&scene, &options).unwrap();
    let inventory = read(svg.as_bytes()).unwrap().inventory;
    assert_eq!(inventory.embedded_images, 0);
    assert_eq!(inventory.stickies, STICKIES, "nothing else changed");
}

/// A compact SVG must parse identically to an indented one — indentation is
/// whitespace, and whitespace must not change a count.
#[test]
fn indentation_does_not_change_what_is_read_back() {
    let scene = scene(Scope::Board);
    let pretty = svg::write(&scene, &svg::SvgOptions::default()).unwrap();
    let compact =
        svg::write(&scene, &svg::SvgOptions { indent: false, ..svg::SvgOptions::default() })
            .unwrap();
    assert!(compact.len() < pretty.len(), "compact output should be smaller");
    assert_eq!(read(pretty.as_bytes()).unwrap().inventory, read(compact.as_bytes()).unwrap().inventory);
}

// ---------------------------------------------------------------------------
// The other three formats, on the same fixture.
// ---------------------------------------------------------------------------

#[test]
fn the_pdf_has_one_page_per_frame_in_presentation_order() {
    let scene = scene(Scope::Board);
    let bytes = pdf::write(&scene, &pdf::PdfOptions::new()).expect("a PDF");
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(&bytes[..5], b"%PDF-");
    assert!(text.contains(&format!("/Count {FRAMES}")), "{text}");
    assert!(text.trim_end().ends_with("%%EOF"));

    // Both frames are 600×400, so both pages are the same size; presentation order
    // is checked by the scene, which the PDF walks in order.
    assert_eq!(scene.frames.iter().map(|f| f.id.0).collect::<Vec<_>>(), vec![200, 100]);
}

#[test]
fn the_csv_holds_every_sticky_and_card_with_its_frame() {
    let scene = scene(Scope::Board);
    let rows = csv::rows(&scene, &csv::CsvOptions::default());
    assert_eq!(rows.len(), STICKIES + LINK_PREVIEWS + EMBEDS);
    assert!(rows.iter().any(|r| r.frame == "Coolant System" && r.text == "Radiator"));
    assert!(
        rows.iter()
            .any(|r| r.frame == "ECU" && r.link == "https://example.com/products/kv16")
    );
    // Presentation order: the ECU frame is first.
    assert_eq!(rows[0].frame, "ECU");

    let text = csv::write(&scene, &csv::CsvOptions::default()).unwrap();
    assert!(text.starts_with('\u{feff}'));
    assert!(text.contains("frame,type,text,link\n"));
    // The two-line sticky keeps its break, quoted.
    assert!(text.contains("\"Standalone\nECU\""), "{text}");
}

#[test]
fn the_png_covers_the_board_and_paints_the_stickies() {
    let scene = scene(Scope::Board);
    let request = raster::RasterRequest::scene(&scene, 1.0)
        .with_background(Color::from_hex("#e3e6e8").unwrap());
    let image = raster::CpuRasteriser.rasterise(&scene, &request).expect("a raster");
    assert_eq!(image.width, scene.bounds.width.ceil() as u32);
    assert_eq!(image.height, scene.bounds.height.ceil() as u32);

    // The first sticky sits at (20,20)-(140,140) in board space, and the scene's
    // origin is (0,0), so its centre is a yellow pixel.
    assert_eq!(image.pixel(80, 80), Some(Color::from_hex("#fff79e").unwrap()));
    // The board's background shows between the frames.
    assert_eq!(image.pixel(650, 380), Some(Color::from_hex("#e3e6e8").unwrap()));

    // No font was supplied, so the text is reported rather than silently missing.
    assert!(
        image.warnings.iter().any(|w: &String| w.contains("Noto Sans")),
        "expected a font warning, got {:?}",
        image.warnings
    );

    let png = image.encode_png().expect("PNG encoding");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
}

/// All four writers work from the same [`Scene`], so a frame export must contain the
/// same items in every format. This is the check that the four cannot drift apart.
#[test]
fn every_format_agrees_on_what_one_frame_contains() {
    let scene = scene(Scope::Frame(ItemId(200)));

    let inventory = inventory(&scene);
    assert_eq!(inventory.stickies, 2);

    let rows = csv::rows(&scene, &csv::CsvOptions::default());
    assert_eq!(rows.len(), 2 + LINK_PREVIEWS + EMBEDS);
    assert!(rows.iter().all(|r| r.frame == "ECU"));

    let bytes = pdf::write(&scene, &pdf::PdfOptions::new()).expect("a PDF");
    assert!(String::from_utf8_lossy(&bytes).contains("/Count 1"));

    let request = raster::RasterRequest::scene(&scene, 1.0);
    let image = raster::CpuRasteriser.rasterise(&scene, &request).expect("a raster");
    assert_eq!(image.width, 600);
    assert_eq!(image.height, 400);
}
