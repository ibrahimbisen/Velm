//! The reference board, end to end: Miro clipboard → document → spatial index.
//!
//! `captures/reference-board.html` is a real copy of a real Miro board — 596
//! widgets, 41283 × 17515 world pixels, every kind the importer knows. It is the only
//! input in the project that can fail in ways a synthetic board cannot: overlapping
//! frames, connectors bound to items further down the array, ink with 128-point
//! strokes, images whose bytes are in an archive that is not here.
//!
//! Everything below runs without a GPU, a window or a hand on a trackpad, which is
//! the whole reason `vellum-app` is a library as well as a binary.
//!
//! The capture is git-ignored, so these skip cleanly when it is absent rather than
//! failing on a fresh clone.

use std::path::{Path, PathBuf};
use std::time::Instant;

use vellum_app::editor::Editor;
use vellum_app::project::Projection;
use vellum_doc::Board;
use vellum_scene::{Camera, ScreenPoint, ScreenSize, WorldPoint};
use vellum_store::BlobStore;

/// Widgets in the capture. Fixed by the file, not by this build: if the importer
/// starts dropping one, this is where it shows up.
const EXPECTED_ITEMS: usize = 596;

/// The **canvas** of Miro's own SVG export of this board, in world pixels — its
/// `width`/`height` attributes, not its content's bounding box.
///
/// Measured, the capture's content occupies 38234 × 11502 of it, centred exactly on
/// the origin. The width agrees with the export to 8%, which is the real signal that
/// the placement arithmetic is right; the height does not, because Miro's exported
/// canvas carries vertical margin the widgets do not. So the assertion below is
/// containment — the content must fit inside the canvas Miro drew it on — rather
/// than equality, which would be asserting something that was never true.
const SVG_CANVAS: (f64, f64) = (41_282.89, 17_515.36);

