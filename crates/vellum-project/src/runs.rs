//! The document's styled runs, in the shape the text engine wants.
//!
//! This is `vellum_app::text::{convert, params_for}` ported to the browser, and it exists
//! because the tab was flattening every board's words to **one plain run**: a
//! `StyledText::plain` over the concatenation of every span's text, under a comment calling
//! it a known loss, with `docs/08-web.md` §9 repeating it. Bold, italic, links and per-run
//! colour all died there — so a link card's title read at the same weight as its blurb and a
//! Miro sticky with one red word came out entirely black.
//!
//! Nothing below is new work. `vellum_doc::text` and `vellum_text::span` are *documented*
//! twins — six fields each, same names, same meanings — because `docs/01-architecture.md` §2
//! puts the dependency arrow at `doc → text`, so the engine may not depend back on the CRDT
//! and the CRDT will not pull the engine in. That leaves exactly one place per front end
//! where the two meet, and this is the browser's.
//!
//! # Why this module is pure
//!
//! Two `use` lines, both to crates that already compile for every target, and no GPU, no
//! `wasm_bindgen`, no `web_sys`. That is deliberate and it is what makes the assertions at
//! the bottom of this file mean anything: they measure real shaping through a real
//! [`vellum_text::TextEngine`], on the host, at `cargo test` speed.
//!
//! ⚠ **They only run once `lib.rs` declares this module outside the crate-level
//! `#![cfg(target_arch = "wasm32")]`.** That attribute empties the whole crate on any other
//! target, and there is no `wasm-bindgen-test` runner in the tree — so a `#[cfg(test)]`
//! module inside the gate executes on **no** target at all. CLAUDE.md feedback 36 found
//! written-tested-and-unreachable nine times in a single night's work; a test that never
//! compiled is that fault wearing evidence's clothes, which is strictly worse than no test at
//! all. The crate-level attribute has to come off — an inner `#![cfg]` has no outside — with
//! the wasm-only body gated at the item level instead and this module declared `pub mod
//! runs;`. `pub`, because an unwired `params_for` inside a private module is a dead-code
//! warning against a zero-warning clippy bar.
//!
//! # Where this would rather live
//!
//! `vellum-project` — the crate that exists precisely for pure code both front ends share,
//! and which already owns the projection, the connector routing and the theme. Two
//! derivations of one conversion is the failure `crate::layout`'s header names when it
//! re-exports `vellum_project::look`'s constants rather than copying them. Nothing here
//! imports anything crate-local, so that move is a file move plus one line of `Cargo.toml`.

use vellum_doc::{Align, Style, StyledText as DocText};
use vellum_text::{
    BUNDLED_FAMILY, FitBox, LayoutParams, Rgb, SpanStyle, StyledText, TextAlign, TextSpan,
};

/// Converts the document's styled text into the text engine's.
///
/// A field-for-field map rather than a translation, which is the whole reason the two types
/// were built as twins. Bold, italic and a span's own colour reach cosmic-text through
/// `span_attrs`; underline, strikethrough and the link target ride the span index in the
/// buffer's metadata, which is how [`vellum_text::Layout::decoration_runs`] recovers them
/// afterwards.
///
/// # What is dropped, and why each one is not a browser gap
///
/// - **A span colour's alpha.** [`vellum_text::Rgb`] has nowhere to put it, exactly as the
///   desktop conversion has nowhere to put it: Miro's rich-text HTML carries `color:` and a
///   translucent glyph run is not something its editor can produce. Dropping the alpha keeps
///   the colour; rejecting the colour for having one would lose both.
/// - **Block structure.** [`vellum_doc::StyledText`] also carries a `BlockStyle` per line —
///   heading level, list kind, indent depth, tick state — and the engine models none of it,
///   so there is no field to write it into and nothing downstream that would draw a bullet.
///   `vellum_app::text::convert` drops it for the same reason. This is a gap in the
///   *painter*, on both front ends, not a loss introduced by the port.
///
/// Normalisation is the receiving type's: `StyledText::from_spans` drops empty spans and
/// merges adjacent runs of equal style, and `vellum_doc` does the same on its side. Keeping
/// the two invariants identical is what lets a value compare equal at every hop, and it is
/// why this function can be a `map` with no bookkeeping around it.
pub fn convert(text: &DocText) -> StyledText {
    StyledText::from_spans(text.spans().iter().map(|span| {
        TextSpan::new(
            span.text.clone(),
            SpanStyle {
                bold: span.style.bold,
                italic: span.style.italic,
                underline: span.style.underline,
                strikethrough: span.style.strikethrough,
                link: span.style.link.clone(),
                color: span.style.color.map(|c| Rgb::new(c.r, c.g, c.b)),
            },
        )
    }))
}

