//! The design language, enforced rather than remembered.
//!
//! `docs/05-design-language.md` states its rules as prohibitions — no purple, no
//! gradients, no 16px radii, no shadow on everything, no emoji icons — because those
//! are precisely the defaults a generated interface drifts toward. A prohibition
//! written in prose survives about as long as the next person who has not read it.
//!
//! So the ones that can be checked mechanically are checked here:
//!
//! - **No widget names a colour.** Every value resolves through a token in
//!   `theme.rs`, which is the only module allowed to hold a hex literal. This is the
//!   load-bearing one: the light and dark cuts are two *specifications*, not one
//!   filtered value, so a hex written into a widget is a colour that is right in one
//!   mode and silently wrong in the other.
//! - **No widget names a radius.** Corners come from `theme::radius`, which cannot
//!   exceed 6.
//! - **The palette contains no violet**, at any saturation worth the name.
//! - **The mark's geometry matches the asset** it implements.
//!
//! The scan reads `src/` at run time rather than through `include_str!`, so a module
//! added tomorrow is covered without anyone remembering to list it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use vellum_ui::theme::{Palette, radius, space, text};

/// The one module allowed to name a colour, a radius or a spacing value.
const TOKEN_MODULE: &str = "theme.rs";

fn sources() -> BTreeMap<String, String> {
    let src: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(&src).expect("the crate has a src directory") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            let name = path
                .file_name()
                .expect("a file has a name")
                .to_string_lossy()
                .into_owned();
            files.insert(name, std::fs::read_to_string(&path).expect("readable source"));
        }
    }
    assert!(files.len() > 10, "the scan found only {} files; it is looking in the wrong place", files.len());
    assert!(files.contains_key(TOKEN_MODULE), "the token module moved");
    files
}

/// Lines of `source` containing `needle`, with their 1-based numbers.
fn hits<'a>(source: &'a str, needle: &str) -> Vec<(usize, &'a str)> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .map(|(n, line)| (n + 1, line.trim()))
        .collect()
}

/// Every colour literal `egui` offers, plus the constructors that build one.
///
/// `Color32::from_rgba_unmultiplied` and `from_rgb` with *computed* arguments are
/// legitimate — `color.rs` converts document colours and builds a hue ramp — so the
/// patterns below catch the named constants and the hex-literal form only.
const COLOUR_LITERALS: [&str; 6] = [
    "Color32::from_black_alpha",
    "Color32::from_white_alpha",
    "Color32::from_gray",
    "Color32::from_rgb(0x",
    "Color32::from_rgba_premultiplied(0x",
    "Color32::from_rgba_unmultiplied(0x",
];

/// The named `Color32` constants. Spelled out rather than matched by pattern so the
/// failure message can name the colour that was reached for.
const NAMED_COLOURS: [&str; 14] = [
    "Color32::WHITE",
    "Color32::BLACK",
    "Color32::RED",
    "Color32::GREEN",
    "Color32::BLUE",
    "Color32::YELLOW",
    "Color32::GRAY",
    "Color32::DARK_GRAY",
    "Color32::LIGHT_GRAY",
    "Color32::DARK_RED",
    "Color32::LIGHT_BLUE",
    "Color32::BROWN",
    "Color32::ORANGE",
    "Color32::PURPLE",
];

