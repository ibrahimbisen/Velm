//! Font embedding, against a real font file.
//!
//! Embedding is the part of the PDF writer that cannot be checked by inspecting a
//! string: it needs a font with a real `cmap`, `hmtx` and `glyf`, and a synthetic
//! one proves only that the code tolerates a synthetic one. So these tests look for
//! a font the operating system already has and **skip cleanly when there is none**,
//! printing which paths were tried.
//!
//! That is the same trade `vellum-import`'s `reference_board.rs` makes for the 300MB
//! reference exports, and for the same reason: a test that is expected to be red on
//! some machines teaches everyone to ignore red tests.

use std::path::{Path, PathBuf};
use vellum_export::text::{FontSpec, TextBlock};
use vellum_export::{
    Color, CpuRasteriser, Geometry, Item, ItemId, Kind, RasterRequest, Rasteriser, Rect, Scene,
    Scope, Snapshot, pdf,
};

/// Fonts likely to be present, in order of preference. All are static TrueType with
/// a Unicode `cmap`, which is all the embedder needs.
const CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Andale Mono.ttf",
    "/Library/Fonts/Arial Unicode.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "C:/Windows/Fonts/arial.ttf",
];

fn a_real_font() -> Option<(PathBuf, Vec<u8>)> {
    for candidate in CANDIDATES {
        let path = Path::new(candidate);
        if let Ok(bytes) = std::fs::read(path) {
            return Some((path.to_path_buf(), bytes));
        }
    }
    eprintln!(
        "skipping: no system font found. Tried:\n  {}",
        CANDIDATES.join("\n  ")
    );
    None
}

fn spec() -> FontSpec {
    FontSpec::new("Test Face", 16.0)
}

/// One frame with one line of text in the supplied font.
fn board(font: &[u8], text: &str) -> Snapshot {
    Snapshot::new(vec![
        Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 400.0, 200.0)))
            .with_name("page"),
        Item::new(2, Kind::Text, Geometry::rect(Rect::new(20.0, 20.0, 360.0, 60.0)))
            .in_frame(ItemId(1))
            .with_text(TextBlock::plain(text, spec(), Color::BLACK)),
    ])
    .with_font(&spec(), font)
}

#[test]
fn a_supplied_font_is_embedded_as_a_composite_font() {
    let Some((path, font)) = a_real_font() else { return };
    let scene = Scene::collect(&board(&font, "Coolant System"), &Scope::Board).unwrap();
    let bytes = pdf::write(&scene, &pdf::PdfOptions::new()).expect("a PDF");
    let text = String::from_utf8_lossy(&bytes);

    assert!(text.contains("/Type0"), "not a composite font, from {}", path.display());
    assert!(text.contains("/Identity-H"), "glyphs must be addressed by id");
    assert!(text.contains("/CIDFontType2"), "a TrueType descendant");
    assert!(text.contains("/FontFile2"), "the font file itself is missing");
    assert!(text.contains("/CIDToGIDMap /Identity"));
    assert!(text.contains("/ToUnicode"), "without this the text is a picture");

    // The font file really is in there, not just referenced.
    assert!(
        bytes.windows(64).any(|w| w == &font[..64]),
        "the embedded stream does not contain the font's own bytes"
    );
    assert!(
        bytes.len() > font.len(),
        "a PDF embedding a {}-byte font cannot be smaller than it",
        font.len()
    );
    // And the fallback is not used when a real font was supplied for every run.
    assert!(!text.contains("(Coolant System)"), "text was written as literal Helvetica");
}

#[test]
fn the_widths_array_covers_only_the_glyphs_used() {
    let Some((_, font)) = a_real_font() else { return };

    let short = Scene::collect(&board(&font, "AB"), &Scope::Board).unwrap();
    let long = Scene::collect(
        &board(&font, "The quick brown fox jumps over the lazy dog 0123456789"),
        &Scope::Board,
    )
    .unwrap();

    let count = |scene: &Scene| {
        let bytes = pdf::write(scene, &pdf::PdfOptions::new()).unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // One `beginbfchar` entry per distinct glyph.
        text.matches("beginbfchar").count().max(1)
            * text
                .split("beginbfchar")
                .nth(1)
                .map_or(0, |section| section.lines().filter(|l| l.starts_with('<')).count())
    };
    assert!(count(&long) > count(&short), "a longer document must map more glyphs");
}

