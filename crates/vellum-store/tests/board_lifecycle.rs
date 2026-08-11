//! The document and persistence layers used together, from outside both crates.
//!
//! The unit tests inside each module check one thing at a time; these check that
//! the seams hold — that a board built through the public API, saved, listed and
//! reopened is the same board, and that its images live in the shared blob store
//! rather than inside the board files.

use std::path::{Path, PathBuf};
use vellum_doc::{
    Board, Color, Crop, ItemId, ItemKind, NewItem, Placement, Point, SpanStyle, StyledText, Style,
    TextSpan,
};
use vellum_store::{BOARD_EXTENSION, BlobStore, BoardDb, Hash, list_boards};

/// A miniature of the reference Miro board: nested frames, rich text, ink and an
/// image whose pixels live in the shared blob store.
fn build_board(logo: &Hash) -> (Board, Vec<ItemId>) {
    let mut board = Board::new();
    board.set_title("Reference Board").unwrap();

    let frame = board
        .add(NewItem::new(
            ItemKind::Text { text: StyledText::plain("Cooling system") },
            Placement::new(-3083.01852968025, 1367.540251981647, 900.0, 120.0),
        ))
        .unwrap();

    let note = board
        .add(
            NewItem::new(
                ItemKind::Sticky {
                    text: StyledText::from_spans([
                        TextSpan::new("fan", SpanStyle::bold()),
                        TextSpan::plain(" — see "),
                        TextSpan::new("the manual", SpanStyle::link("https://example.invalid/m")),
                    ]),
                    background: Some(Color::rgb(0xFF, 0xF7, 0x9E)),
                },
                Placement::new(-3000.0, 1500.0, 199.0, 228.0),
            )
            .with_parent(frame)
            .with_style(Style {
                font_family: Some("Noto Sans".into()),
                line_height: Some(1.36),
                ..Style::default()
            }),
        )
        .unwrap();

    let stroke = board
        .add(NewItem::new(
            ItemKind::Ink {
                points: (0..512).map(|i| Point::new(i as f64 * 0.25, (i as f64).cos())).collect(),
                color: Some(Color::rgb(0x2D, 0xC7, 0x5C).with_opacity(0.6)),
                thickness: 18.0,
            },
            Placement::new(-3351.5, -575.4, 128.0, 64.0),
        ))
        .unwrap();

    let image = board
        .add(NewItem::new(
            ItemKind::Image {
                asset_id: logo.to_hex(),
                crop: Some(Crop { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 }),
            },
            Placement::new(0.0, 0.0, 1920.0, 1080.0),
        ))
        .unwrap();

    (board, vec![frame, note, stroke, image])
}

fn board_path(home: &Path, name: &str) -> PathBuf {
    home.join(format!("{name}.{BOARD_EXTENSION}"))
}

#[test]
fn a_board_saved_and_reopened_is_the_same_board() {
    const PIXELS: &[u8] = b"\x89PNG\r\n\x1a\n pretend this is a screenshot";
    let home = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
    let logo = blobs.put(PIXELS).unwrap();

    let (board, ids) = build_board(&logo);
    let path = board_path(home.path(), "garage");

    let mut db = BoardDb::open(&path).unwrap();
    db.save(&board).unwrap();
    drop(db);

    let mut reopened = BoardDb::open(&path).unwrap();
    let loaded = reopened.load().unwrap().unwrap();

    assert_eq!(loaded.title(), board.title());
    assert_eq!(loaded.items().unwrap(), board.items().unwrap());
    assert_eq!(loaded.item_ids(), ids);
    assert_eq!(loaded.parent_of(ids[1]), Some(ids[0]));

    // The image's pixels came from the shared store, not from the board file.
    let ItemKind::Image { asset_id, .. } = &loaded.item(ids[3]).unwrap().kind else {
        panic!("expected an image")
    };
    let hash: Hash = asset_id.parse().unwrap();
    assert_eq!(blobs.get(&hash).unwrap().unwrap(), PIXELS);
}

/// A whole editing session: repeated incremental saves, an undo, more edits, and a
/// reopen that has to agree with the in-memory document at every step.
#[test]
fn an_editing_session_of_incremental_saves_reloads_faithfully() {
    let home = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
    let logo = blobs.put(b"logo bytes").unwrap();
    let path = board_path(home.path(), "session");

    let (mut board, ids) = build_board(&logo);
    let mut db = BoardDb::open(&path).unwrap();
    db.save(&board).unwrap();

    for step in 0..20 {
        board.translate(ids[1], 1.5, -0.5).unwrap();
        board.set_title(&format!("session step {step}")).unwrap();
        db.save(&board).unwrap();
    }

    board.undo().unwrap();
    db.save(&board).unwrap();

    board.bring_to_front(ids[0]).unwrap();
    board.reparent(ids[2], Some(ids[0])).unwrap();
    db.save(&board).unwrap();
    drop(db);

    let mut reopened = BoardDb::open(&path).unwrap();
    let loaded = reopened.load().unwrap().unwrap();

    assert_eq!(loaded.items().unwrap(), board.items().unwrap());
    assert_eq!(loaded.title(), board.title());
    assert_eq!(loaded.parent_of(ids[2]), Some(ids[0]));
    assert_eq!(loaded.children(None).last(), Some(&ids[0]));
}