/// Layout parameters for an item's widget-level style.
///
/// A copy of Miro's own widget keys — `ffn`, `fs`, `ta`, `lh` — as
/// `vellum_app::text::params_for` does it, with **one** deliberate divergence: the family.
/// The size is expected to be overwritten by the caller through
/// [`LayoutParams::with_font_size`] once auto-fit has resolved, which is what both front ends
/// do; what is here is the fallback for text that named a size.
///
/// # ⚠ Trap 10, restated for a tab that has exactly one family
///
/// cosmic-text does **not** fall back to a family's regular face when the weight it was asked
/// for is missing — it leaves the family altogether, and on the machine that found this it
/// landed in a monospace one, so every bold span in the application was being set in Courier
/// at an advance ratio of 1.00 where the right face measures 4.11.
/// `TextEngine::family_has_bold` is the guard: a bold span whose family has no bold face is
/// shaped **regular in the right family** instead, losing the emphasis and keeping the
/// typeface, because there is no synthetic bold to fall back on.
///
/// That guard asks about the family that was *requested*. On the desktop the requested family
/// is very nearly always the family that will shape, because the machine's own fonts sit in
/// the database beside the bundle — a board naming `Arial` gets Arial, and Arial has a Bold.
///
/// **In a browser the database is the bundle and nothing else.** `fontdb`'s system scan
/// compiles out to a no-op on `wasm32-unknown-unknown` — `crate::text`'s header states it —
/// so [`vellum_text::BUNDLED_FONTS`], Inter Regular and Inter Bold, is the entire font set
/// for the life of the tab. Forwarding `Arial` therefore asks the guard about a family that
/// is not there: it answers false, the weight is dropped, and the shaper falls through to
/// Inter for the glyphs anyway. **The forwarded name changes nothing except that the bold is
/// lost.** Substituting the bundled family is not a downgrade to fidelity, it is the same
/// glyphs with the emphasis kept.
///
/// The substitution is exact rather than a heuristic *because the browser's font set is a
/// compile-time constant*. The same rule would be plainly wrong on the desktop, where the set
/// is whatever the machine happens to have and only the font database can answer.
///
/// # `Some(BUNDLED_FAMILY)` and never `None`
///
/// `None` means "whatever `sans-serif` resolves to", and the two engine constructors do not
/// agree about that. [`vellum_text::TextEngine::new`] calls `set_sans_serif_family` and points
/// the alias at the bundle. `TextEngine::with_fonts` — built from an explicit font list, which
/// is how the tests below model a tab — does not, and cosmic-text's own
/// `FontSystem::new_with_fonts` leaves the alias on its hardcoded default: measured in
/// cosmic-text 0.14.2, `db.set_sans_serif_family("Open Sans")`, a family that is not in the
/// bundle and is therefore not loaded. So `family_has_bold(None)` would answer about Open
/// Sans, find nothing, and drop every bold — in the tests only, while production kept working.
/// That is the shape of bug that gets a failing assertion weakened rather than a defect fixed.
/// Naming the family means the guard is asked about the family that actually shapes, under
/// either constructor.
///
/// Native arrived at the same spelling from the other end: its `DEFAULT_FONT_FAMILY` was
/// `"Noto Sans"` long after Inter was bundled, so `family_has_bold` was being asked about a
/// regular-only family and bundling a real Bold changed nothing on the board until that one
/// constant moved.
pub fn params_for(style: &Style, fit: Option<FitBox>) -> LayoutParams {
    LayoutParams {
        // ⚠ Never `style.font_family`, and never `None` — see the two notes above. One family
        // in the tab means one family here, and saying so is what keeps a bold span bold.
        font_family: Some(BUNDLED_FAMILY.to_owned()),
        font_size: style.font_size.unwrap_or(vellum_text::DEFAULT_FONT_SIZE as f64) as f32,
        line_height: style.line_height.unwrap_or(vellum_text::DEFAULT_LINE_HEIGHT as f64) as f32,
        align: match style.align {
            Some(Align::Center) => TextAlign::Center,
            Some(Align::Right) => TextAlign::Right,
            _ => TextAlign::Left,
        },
        // The height is deliberately not passed on: a wrap width is the only part of a box
        // that shaping needs, and a buffer that clips its own layout cannot be measured — so
        // auto-fit, which measures, would never converge.
        max_width: fit.map(|b| b.width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{Color, SpanStyle as DocSpanStyle, TextSpan as DocSpan};
    use vellum_text::{BUNDLED_FONTS, TextEngine};

    /// The browser's font database, byte for byte, running on the host.
    ///
    /// `with_fonts` rather than [`TextEngine::new`], and the difference is the whole point:
    /// `new` also loads every font this laptop has installed, so a measurement taken through
    /// it says nothing about a tab that has only Inter — the bold that survives might be
    /// Helvetica's. Two faces in and nothing else is exactly what `fontdb` holds on wasm32,
    /// where the system scan is a no-op.
    fn browser_engine() -> TextEngine {
        TextEngine::with_fonts(BUNDLED_FONTS.iter().map(|face| face.to_vec()))
            .expect("the bundled faces parse")
    }

    fn doc_span(text: &str, style: DocSpanStyle) -> DocSpan {
        DocSpan::new(text, style)
    }

    #[test]
    fn a_half_bold_string_keeps_both_of_its_runs() {
        let doc = DocText::from_spans([
            doc_span("torque ", DocSpanStyle::default()),
            doc_span("540 Nm", DocSpanStyle { bold: true, ..DocSpanStyle::default() }),
        ]);

        let converted = convert(&doc);
        assert_eq!(converted.to_plain(), "torque 540 Nm");
        // Two, not one: the flattening this module replaces produced exactly one run, and a
        // test that only checked the text would have passed against it.
        assert_eq!(converted.spans().len(), 2, "{:?}", converted.spans());
        assert!(!converted.spans()[0].style.bold);
        assert!(converted.spans()[1].style.bold);
    }

    /// A Miro link arrives as three independent fields on one span, so all three are checked.
    ///
    /// The document model does not derive a link's appearance from its target — `underline`
    /// and `color` are set by the HTML importer beside `link`, and a board could carry any
    /// combination. Asserting only the target would pass on a conversion that dropped the
    /// two fields that are the reason a reader can see it is a link at all.
    #[test]
    fn a_link_span_keeps_its_target_its_underline_and_its_colour() {
        let doc = DocText::from_spans([doc_span(
            "spec sheet",
            DocSpanStyle {
                link: Some("https://example.com/spec".to_string()),
                underline: true,
                color: Some(Color::rgb(0x00, 0xA3, 0x8C)),
                ..DocSpanStyle::default()
            },
        )]);

        // Bound rather than inlined: a `&` through a method call does not extend the
        // temporary the method borrowed from, so `&convert(&doc).spans()[0]` is E0716.
        let converted = convert(&doc);
        let span = &converted.spans()[0];
        assert_eq!(span.style.link.as_deref(), Some("https://example.com/spec"));
        assert!(span.style.underline);
        assert_eq!(span.style.color, Some(Rgb::new(0x00, 0xA3, 0x8C)));
    }

    /// A span colour's alpha is dropped and the colour is kept, which is the trade
    /// [`convert`]'s doc argues for. Pinned so nobody later "fixes" it into dropping the
    /// colour outright, which would be the strictly worse half.
    #[test]
    fn a_translucent_span_colour_arrives_opaque_rather_than_absent() {
        let doc = DocText::from_spans([doc_span(
            "faded",
            DocSpanStyle {
                color: Some(Color::rgba(0xE6, 0x5B, 0x58, 0x40)),
                ..DocSpanStyle::default()
            },
        )]);
        let converted = convert(&doc);
        assert_eq!(converted.spans()[0].style.color, Some(Rgb::new(0xE6, 0x5B, 0x58)));
    }

    #[test]
    fn empty_text_converts_to_empty() {
        assert!(convert(&DocText::from_spans([])).is_empty());
        assert!(convert(&DocText::plain("")).is_empty());
        assert!(convert(&DocText::default()).is_empty());
    }

    #[test]
    fn one_plain_span_stays_one_run_and_stays_plain() {
        let converted = convert(&DocText::plain("fan control module"));
        assert_eq!(converted.spans().len(), 1);
        assert_eq!(converted.to_plain(), "fan control module");
        assert!(converted.spans()[0].style.is_plain());
    }

    /// Whatever a board named, a tab is set in the one family it has.
    ///
    /// `Some(BUNDLED_FAMILY)` in every case, including the case where the board asked for it
    /// by name — so this fails on the two wrong spellings equally: forwarding
    /// `style.font_family`, and leaving it `None` to lean on the `sans-serif` alias.
    #[test]
    fn a_family_the_tab_has_not_got_is_replaced_rather_than_forwarded() {
        for asked in [None, Some("Arial"), Some("Noto Sans"), Some("Courier New"), Some("Inter")] {
            let style = Style { font_family: asked.map(str::to_owned), ..Style::default() };
            assert_eq!(
                params_for(&style, None).font_family.as_deref(),
                Some(BUNDLED_FAMILY),
                "{asked:?} reached the shaper — see the trap-10 note on params_for",
            );
        }
    }

    #[test]
    fn the_widget_level_keys_are_carried_across() {
        let style = Style {
            font_size: Some(22.0),
            line_height: Some(1.35),
            align: Some(Align::Center),
            ..Style::default()
        };
        let params = params_for(&style, Some(FitBox::new(320.0, 180.0)));
        assert_eq!(params.font_size, 22.0);
        assert_eq!(params.line_height, 1.35);
        assert_eq!(params.align, TextAlign::Center);
        // The wrap width, and only the wrap width.
        assert_eq!(params.max_width, Some(320.0));
        assert_eq!(params_for(&Style::default(), None).max_width, None);
    }

    /// **Trap 10, measured.** A bold span must come out heavier *and* in the same typeface.
    ///
    /// Both halves are here because neither can catch what the other does, and this file's
    /// whole reason for existing sits between them:
    ///
    /// - **A width assertion cannot tell "heavier" from "different".** `bold >= regular` is
    ///   satisfied comfortably by a fallback into a monospace face, which is precisely why
    ///   `vellum-text`'s original test stayed green through the entire Courier bug.
    /// - **A proportionality assertion cannot tell "heavier" from "dropped".** A request that
    ///   `family_has_bold` refuses keeps the family perfectly, so the W/I ratio is identical
    ///   and every bold span on the board silently shapes at regular weight.
    ///
    /// So the ratio of `WWWWWWWW` to `IIIIIIII` — about 4:1 in any proportional face and
    /// exactly 1.00 in a monospace one — pins the *family*, and a width delta pins the
    /// *weight*. The bar for the delta is 1%: measured on Inter at 40px the two come out
    /// 535.16 against 550.51, a 2.87% gap, and it is **0.0%** the moment the request is
    /// dropped, so 1% is impossible without a real weight change and loose enough to survive
    /// a font update.
    ///
    /// The style asks for a family no tab has, on purpose, and the params come from
    /// [`params_for`] rather than being built by hand — otherwise this re-tests the engine
    /// that already has its own tests, and says nothing about the substitution that is the
    /// only thing this module decides. A/B: put `style.font_family` back into `params_for`
    /// and the delta is 0.0%.
    ///
    /// ⚠ **The second half of this A/B is no longer reproducible, and it is recorded rather
    /// than deleted because the change that broke it is the interesting part.** It used to
    /// read that writing `None` also gives 0.0%, *because* `with_fonts` leaves the
    /// `sans-serif` alias on cosmic-text's own default. That was true and is the bug the
    /// browser later paid for: `with_fonts` is how a tab builds its engine, so every bold
    /// span in a browser silently shaped at regular weight. `with_fonts` now points the alias
    /// at the bundle when the bundle is among what was loaded — so `None` resolves to Inter
    /// here and the measurement it described cannot be taken any more.
    #[test]
    fn a_bold_span_is_heavier_and_has_not_left_the_family() {
        let mut engine = browser_engine();
        let style = Style {
            font_family: Some("Arial".to_string()),
            font_size: Some(40.0),
            ..Style::default()
        };
        let params = params_for(&style, None);

        let run = |text: &str, bold: bool| {
            convert(&DocText::from_spans([doc_span(
                text,
                DocSpanStyle { bold, ..DocSpanStyle::default() },
            )]))
        };

        let phrase = "Finally Driving The Acme M5";
        let regular = engine.measure(&run(phrase, false), &params).width;
        let heavy = engine.measure(&run(phrase, true), &params).width;
        assert!(
            f64::from(heavy) > f64::from(regular) * 1.01,
            "asking for bold changed nothing: {regular:.2} against {heavy:.2} — the family \
             reaching the shaper is one this database has not got, so `family_has_bold` \
             refused the weight",
        );

        let mut ratio = |bold: bool| {
            let narrow = engine.measure(&run("IIIIIIII", bold), &params).width;
            let wide = engine.measure(&run("WWWWWWWW", bold), &params).width;
            assert!(narrow > 0.0, "bold={bold} measured nothing at all");
            wide / narrow
        };
        let (plain, bold) = (ratio(false), ratio(true));
        assert!(bold > 2.0, "bold shaped in a monospace face: W/I came out {bold:.2}");
        assert!(
            (bold - plain).abs() < 0.5,
            "the proportions moved from {plain:.2} to {bold:.2} — that is a different \
             typeface, not a heavier one",
        );
    }

    /// The converted runs are what the shaper actually reads, not merely what the model
    /// holds.
    ///
    /// One string, set twice: once as a single plain span and once split so that half of it
    /// is bold. Same characters, same params, same engine — so any width difference at all
    /// can only have come from a span style surviving [`convert`] and reaching cosmic-text.
    /// This is the assertion that fails against the flattening this module replaces, and it
    /// fails there by a margin of exactly zero.
    #[test]
    fn a_half_bold_string_measures_wider_than_the_same_string_flattened() {
        let mut engine = browser_engine();
        let params = params_for(
            &Style { font_size: Some(40.0), ..Style::default() },
            None,
        );

        let flat = convert(&DocText::plain("Finally Driving The Acme M5"));
        let split = convert(&DocText::from_spans([
            doc_span("Finally Driving ", DocSpanStyle::default()),
            doc_span(
                "The Acme M5",
                DocSpanStyle { bold: true, ..DocSpanStyle::default() },
            ),
        ]));
        assert_eq!(split.to_plain(), flat.to_plain(), "the two must differ only in style");

        let (flat_width, split_width) =
            (engine.measure(&flat, &params).width, engine.measure(&split, &params).width);
        assert!(
            split_width > flat_width,
            "the bold half did not reach shaping: {flat_width:.2} flat against \
             {split_width:.2} split",
        );
    }
}