/// The first `.html` capture in `captures/`, whatever it is called.
///
/// Found by extension rather than by name: captures are git-ignored and named after
/// whichever board they came from, so a hardcoded title would bake someone's board
/// name into the source and only ever work on one machine.
fn capture_path() -> Option<PathBuf> {
    // `CARGO_MANIFEST_DIR` is the crate, and captures live at the workspace root.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join("captures");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "html"))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// Imports the capture, or `None` when it is not checked out.
fn imported() -> Option<(tempfile::TempDir, Editor, usize)> {
    let Some(path) = capture_path() else {
        eprintln!("skipping: no .html capture in captures/");
        return None;
    };
    let html = std::fs::read_to_string(&path).expect("the capture is readable");

    let home = tempfile::tempdir().expect("a temp directory");
    let blobs = BlobStore::open(home.path().join("blobs")).expect("a blob store");
    let mut editor = Editor::in_memory(Board::new(), blobs);

    let outcome = editor
        .import_html(&html, None)
        .expect("the capture imports")
        .expect("the capture is a Miro payload");
    let total = outcome.total();
    eprintln!("{outcome}");
    Some((home, editor, total))
}

/// The headline: every widget in the capture becomes an item, and every item is in
/// the spatial index with a box the renderer can use.
#[test]
fn the_reference_board_imports_and_projects_every_item() {
    let Some((_home, editor, total)) = imported() else { return };

    assert_eq!(total, EXPECTED_ITEMS, "the importer produced {total} items");
    assert_eq!(editor.board().item_count(), EXPECTED_ITEMS);

    let projection = editor.projection();
    assert_eq!(projection.len(), EXPECTED_ITEMS, "the projection lost items");
    assert_eq!(
        projection.scene().len(),
        EXPECTED_ITEMS,
        "the R-tree and the item table disagree"
    );

    // Every document item resolves to a scene item and back.
    for doc_id in editor.board().item_ids() {
        let scene_id = projection
            .scene_id(doc_id)
            .unwrap_or_else(|| panic!("{doc_id} never reached the scene"));
        let projected = projection.get(scene_id).expect("a projected item");
        assert_eq!(projected.doc_id, doc_id);
    }
}

/// Sane bounds, item by item. A NaN or an inverted box does not crash anything — it
/// silently drops the item out of every viewport query, which reads as "the importer
/// lost my drawing".
#[test]
fn every_item_has_bounds_a_renderer_can_use() {
    let Some((_home, editor, _)) = imported() else { return };
    let projection = editor.projection();

    let mut sized = 0;
    let mut degenerate = Vec::new();
    for (id, item) in projection.iter() {
        let bounds = item.bounds;
        let label = item.item.kind.tag();

        assert!(
            bounds.min.x.is_finite()
                && bounds.min.y.is_finite()
                && bounds.max.x.is_finite()
                && bounds.max.y.is_finite(),
            "item {id} ({label}) has a non-finite box: {bounds:?}"
        );
        assert!(
            bounds.min.x <= bounds.max.x && bounds.min.y <= bounds.max.y,
            "item {id} ({label}) has an inverted box: {bounds:?}"
        );
        // Nothing on a 41k-pixel board belongs a million pixels away. A placement
        // read from the wrong field, or a stroke that was not recentred, lands here.
        assert!(
            bounds.min.x.abs() < 1e6 && bounds.min.y.abs() < 1e6,
            "item {id} ({label}) is off in the weeds: {bounds:?}"
        );

        if bounds.width() > 0.0 && bounds.height() > 0.0 {
            sized += 1;
        } else {
            // A container with no members, or a Miro widget that carried no size at
            // all, legitimately collapses to a point. It draws nothing and it is
            // still hit-testable at its position, which is the honest outcome; what
            // would not be honest is a *board* full of them.
            degenerate.push((*id, label, bounds));
        }
    }

    eprintln!("{sized} of {EXPECTED_ITEMS} items have a rectangle; degenerate: {degenerate:?}");
    assert!(
        degenerate.len() * 100 < EXPECTED_ITEMS,
        "{} of {EXPECTED_ITEMS} items collapsed to a point: {degenerate:?}",
        degenerate.len()
    );
}

/// The board's own extent, against Miro's SVG export of the same board.
///
/// The two come from completely different Miro code paths — an obfuscated clipboard
/// payload and a rendered vector export — so this is a real check on the placement
/// arithmetic rather than a restatement of it. See [`SVG_CANVAS`] for why the check
/// is containment plus a width match rather than equality on both axes.
#[test]
fn the_projected_extent_fits_the_canvas_miro_exported() {
    let Some((_home, editor, _)) = imported() else { return };
    let content = editor
        .projection()
        .content_bounds()
        .expect("a 596-item board has an extent");
    eprintln!(
        "content {:.0} x {:.0}, centred on ({:.0}, {:.0}); Miro's canvas {:.0} x {:.0}",
        content.width(),
        content.height(),
        content.center().x,
        content.center().y,
        SVG_CANVAS.0,
        SVG_CANVAS.1
    );

    assert!(content.width() > 0.0 && content.height() > 0.0);
    assert!(
        content.width() <= SVG_CANVAS.0 && content.height() <= SVG_CANVAS.1,
        "the board is larger than the canvas Miro exported it on: {:.0} x {:.0}",
        content.width(),
        content.height()
    );
    // Within 15% on the axis that has no export margin. A placement read from the
    // wrong field, or a scale applied twice, misses this by a factor, not by a
    // percent.
    assert!(
        SVG_CANVAS.0 / content.width() < 1.15,
        "width {:.0} against Miro's canvas {:.0}",
        content.width(),
        SVG_CANVAS.0
    );
    // Miro's clipboard coordinates are centred on the board's own centre, so the
    // content's midpoint is the origin. Drifting off it means a widget was placed
    // from the wrong reference point.
    assert!(
        content.center().x.abs() < 1.0 && content.center().y.abs() < 1.0,
        "the board is not centred on the origin: {:?}",
        content.center()
    );
}

/// The claim the whole architecture rests on: frame cost follows the viewport, and
/// the index agrees with a brute-force scan. A fast query that quietly drops items is
/// the one bug a performance test would never catch.
#[test]
fn culling_is_correct_and_cheap_on_the_real_board() {
    let Some((_home, editor, _)) = imported() else { return };
    let projection = editor.projection();
    let content = projection.content_bounds().unwrap();

    let mut fitted = Camera::new(ScreenSize::new(1440.0, 900.0));
    fitted.fit_to_rect(content, 0.02);
    assert_eq!(
        projection.scene().query_viewport(&fitted).count(),
        projection.len(),
        "fitting the board did not show all of it"
    );

    // A screenful at 100% somewhere in the middle of the board.
    let mut close = Camera::new(ScreenSize::new(1440.0, 900.0));
    close.set_center(content.center());
    close.set_zoom_about(1.0, ScreenPoint::new(720.0, 450.0));
    close.set_center(content.center());

    let visible = projection.scene().query_viewport(&close).count();
    assert!(
        visible < projection.len(),
        "a 1440x900 window at 100% showed all {} items of a {:.0}px board",
        projection.len(),
        content.width()
    );

    // And the index agrees with the honest answer, at several viewports.
    for (fraction_x, fraction_y, zoom) in [
        (0.1, 0.1, 1.0),
        (0.5, 0.5, 0.25),
        (0.9, 0.8, 4.0),
        (0.5, 0.5, 0.02),
    ] {
        let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
        let centre = WorldPoint::new(
            content.min.x + content.width() * fraction_x,
            content.min.y + content.height() * fraction_y,
        );
        camera.set_center(centre);
        camera.set_zoom_about(zoom, ScreenPoint::new(720.0, 450.0));
        camera.set_center(centre);

        let rect = camera.visible_world_rect();
        let mut expected: Vec<u64> = projection
            .iter()
            .filter(|(_, item)| item.bounds.intersects(&rect))
            .map(|(id, _)| *id)
            .collect();
        let mut got: Vec<u64> = projection
            .scene()
            .query_viewport(&camera)
            .map(|item| item.id)
            .collect();
        expected.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expected, "index disagreed with brute force at {zoom}x");
    }
}