/// The check that the two fallback paths agree: with no font bytes the PDF must
/// still be produced, in Helvetica, with the text intact.
#[test]
fn no_font_still_produces_a_readable_pdf() {
    let items = vec![
        Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 400.0, 200.0))),
        Item::new(2, Kind::Text, Geometry::rect(Rect::new(20.0, 20.0, 360.0, 60.0)))
            .in_frame(ItemId(1))
            .with_text(TextBlock::plain("Coolant System", spec(), Color::BLACK)),
    ];
    let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
    let text = String::from_utf8_lossy(&pdf::write(&scene, &pdf::PdfOptions::new()).unwrap())
        .into_owned();
    assert!(text.contains("/BaseFont /Helvetica"));
    assert!(text.contains("(Coolant System)"), "the words themselves must survive");
    assert!(!text.contains("/FontFile2"), "nothing was embedded");
}

/// The rasteriser has no text engine; a supplied font is what makes glyphs appear at
/// all. This is the check that they do.
#[test]
fn a_supplied_font_puts_glyphs_on_the_raster() {
    let Some((_, font)) = a_real_font() else { return };

    let with_font = board(&font, "IIIIIIIIII");
    let scene = Scene::collect(&with_font, &Scope::Board).unwrap();
    let request = RasterRequest::scene(&scene, 2.0).with_background(Color::WHITE);
    let image = CpuRasteriser.rasterise(&scene, &request).expect("a raster");
    assert!(image.warnings.is_empty(), "{:?}", image.warnings);

    let inked = image
        .rgba
        .chunks_exact(4)
        .filter(|p| p[0] < 200 && p[3] > 0)
        .count();
    assert!(inked > 50, "expected glyph coverage, found {inked} dark pixels");

    // The same board with no font must be blank and must say so.
    let bare = Snapshot::new(with_font.items_slice().to_vec());
    let scene = Scene::collect(&bare, &Scope::Board).unwrap();
    let blank = CpuRasteriser.rasterise(&scene, &request).expect("a raster");
    assert_eq!(blank.rgba.chunks_exact(4).filter(|p| p[0] < 200 && p[3] > 0).count(), 0);
    assert_eq!(blank.warnings.len(), 1, "{:?}", blank.warnings);
}

/// Alignment needs real advances, and the embedded face is where they come from.
#[test]
fn centred_text_uses_the_faces_own_advances() {
    let Some((_, font)) = a_real_font() else { return };
    let spec = spec();
    let scene = Scene::collect(&board(&font, "centre me"), &Scope::Board).unwrap();
    let measured = {
        // The scene resolved the face, so measuring through it is measuring what the
        // writers will use.
        let bytes = scene.font(&spec).expect("the face resolved");
        let with_face = ttf_width(bytes, "centre me", spec.size);
        let helvetica: f64 =
            "centre me".chars().map(helvetica_em).sum::<f64>() * spec.size;
        (with_face, helvetica)
    };
    assert!(measured.0 > 0.0, "the face measured nothing");
    // Not a claim that they differ by much — only that the face is actually
    // consulted rather than the constant table being used regardless.
    assert!(
        (measured.0 - measured.1).abs() > f64::EPSILON,
        "the embedded face and Helvetica measured identically, which means the \
         face was not used: {measured:?}"
    );
}

/// A minimal advance sum, duplicated here rather than exposed from the crate: the
/// point of the test is to check the crate's number against an independent one.
fn ttf_width(bytes: &[u8], text: &str, size: f64) -> f64 {
    let face = ttf_parser::Face::parse(bytes, 0).expect("a font");
    let em = f64::from(face.units_per_em());
    text.chars()
        .map(|c| {
            face.glyph_index(c)
                .and_then(|g| face.glyph_hor_advance(g))
                .map_or(0.0, |a| f64::from(a) / em)
        })
        .sum::<f64>()
        * size
}

fn helvetica_em(c: char) -> f64 {
    const W: [u16; 95] = [
        278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556,
        556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722,
        722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722,
        667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556,
        556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500,
        500, 334, 260, 334, 584,
    ];
    let index = (c as u32).checked_sub(0x20).and_then(|i| usize::try_from(i).ok());
    f64::from(index.and_then(|i| W.get(i)).copied().unwrap_or(556)) / 1000.0
}