/// Reopening resumes incremental saving rather than starting the file over — the
/// second handle has to know what the first one already wrote.
#[test]
fn saving_continues_incrementally_after_a_reopen() {
    let home = tempfile::tempdir().unwrap();
    let path = board_path(home.path(), "resumed");

    let mut first = BoardDb::open(&path).unwrap();
    let mut board = Board::new();
    for i in 0..30 {
        board
            .add(NewItem::new(
                ItemKind::Text { text: StyledText::plain(format!("note {i}")) },
                Placement::new(i as f64, 0.0, 100.0, 40.0),
            ))
            .unwrap();
    }
    first.save(&board).unwrap();
    drop(first);

    let mut second = BoardDb::open(&path).unwrap();
    let mut board = second.load().unwrap().unwrap();
    assert_eq!(second.chunk_count().unwrap(), 1);

    board.set_title("resumed").unwrap();
    second.save(&board).unwrap();
    assert_eq!(second.chunk_count().unwrap(), 2, "the reopened handle rewrote the snapshot");

    let loaded = BoardDb::open(&path).unwrap().load().unwrap().unwrap();
    assert_eq!(loaded.title(), "resumed");
    assert_eq!(loaded.item_count(), 30);
}

/// The library view is the one thing that touches every board at once, so it must
/// stay off the document path entirely.
#[test]
fn the_library_lists_every_board_without_opening_a_document() {
    let home = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(home.path().join("blobs")).unwrap();

    for index in 0..5 {
        let mut db = BoardDb::open(board_path(home.path(), &format!("board-{index}"))).unwrap();
        let mut board = Board::new();
        board.set_title(&format!("Board {index}")).unwrap();
        for item in 0..=index {
            board
                .add(NewItem::new(
                    ItemKind::Text { text: StyledText::plain(format!("item {item}")) },
                    Placement::default(),
                ))
                .unwrap();
        }
        db.save(&board).unwrap();
        let thumbnail = blobs.put(format!("thumbnail for board {index}").as_bytes()).unwrap();
        db.set_thumbnail(Some(&thumbnail)).unwrap();
    }

    let library = list_boards(home.path()).unwrap();

    assert_eq!(library.len(), 5);
    for entry in &library {
        let index: usize =
            entry.title.strip_prefix("Board ").expect("unexpected title").parse().unwrap();
        assert_eq!(entry.item_count as usize, index + 1);

        let thumbnail = entry.thumbnail.expect("every board was given a thumbnail");
        assert_eq!(
            blobs.get(&thumbnail).unwrap().unwrap(),
            format!("thumbnail for board {index}").into_bytes()
        );
    }
}

/// Deduplication across board files is the reason the blob store is shared, so it
/// is checked across boards rather than only across repeated writes.
#[test]
fn an_asset_used_by_many_boards_is_stored_once() {
    let home = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
    let pixels = vec![0xABu8; 64 * 1024];

    let mut hashes = Vec::new();
    for index in 0..20 {
        let hash = blobs.put(&pixels).unwrap();
        hashes.push(hash);

        let mut db = BoardDb::open(board_path(home.path(), &format!("b{index}"))).unwrap();
        let mut board = Board::new();
        board
            .add(NewItem::new(
                ItemKind::Image { asset_id: hash.to_hex(), crop: None },
                Placement::new(0.0, 0.0, 256.0, 256.0),
            ))
            .unwrap();
        db.save(&board).unwrap();
    }

    assert!(hashes.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(stored_blob_count(&blobs), 1, "the same pixels were stored more than once");

    // Every board file stayed small because none of them carries the pixels.
    for index in 0..20 {
        let size = std::fs::metadata(board_path(home.path(), &format!("b{index}"))).unwrap().len();
        assert!(size < pixels.len() as u64, "board {index} is {size} bytes");
    }
}

fn stored_blob_count(blobs: &BlobStore) -> usize {
    std::fs::read_dir(blobs.root())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|shard| shard.path().is_dir())
        .filter(|shard| !shard.file_name().to_string_lossy().starts_with('.'))
        .map(|shard| std::fs::read_dir(shard.path()).unwrap().count())
        .sum()
}
