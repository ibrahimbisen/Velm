//! The import measured against a real board.
//!
//! Ground truth is one real Miro board, exported four ways on 2026-07-28 and
//! recorded in `docs/02-miro-formats.md`: a 1,096,928-byte full-board clipboard
//! copy of 596 objects, the `.rtb` backup holding 205 assets at original
//! resolution, and the SVG export that acts as an independent oracle.
//!
//! Unit tests prove each rule in isolation; this proves they compose on a board
//! nobody designed to be easy. Every count below was measured, not chosen.
//!
//! The exports are ~300MB of someone's real work and are git-ignored, so the test
//! **skips cleanly when they are absent** rather than failing. That is a deliberate
//! trade: a CI checkout has no way to run this, and a red test that is expected to
//! be red teaches everyone to ignore red tests. [`ground_truth`] prints which file
//! was missing, so a skip is never mistaken for a pass.
//!
//! The exports are found **by extension at the repository root**, not by name, so
//! whichever board you hold works and no board's title is baked into the source.

use std::path::{Path, PathBuf};
use vellum_doc::{Board, ItemKind};
use vellum_import::pipeline;
use vellum_import::rtb::{ArchiveSet, RtbArchive};
use vellum_store::BlobStore;

/// Per-type counts from `miro-peek captures/reference-board.html`, cross-checked
/// against the SVG export. 596 objects, all of them mapped.
const EXPECTED: &[(&str, usize)] = &[
    ("ink", 219),
    ("image", 122),
    ("link_preview", 91),
    ("text", 46),
    ("sticky", 44),
    ("embed", 41),
    ("connector", 18),
    ("frame", 12),
    ("document", 1),
    ("group", 1),
    ("rich_document", 1),
];

const TOTAL: usize = 596;

/// Distinct `.rtb` resources the import joins — **every asset in the archive**.
///
/// This was 123 while only image and document widgets claimed a resource. Link cards
/// claim one too: Miro downloads a page's preview picture and stores it as a board
/// resource, referenced from the widget's `resourceWidget`. Taking those adds 57 of the
/// 91 `preview` cards and 25 of the 41 `embed` cards — 82 — and 123 + 82 is exactly the
/// **205** assets the archive holds, so nothing in it is unused any more.
///
/// The remainder claim a resource the archive does not have (7 previews, 15 embeds):
/// Miro prunes a preview image the board no longer displays. Those keep their
/// `meta.externalLink` and are the fetch pool's job.
const DISTINCT_ASSETS: usize = 205;

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("crate is in the workspace")
}

/// The first file with this extension sitting at the repository root.
///
/// By extension rather than by name: the exports are git-ignored and named after
/// whichever board they came from, so hardcoding a title would both bake someone's
/// board name into the source and only ever work on one machine.
fn export_at_root(root: &Path, extension: &str) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == extension))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The first `.html` capture in `captures/`, whatever it is called.
fn capture_in(root: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(root.join("captures"))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "html"))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The captured exports, or `None` with a printed reason.
fn ground_truth() -> Option<(String, PathBuf, PathBuf)> {
    let root = repo_root();
    let clipboard = capture_in(&root);
    let archive = export_at_root(&root, "rtb");
    let oracle = export_at_root(&root, "svg");

    let Some(clipboard) = clipboard else {
        eprintln!(
            "skipping: no .html capture in {}/captures (the reference exports are \
             git-ignored; see docs/02-miro-formats.md)",
            root.display()
        );
        return None;
    };
    let Some(archive) = archive else {
        eprintln!(
            "skipping: no .rtb at {} (the reference exports are git-ignored; \
             see docs/02-miro-formats.md)",
            root.display()
        );
        return None;
    };
    // The oracle is optional — the counts stand on the clipboard and the archive.
    let oracle = oracle.unwrap_or_else(|| root.join("oracle.svg"));
    Some((std::fs::read_to_string(&clipboard).expect("reading the capture"), archive, oracle))
}