/// Paint order has to be the document's depth-first order: a frame before its
/// contents, siblings back to front. Getting it wrong hides every item inside a
/// frame behind the frame's own background.
#[test]
fn frames_paint_before_what_they_contain() {
    let Some((_home, editor, _)) = imported() else { return };
    let projection = editor.projection();

    let mut checked = 0;
    for (id, item) in projection.iter() {
        let Some(parent) = item.parent else { continue };
        let parent_item = projection.get(parent).expect("a parent that exists");
        assert!(
            parent_item.z < item.z,
            "item {id} ({}) drew before its parent ({})",
            item.item.kind.tag(),
            parent_item.item.kind.tag()
        );
        checked += 1;
    }
    assert!(checked > 0, "the reference board has no nesting at all");
}

/// Reprojecting a 596-item board is what a paste, an undo and a delete each cost.
/// It is not on the frame path, but it *is* on the interaction path.
#[test]
fn reprojection_is_fast_enough_to_sit_behind_an_edit() {
    let Some((_home, editor, _)) = imported() else { return };

    let mut projection = Projection::new();
    let started = Instant::now();
    const ROUNDS: u32 = 5;
    for _ in 0..ROUNDS {
        projection.rebuild(editor.board()).expect("reprojection");
    }
    let each = started.elapsed() / ROUNDS;

    eprintln!("reprojected {} items in {:.1} ms", projection.len(), each.as_secs_f64() * 1000.0);
    assert_eq!(projection.len(), EXPECTED_ITEMS);
    // Generous: this is a debug build, and the point is to catch an accidental
    // quadratic, not to police a millisecond.
    assert!(
        each.as_secs_f64() < 2.0,
        "reprojection took {:.1} ms",
        each.as_secs_f64() * 1000.0
    );
}

/// Hit-testing has to agree with what the user can see, on real overlapping content.
#[test]
fn hit_testing_agrees_with_paint_order() {
    let Some((_home, editor, _)) = imported() else { return };
    let projection = editor.projection();
    let content = projection.content_bounds().unwrap();

    let mut probed = 0;
    for i in 0..64 {
        // A deterministic scatter across the board.
        let t = i as f64 / 64.0;
        let point = WorldPoint::new(
            content.min.x + content.width() * ((t * 7.0) % 1.0),
            content.min.y + content.height() * ((t * 13.0) % 1.0),
        );
        let Some(hit) = projection.scene().hit_test(point) else { continue };
        probed += 1;

        let topmost = projection
            .iter()
            .filter(|(_, item)| item.bounds.contains(point))
            .max_by_key(|(id, item)| (item.z, **id))
            .map(|(id, _)| *id);
        assert_eq!(Some(hit), topmost, "hit-test picked the wrong item at {point:?}");
    }
    assert!(probed > 8, "only {probed} of 64 probes landed on anything");
}

/// Every item kind the capture contains ends up somewhere the renderer can dispatch
/// on. This is the list that tells the honest story about import fidelity.
#[test]
fn the_capture_is_reported_kind_by_kind() {
    let Some((_home, editor, _)) = imported() else { return };

    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (_, item) in editor.projection().iter() {
        *counts.entry(item.item.kind.tag()).or_default() += 1;
    }
    eprintln!("projected kinds: {counts:?}");

    assert_eq!(
        counts.values().sum::<usize>(),
        EXPECTED_ITEMS,
        "the kind census does not add up"
    );
    // Ink is the largest thing on this board and the one no Miro API exposes; if it
    // ever stops arriving, the import has lost its whole reason to exist.
    assert!(
        counts.get("ink").copied().unwrap_or(0) > 100,
        "the board's freehand ink did not survive: {counts:?}"
    );
}
