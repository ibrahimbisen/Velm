//! Taking a board out of the tab — SVG, and a spreadsheet.
//!
//! The adapter is [`vellum_project::export`], shared with the desktop, so a board exports the
//! same way from both: connectors routed from live bounds, text shaped for the lines it is
//! actually drawn as, an auto-fitted sticky at the size it resolved to. What differs is the
//! one seam the document cannot answer — where an image's bytes come from — and here the
//! answer is *nowhere*, which is stated rather than worked around.
//!
//! # What comes out, and what is missing from it
//!
//! - **SVG** — every item's geometry, style and words. A picture is a placeholder rectangle.
//! - **CSV** — whole. It carries no pictures on any platform, so nothing is lost.
//!
//! **PDF and PNG are deliberately absent**, and they are absent for different reasons worth
//! keeping apart. A PDF is one page per frame and refuses a frameless board outright, which
//! is a refusal a person meets after choosing a menu row — it belongs behind a control that
//! can explain itself, not behind a download that silently produces nothing. A PNG needs the
//! rasteriser to run, and `raster_request` caps by area precisely because the desktop once
//! asked for 1.34 GB in one allocation; a tab's linear memory never returns to the operating
//! system, so the peak of one export would be the tab's footprint for the rest of its life.
//!
//! # ⚠ Images are missing, and the honest thing is to say so in the file
//!
//! `ImageLayer` holds GPU textures, not bytes: the decode happens in the browser and the
//! pixels go straight to the driver, so there is nothing here to hand an exporter. Fetching
//! every picture again to export one file is a real design and it is not this one.
//!
//! So a picture exports as a placeholder — and the *count* comes back with the bytes, so the
//! page can say `12 pictures are placeholders` beside the download rather than letting
//! somebody discover it when they open the file. Never describing a gesture the user cannot
//! perform is this application's rule; quietly handing them a lesser file than they asked for
//! is the same failure with the disappointment postponed.

use wasm_bindgen::prelude::*;

use vellum_project::export::BoardExport;

/// One board, in one format, or an empty string.
///
/// `format` is `"svg"` or `"csv"`. The answer is the file's own text — both are text formats,
/// which is why this can hand bytes back across the wasm boundary as a `String` and let the
/// page make its own `Blob`. A binary format would need a `Vec<u8>` and a different door.
///
/// ⚠ **Empty means "nothing came out", and the page must not offer a download for it.** Every
/// failure lands here: no viewer yet, a board with nothing on it, an emitter that refused. The
/// alternative — a zero-byte file with the right name — is the shape this application refuses
/// everywhere else, because it looks exactly like success until it is opened.
#[wasm_bindgen]
pub fn export_board(format: &str) -> String {
    let Some(held) = crate::edit::viewer() else { return String::new() };
    let Ok(mut viewer) = held.try_borrow_mut() else { return String::new() };
    let crate::Viewer { projection, text, .. } = &mut *viewer;

    let mut source = BoardExport::plain(projection, None);
    // ⚠ `shape_text_with` and never `shape_text`. The desktop resolves the family a board
    // asked for against the machine's fonts; a tab's font database **is** the bundle, so it
    // must force the bundled family — `runs::params_for`'s whole reason for existing. Shaping
    // through the desktop's policy here would ask fontdb for Arial, get nothing, and fall out
    // of the family entirely, which is trap 10 exported to a file.
    source.shape_text_with(text.engine_mut(), vellum_project::runs::params_for);

    let scene = match vellum_export::Scene::collect(&source, &vellum_export::Scope::Board) {
        Ok(scene) => scene,
        // `collect`'s one failure is "nothing to export", which is a statement about the
        // board rather than a fault, so it is logged at info and not as an error.
        Err(error) => {
            log::info!("velm export: nothing to export: {error}");
            return String::new();
        }
    };

    let written = match format {
        "svg" => vellum_export::svg::write(&scene, &vellum_export::svg::SvgOptions::default()),
        // Miro's own column set, which is what somebody exporting a spreadsheet from a board
        // is expecting to open — and the desktop's row uses it too.
        "csv" => vellum_export::csv::write(&scene, &vellum_export::csv::CsvOptions::miro()),
        other => {
            log::error!("velm export: no such format {other}");
            return String::new();
        }
    };
    match written {
        Ok(text) => text,
        Err(error) => {
            log::error!("velm export: {error}");
            String::new()
        }
    }
}

/// How many pictures this board would export as placeholders.
///
/// Its own verb rather than a field on the answer above, so the page can say it **before** the
/// download rather than after — the export itself is the expensive half, and a warning that
/// arrives with the file is a warning about a decision already made.
#[wasm_bindgen]
pub fn export_placeholders() -> u32 {
    let Some(held) = crate::edit::viewer() else { return 0 };
    let Ok(viewer) = held.try_borrow() else { return 0 };
    let images = viewer
        .projection
        .iter()
        .filter(|(_, projected)| matches!(projected.item.kind, vellum_doc::ItemKind::Image { .. }))
        .count();
    u32::try_from(images).unwrap_or(u32::MAX)
}