/// The whole pipeline against the whole board: 596 items, the exact type mix,
/// every asset joined out of the `.rtb`, and the SVG oracle agreeing.
#[test]
fn the_reference_board_imports_completely() {
    let Some((clipboard, archive_path, oracle_path)) = ground_truth() else { return };

    let home = tempfile::tempdir().expect("a temp directory");
    let blobs = BlobStore::open(home.path().join("blobs")).expect("opening the blob store");
    let mut archive = ArchiveSet::from(RtbArchive::open(&archive_path).expect("opening the .rtb"));
    let mut board = Board::new();

    let mut outcome = pipeline::import(&clipboard, Some(&mut archive), &blobs, &mut board)
        .expect("the capture is a Miro payload")
        .expect("the capture is a Miro payload");

    // ----- structure ------------------------------------------------------

    assert_eq!(outcome.total(), TOTAL, "one item per clipboard object");
    assert_eq!(board.item_count(), TOTAL, "and all of them on the board");
    assert_eq!(outcome.items.len(), TOTAL);

    for (label, expected) in EXPECTED {
        assert_eq!(
            outcome.counts.get(*label),
            Some(expected),
            "{label}: {:?}",
            outcome.counts
        );
    }
    assert_eq!(outcome.counts.values().sum::<usize>(), TOTAL, "an unexpected type appeared");
    assert!(outcome.unmapped_types.is_empty(), "unmapped: {:?}", outcome.unmapped_types);

    // ----- hierarchy ------------------------------------------------------

    // 485 of 596 widgets name a parent through `_parent`, and the group makes 486:
    // it has no `_parent` of its own and inherits the frame its members were in.
    // The containers are the 12 frames, that group, and the handful of images and
    // stickies that carry ink drawn on top of them.
    let nested = outcome.items.iter().filter(|&&id| board.parent_of(id).is_some()).count();
    assert_eq!(nested, 486, "the parent chain must survive the rebuild");
    assert_eq!(board.children(None).len(), TOTAL - nested);

    // Frame 263 contains group 595, which contains widgets 266 and 267 — the one
    // place on this board where Miro's two containment mechanisms interlock.
    let (frame, group) = (outcome.items[263], outcome.items[595]);
    assert_eq!(board.parent_of(group), Some(frame));
    assert_eq!(board.children(Some(group)), vec![outcome.items[266], outcome.items[267]]);

    // Reparenting must not move anything: absolute in, absolute out. Connectors
    // and the group are excluded because Miro gives them no `_position` at all —
    // a connector *is* its endpoints and a group *is* its members — so the
    // pipeline derives their placement rather than preserving one.
    let mapped = vellum_import::import_clipboard(&clipboard).unwrap().unwrap();
    let mut checked = 0;
    for (widget, &id) in mapped.widgets.iter().zip(&outcome.items) {
        if widget.raw.get("_position").is_none_or(serde_json::Value::is_null) {
            continue;
        }
        let placed = board.item(id).unwrap().placement;
        assert_eq!(
            (placed.x, placed.y),
            (widget.placement.x, widget.placement.y),
            "nesting moved a {} widget",
            widget.kind.label()
        );
        checked += 1;
    }
    assert_eq!(checked, TOTAL - 18 - 1, "every widget but the connectors and the group");

    // ----- z-order --------------------------------------------------------

    // Every stack on the board — the roots and each container's children — must
    // run in the order Miro listed them.
    let position: std::collections::HashMap<_, _> =
        outcome.items.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let stacks = std::iter::once(None).chain(outcome.items.iter().copied().map(Some));
    for parent in stacks {
        let order: Vec<usize> = board.children(parent).iter().map(|id| position[id]).collect();
        assert!(order.is_sorted(), "z-order scrambled under {parent:?}: {order:?}");
    }

    // ----- assets ---------------------------------------------------------

    assert!(outcome.missing_assets.is_empty(), "unresolved: {:?}", outcome.missing_assets);
    assert_eq!(outcome.assets_recovered, DISTINCT_ASSETS, "every referenced asset joined");
    assert!(outcome.asset_bytes > 50_000_000, "recovered only {} bytes", outcome.asset_bytes);

    let mut with_bytes = 0;
    for &id in &outcome.items {
        let ItemKind::Image { asset_id, .. } = board.item(id).unwrap().kind else { continue };
        assert!(!asset_id.is_empty(), "an image imported with no asset");
        let hash = asset_id.parse().expect("the item holds a BLAKE3 hex hash");
        assert!(blobs.get(&hash).unwrap().is_some(), "{asset_id} is not in the blob store");
        with_bytes += 1;
    }
    // 122 images plus the one PDF, which has no item kind of its own yet.
    assert_eq!(with_bytes, 123);

    // ----- the oracle -----------------------------------------------------

    if oracle_path.exists() {
        let discrepancies = outcome.cross_check(&oracle_path).expect("reading the SVG export");
        assert!(discrepancies.is_empty(), "the SVG export disagrees: {discrepancies:?}");
    } else {
        eprintln!("note: {} is absent, so the oracle was not run", oracle_path.display());
    }

    // ----- the summary ----------------------------------------------------

    let summary = outcome.to_string();
    assert!(summary.contains("596 items imported from Miro"), "{summary}");
    assert!(summary.contains("219  ink"), "{summary}");
    assert!(summary.contains("assets: 205 of 205 recovered"), "{summary}");
    println!("{summary}");
}