/// The rule that makes the two palettes safe to edit independently.
#[test]
fn no_widget_names_a_colour() {
    let mut offences: Vec<String> = Vec::new();
    for (file, source) in sources() {
        if file == TOKEN_MODULE {
            continue;
        }
        for needle in COLOUR_LITERALS.iter().chain(NAMED_COLOURS.iter()) {
            for (line, text) in hits(&source, needle) {
                offences.push(format!("{file}:{line}  {text}"));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "a colour was named outside `{TOKEN_MODULE}`. Every colour must resolve \
         through a token, because the light and dark cuts are two specifications \
         rather than one filtered value:\n  {}",
        offences.join("\n  ")
    );
}

/// `Color32::TRANSPARENT` gets its own rule and its own reason.
///
/// It is not *wrong* in the way a hex is — it is the absence of colour, which is the
/// same in both modes. But it is almost always the wrong tool: what a widget usually
/// wants is the colour it is already holding at zero alpha, so that a gradient fades
/// to *that* colour rather than to nothing.
#[test]
fn nothing_fades_to_a_named_transparent() {
    for (file, source) in sources() {
        if file == TOKEN_MODULE {
            continue;
        }
        let found = hits(&source, "Color32::TRANSPARENT");
        assert!(
            found.is_empty(),
            "{file} fades to a named transparent rather than to its own colour at \
             zero alpha: {found:?}"
        );
    }
}

/// Corners come from the scale, and the scale cannot leave the 4–6px band.
///
/// `docs/05` §2 calls pillowy radii out by name, and a `CornerRadius::same(16)`
/// written into one widget is the single most recognisable tell there is.
#[test]
fn no_widget_names_a_radius() {
    let mut offences: Vec<String> = Vec::new();
    for (file, source) in sources() {
        if file == TOKEN_MODULE {
            continue;
        }
        for (line, text) in hits(&source, "CornerRadius::same(") {
            let literal = text
                .split("CornerRadius::same(")
                .skip(1)
                .any(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
            if literal {
                offences.push(format!("{file}:{line}  {text}"));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "a radius was written out rather than taken from `theme::radius`:\n  {}",
        offences.join("\n  ")
    );

    for r in [radius::TIGHT, radius::SMALL, radius::MEDIUM, radius::LARGE] {
        assert!(r <= 6, "{r}px is a pillowy radius");
    }
}

/// Hue, in degrees, and how saturated the colour is — enough to tell a violet from a
/// neutral that happens to lean cool.
fn hue_and_saturation(color: egui::Color32) -> (f32, f32) {
    let (r, g, b) = (
        f32::from(color.r()) / 255.0,
        f32::from(color.g()) / 255.0,
        f32::from(color.b()) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    if chroma < f32::EPSILON || max < f32::EPSILON {
        return (0.0, 0.0);
    }
    let hue = if max == r {
        ((g - b) / chroma).rem_euclid(6.0)
    } else if max == g {
        (b - r) / chroma + 2.0
    } else {
        (r - g) / chroma + 4.0
    } * 60.0;
    (hue, chroma / max)
}

/// The first prohibition in `docs/05` §2, and the one every generated interface
/// breaks: violet-to-blue is the house style of software that was not designed.
#[test]
fn the_interface_contains_no_purple() {
    for (name, palette) in [("light", Palette::LIGHT), ("dark", Palette::DARK)] {
        let tokens: [(&str, egui::Color32); 16] = [
            ("backdrop", palette.backdrop),
            ("surface", palette.surface),
            ("raised", palette.raised),
            ("well", palette.well),
            ("canvas", palette.canvas),
            ("hover", palette.hover),
            ("pressed", palette.pressed),
            ("border", palette.border),
            ("text", palette.text),
            ("muted", palette.muted),
            ("faint", palette.faint),
            ("accent", palette.accent),
            ("accent_soft", palette.accent_soft),
            ("info", palette.info),
            ("info_soft", palette.info_soft),
            ("success", palette.success),
        ];
        for (token, color) in tokens {
            let (hue, saturation) = hue_and_saturation(color);
            assert!(
                !((250.0..=330.0).contains(&hue) && saturation > 0.12),
                "{name}.{token} is violet: hue {hue:.0}°, saturation {saturation:.2}"
            );
        }
    }
}

/// Every distinct colour that reaches the vertex buffer for one full frame of chrome.
///
/// The token tests above check what the palette *says*; this checks what is actually
/// rasterised, including anything a widget draws by hand and anything egui contributes
/// from its own defaults.
fn rendered_colours(theme: vellum_ui::Theme) -> Vec<egui::Color32> {
    use std::time::SystemTime;
    use vellum_ui::{BoardState, Chrome, ChromeState, Screen};

    let ctx = egui::Context::default();
    let mut chrome = Chrome::new();
    chrome.set_theme(theme);

    let selection = [vellum_ui::SelectionItem::new(
        "1@1".parse().expect("well-formed item id"),
        vellum_ui::ItemFacet::Sticky,
        vellum_ui::Placement::new(1234.0, 5678.0, 200.0, 200.0),
    )];
    let boards = [vellum_ui::BoardCard {
        path: "/boards/site-plan.vellum".into(),
        title: "Site plan".to_owned(),
        item_count: 596,
        modified: SystemTime::UNIX_EPOCH,
        starred: true,
        deleted: None,
        thumbnail: None,
    }];

    let mut colours = Vec::new();
    for screen in [Screen::Library, Screen::Board] {
        let state = ChromeState {
            screen,
            board: BoardState { title: "Site plan", dirty: true, ..BoardState::default() },
            selection: &selection,
            library: &boards,
            now: SystemTime::UNIX_EPOCH,
            ..ChromeState::default()
        };
        // Two passes: the first sizes the panels, the second fills them.
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 900.0),
                )),
                ..egui::RawInput::default()
            };
            let output = ctx.run_ui(raw, |ui| {
                let _ = chrome.show(ui, &state);
            });
            for clipped in ctx.tessellate(output.shapes, 1.0) {
                if let egui::epaint::Primitive::Mesh(mesh) = clipped.primitive {
                    colours.extend(mesh.vertices.iter().map(|v| v.color));
                }
            }
        }
    }
    assert!(colours.len() > 500, "only {} vertices; the chrome did not draw", colours.len());
    colours.sort_by_key(egui::Color32::to_array);
    colours.dedup();
    colours
}

/// The end-to-end form of the first prohibition. Not "the palette has no purple in
/// it" but "no purple pixel is emitted", which also covers whatever egui's own
/// defaults would have contributed had a `Visuals` field been left unset.
#[test]
fn no_violet_reaches_the_vertex_buffer() {
    for theme in [vellum_ui::Theme::Light, vellum_ui::Theme::Dark] {
        for color in rendered_colours(theme) {
            if color.a() == 0 {
                continue;
            }
            let (hue, saturation) = hue_and_saturation(color);
            assert!(
                !((250.0..=330.0).contains(&hue) && saturation > 0.12),
                "{theme:?} rasterised {color:?}: hue {hue:.0}°, saturation {saturation:.2}"
            );
        }
    }
}

/// The neutrals being only four percent apart is the point — hierarchy is carried by
/// type and hairlines rather than by slabs of contrasting grey. If someone "fixes"
/// the contrast by pulling the surfaces apart, this is what notices.
#[test]
fn the_neutrals_stay_close_together() {
    let value = |c: egui::Color32| f32::from(c.r()) + f32::from(c.g()) + f32::from(c.b());
    for (name, palette) in [("light", Palette::LIGHT), ("dark", Palette::DARK)] {
        let step = (value(palette.surface) - value(palette.canvas)).abs() / 3.0;
        assert!(step > 2.0, "{name}: the canvas and the panels are indistinguishable");
        assert!(
            step < 32.0,
            "{name}: the surfaces are {step:.0} apart — this design separates them \
             with a hairline, not with a slab of grey"
        );
    }
}

/// The board's default ink and fill are the interface's own, not a fourth family.
#[test]
fn the_document_defaults_agree_with_the_palette() {
    assert_eq!(
        vellum_ui::to_egui(vellum_ui::color::INK),
        Palette::LIGHT.text,
        "the pen's default ink has drifted from the interface's ink"
    );
    assert_eq!(
        vellum_ui::color::DEFAULT_FILL,
        vellum_ui::SWATCHES[0][0],
        "the board default is no longer the first swatch a user reaches for"
    );
    assert_eq!(
        vellum_ui::PenPreset::default().color,
        vellum_ui::color::INK,
        "the pen picked up a colour of its own"
    );
}

/// The type scale is four sizes and the grid is 4px. Both are stated in `docs/05` §3
/// and §5, and both are the kind of thing that erodes one widget at a time.
#[test]
fn the_scales_are_the_ones_the_design_language_states() {
    assert_eq!(space::UNIT, 4.0);
    assert_eq!(
        [text::LABEL, text::BODY, text::PANEL_TITLE, text::SCREEN_TITLE],
        [11.0, 13.0, 15.0, 20.0]
    );
    assert_eq!(text::LINE_HEIGHT, 1.4);
}

/// Emoji as iconography, ruled out by `docs/05` §2.
///
/// The icon set is geometry — `Icon` and `mark` draw every glyph in the chrome from
/// polylines — so the only way an emoji reaches the interface is through a string
/// literal. This catches the ones that have historically crept in as ticks, arrows
/// and status dots.
#[test]
fn no_emoji_is_used_as_an_icon() {
    // Dingbats and symbols that render from the emoji fallback font on both
    // platforms Vellum ships on. The bullet `·` and `•` are punctuation and are
    // allowed; these are pictures.
    const EMOJI: [&str; 12] =
        ["✔", "✅", "❌", "⚠", "🔍", "📁", "📌", "⭐", "🎨", "➡", "⬅", "🖊"];
    let mut offences: Vec<String> = Vec::new();
    for (file, source) in sources() {
        for glyph in EMOJI {
            for (line, text) in hits(&source, glyph) {
                offences.push(format!("{file}:{line}  {text}"));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "an emoji is standing in for an icon; the set is drawn geometry:\n  {}",
        offences.join("\n  ")
    );
}

/// The mark is drawn from geometry and specified by an asset. They have to agree, and
/// the accent has to be the one its mode actually uses.
#[test]
fn the_mark_agrees_with_its_asset_in_both_cuts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels under the repository root");
    let read = |name: &str| {
        std::fs::read_to_string(root.join("assets/logo").join(name))
            .unwrap_or_else(|e| panic!("assets/logo/{name}: {e}"))
    };
    // Matched as a stroke rather than as a bare hex: both files name the *other*
    // mode's red in a comment explaining why they are two files.
    let stroke = |c: egui::Color32| format!("stroke=\"#{:02X}{:02X}{:02X}\"", c.r(), c.g(), c.b());

    let light = read("mark.svg");
    assert!(light.contains(&stroke(Palette::LIGHT.accent)), "light accent disagrees");
    assert!(light.contains(&stroke(Palette::LIGHT.text)), "light ink disagrees");

    let dark = read("mark-dark.svg");
    assert!(dark.contains(&stroke(Palette::DARK.accent)), "dark accent disagrees");
    assert!(dark.contains(&stroke(Palette::DARK.text)), "dark ink disagrees");

    // One accented corner, so the mark has a reading direction rather than anonymous
    // four-fold symmetry.
    assert_eq!(light.matches(&stroke(Palette::LIGHT.accent)).count(), 1);
    assert_eq!(dark.matches(&stroke(Palette::DARK.accent)).count(), 1);

    // Clear space is one bracket arm: 12 units on the 48-unit grid.
    assert_eq!(vellum_ui::mark::CLEAR_SPACE, 12.0 / 48.0);
}