/// What actually survives, stated as a number so a regression is visible rather
/// than argued about.
///
/// 441 of 596 widgets — 74% — carry through with no loss at all: every ink stroke,
/// every image, every sticky, all 12 frames, and 44 of the 46 text widgets.
///
/// The other 155 are 153 widgets of a type Vellum has no item kind for yet, plus
/// the 2 text widgets whose rich text contains a bulleted list. All of them still
/// import placed, sized, nested and stacked; the shortfall is named per type in
/// the fidelity report.
///
/// Frames left this list when they stopped becoming text items. They had been the
/// largest single group that imported and then drew nothing at all, because the
/// painter's `Text` arm pushes no geometry.
#[test]
fn the_lossless_fraction_of_the_reference_board_is_known() {
    let Some((clipboard, archive_path, _)) = ground_truth() else { return };

    let home = tempfile::tempdir().expect("a temp directory");
    let blobs = BlobStore::open(home.path().join("blobs")).expect("opening the blob store");
    let mut archive = ArchiveSet::from(RtbArchive::open(&archive_path).expect("opening the .rtb"));
    let mut board = Board::new();

    let outcome = pipeline::import(&clipboard, Some(&mut archive), &blobs, &mut board)
        .unwrap()
        .unwrap();

    // embed 41 + connector 18 + document 1 + group 1 + rich_document 1 = 62 with no matching
    // item kind, plus the 2 text widgets carrying a list. The 12 frames left this list when
    // they stopped becoming text; the **91 link previews** left it when they became cards.
    let by_type: std::collections::BTreeMap<_, _> =
        outcome.degraded.iter().map(|(d, n)| (d.miro_type, *n)).collect();
    assert_eq!(
        by_type,
        [
            ("connector", 18),
            ("document", 1),
            ("embed", 41),
            ("group", 1),
            ("rich_document", 1),
            ("sticky/text with a list", 2),
        ]
        .into_iter()
        .collect()
    );

    let degraded: usize = outcome.degraded.iter().map(|(_, n)| n).sum();
    assert_eq!(degraded, 64);
    assert_eq!(outcome.lossless(), 532);
    // ink + image + sticky + frame + link_preview + (text − the two with lists).
    assert_eq!(outcome.lossless(), 219 + 122 + 44 + 12 + 91 + 46 - 2);

    // The 41 embeds are still reported, because the live frame genuinely is not imported —
    // but they are cards now, with a title, a link and a provider, rather than paragraphs.
    let (embed, count) =
        outcome.degraded.iter().find(|(d, _)| d.miro_type == "embed").expect("still reported");
    assert_eq!(*count, 41);
    assert_eq!(embed.imported_as, "embed", "no longer `text`");

    // Nothing is lossy for an undeclared reason.
    for (degradation, _) in &outcome.degraded {
        assert!(!degradation.lost.is_empty(), "{} has no stated cost", degradation.miro_type);
    }
}
