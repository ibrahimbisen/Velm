//! The autosave engine seen from where the app sits: mutate, never save, and check
//! the file anyway.
//!
//! The unit tests in `src/autosave.rs` pin the coalescing policy with a fake clock.
//! These run the real thread against a real database, because the properties that
//! matter — "a burst is one write", "an unstopping burst is still written on time",
//! "closing the app loses nothing" — are properties of the whole assembly.
//!
//! Timings here are deliberately short (tens of milliseconds) so the suite stays
//! quick, and every assertion is one-sided: *at least* this much was saved, *at
//! most* this many writes happened. A slow machine makes them pass more easily, not
//! less, which is the only way a timing test earns its place.

use std::time::{Duration, Instant};
use vellum_doc::{Board, ItemId, ItemKind, NewItem, Placement, StyledText};
use vellum_store::{
    Autosave, AutosaveConfig, BOARD_EXTENSION, BlobStore, BoardDb, REPLACED_BY_RESTORE,
};

/// Short enough to keep the suite fast, and still an order of magnitude above the
/// scheduler noise that would make the tests lie.
fn brisk() -> AutosaveConfig {
    AutosaveConfig {
        coalesce: Duration::from_millis(15),
        max_staleness: Duration::from_millis(60),
        restore_point_interval: None,
        ..AutosaveConfig::default()
    }
}

fn sticky(text: &str) -> NewItem {
    NewItem::new(
        ItemKind::Sticky { text: StyledText::plain(text), background: None },
        Placement::new(0.0, 0.0, 199.0, 228.0),
    )
}

struct Fixture {
    home: tempfile::TempDir,
    board: Board,
    autosave: Autosave,
}

impl Fixture {
    fn new(config: AutosaveConfig) -> Self {
        let home = tempfile::tempdir().unwrap();
        let board = Board::new();
        let db = BoardDb::open(home.path().join(format!("board.{BOARD_EXTENSION}"))).unwrap();
        let autosave = Autosave::with_config(db, &board, config).unwrap();
        Self { home, board, autosave }
    }

    fn path(&self) -> std::path::PathBuf {
        self.home.path().join(format!("board.{BOARD_EXTENSION}"))
    }

    fn add(&mut self, text: &str) -> ItemId {
        let id = self.board.add(sticky(text)).unwrap();
        self.autosave.record(&self.board).unwrap();
        id
    }

    /// What a second process would see right now. Opening a separate connection is
    /// the point: it reads what is actually committed, not what the engine believes.
    fn on_disk(&self) -> Option<Board> {
        BoardDb::open(self.path()).unwrap().load().unwrap()
    }

    fn items_on_disk(&self) -> usize {
        self.on_disk().map_or(0, |board| board.item_count())
    }

    /// Waits for the engine to reach `wanted` items on disk, or gives up.
    fn wait_for_disk(&self, wanted: usize, patience: Duration) -> usize {
        self.wait_until(patience, |fixture| fixture.items_on_disk() >= wanted);
        self.items_on_disk()
    }

    fn wait_until(&self, patience: Duration, done: impl Fn(&Self) -> bool) -> bool {
        let until = Instant::now() + patience;
        loop {
            if done(self) {
                return true;
            }
            if Instant::now() > until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// The whole promise in one test: edit, never ask for a save, and the work is on
/// disk anyway.
#[test]
fn an_edit_reaches_the_disk_without_anyone_asking() {
    let mut fixture = Fixture::new(brisk());
    fixture.board.set_title("Engine bay").unwrap();
    fixture.add("radiator");

    assert_eq!(fixture.wait_for_disk(1, Duration::from_secs(2)), 1);
    let stored = fixture.on_disk().unwrap();
    assert_eq!(stored.title(), "Engine bay");
    assert_eq!(stored.items().unwrap(), fixture.board.items().unwrap());
}

/// The replica the writer keeps has to be the *same document*, not merely one with
/// the same items in it. Undo, redo, reparenting and restacking all produce
/// operations that a naive shadow would get wrong, so a session exercises them all
/// and the file is compared against the document at the end.
#[test]
fn a_full_editing_session_reloads_as_the_document_the_user_had() {
    let mut fixture = Fixture::new(brisk());
    fixture.board.set_title("Cooling system").unwrap();

    let frame = fixture.add("frame");
    let note = fixture.add("fan");
    let stroke = fixture.add("pen stroke");
    let doomed = fixture.add("deleted later");

    for step in 0..20 {
        fixture.board.translate(note, 1.5, -0.5).unwrap();
        fixture.board.set_title(&format!("session step {step}")).unwrap();
        fixture.autosave.record(&fixture.board).unwrap();
    }

    fixture.board.undo().unwrap();
    fixture.board.undo().unwrap();
    fixture.board.redo().unwrap();
    fixture.autosave.record(&fixture.board).unwrap();

    fixture.board.reparent(note, Some(frame)).unwrap();
    fixture.board.reparent(stroke, Some(frame)).unwrap();
    fixture.board.bring_to_front(frame).unwrap();
    fixture.board.remove(doomed).unwrap();
    fixture.autosave.record(&fixture.board).unwrap();

    fixture.autosave.flush(&fixture.board).unwrap();

    let stored = fixture.on_disk().unwrap();
    assert_eq!(stored.title(), fixture.board.title());
    assert_eq!(stored.items().unwrap(), fixture.board.items().unwrap());
    assert_eq!(stored.item_ids(), fixture.board.item_ids());
    assert_eq!(stored.parent_of(note), Some(frame));
    assert!(!stored.contains(doomed));
}

/// A board that is created and then left alone still has to exist on disk, or a
/// crash before the first edit would lose the board itself.
#[test]
fn a_board_that_is_never_edited_is_still_written() {
    let fixture = Fixture::new(brisk());
    assert!(
        fixture.wait_until(Duration::from_secs(2), |f| f.on_disk().is_some()),
        "an untouched board never reached the disk"
    );
    assert_eq!(fixture.items_on_disk(), 0);
}

/// Dragging a sticky emits a mutation per mouse move. Writing each one would be the
/// stutter this project exists to avoid.
#[test]
fn a_burst_of_mutations_becomes_a_handful_of_writes() {
    let mut fixture = Fixture::new(brisk());
    let id = fixture.add("dragged");

    for step in 0..600 {
        fixture.board.translate(id, 0.5, 0.25).unwrap();
        fixture.autosave.record(&fixture.board).unwrap();
        if step % 50 == 0 {
            // A drag is not an infinitely tight loop; give the writer a chance to
            // behave badly if it is going to.
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fixture.autosave.flush(&fixture.board).unwrap();

    let stats = fixture.autosave.stats();
    assert_eq!(stats.deltas_sent, 601);
    assert!(
        stats.writes <= 12,
        "601 mutations caused {} disk writes; coalescing is not working",
        stats.writes
    );
    assert!(stats.is_durable());
    assert_eq!(fixture.items_on_disk(), 1);
    assert_eq!(
        fixture.on_disk().unwrap().item(id).unwrap().placement.x,
        fixture.board.item(id).unwrap().placement.x
    );
}

/// The guarantee that makes coalescing safe: a burst that never settles is still
/// written continuously, so the amount at risk stays bounded no matter how long the
/// user keeps their hand down.
#[test]
fn a_burst_that_never_settles_is_still_written_on_the_ceiling() {
    let config = AutosaveConfig {
        coalesce: Duration::from_millis(500),
        max_staleness: Duration::from_millis(50),
        ..brisk()
    };
    let mut fixture = Fixture::new(config);

    let started = Instant::now();
    let mut added = 0usize;
    // Mutate without pause for well past the ceiling, but never past the quiet
    // period, so only the ceiling can be causing a write.
    while started.elapsed() < Duration::from_millis(400) {
        fixture.add(&format!("note {added}"));
        added += 1;
        std::thread::sleep(Duration::from_millis(2));
    }

    let on_disk = fixture.items_on_disk();
    let writes = fixture.autosave.stats().writes;
    assert!(writes >= 3, "only {writes} writes in 400ms of unbroken editing");
    assert!(
        on_disk > added / 2,
        "{on_disk} of {added} items were on disk during an unbroken burst"
    );
}

#[test]
fn flushing_makes_everything_durable_before_it_returns() {
    let mut fixture = Fixture::new(AutosaveConfig {
        // Long enough that nothing would be written on its own during this test.
        coalesce: Duration::from_secs(30),
        max_staleness: Duration::from_secs(30),
        ..brisk()
    });
    for i in 0..25 {
        fixture.add(&format!("note {i}"));
    }
    assert_eq!(fixture.items_on_disk(), 0, "something was written before the flush");

    fixture.autosave.flush(&fixture.board).unwrap();

    assert_eq!(fixture.items_on_disk(), 25);
    assert!(fixture.autosave.stats().is_durable());
}

/// Quitting must not need a save prompt. Dropping the handle is the app closing.
#[test]
fn dropping_the_engine_writes_everything_still_pending() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(format!("board.{BOARD_EXTENSION}"));
    let mut board = Board::new();
    {
        let db = BoardDb::open(&path).unwrap();
        let mut autosave = Autosave::with_config(
            db,
            &board,
            AutosaveConfig {
                coalesce: Duration::from_secs(30),
                max_staleness: Duration::from_secs(30),
                ..brisk()
            },
        )
        .unwrap();
        for i in 0..40 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            autosave.record(&board).unwrap();
        }
        assert!(
            BoardDb::open(&path).unwrap().load().unwrap().is_none(),
            "something was written before the handle dropped"
        );
    }

    let reopened = BoardDb::open(&path).unwrap();
    assert!(reopened.recovery().is_clean(), "an orderly exit looked like a crash");
    let stored = BoardDb::open(&path).unwrap().load().unwrap().unwrap();
    assert_eq!(stored.items().unwrap(), board.items().unwrap());
}

#[test]
fn stopping_hands_the_database_back_current() {
    let mut fixture = Fixture::new(brisk());
    for i in 0..10 {
        fixture.add(&format!("note {i}"));
    }
    let Fixture { autosave, board, .. } = fixture;

    let mut db = autosave.stop().unwrap();

    assert_eq!(db.load().unwrap().unwrap().items().unwrap(), board.items().unwrap());
    assert!(db.recovery().is_clean());
}

/// The board library and the renderer both need the database while a session is
/// running, and the writer thread owns it. Those calls have to keep working.
#[test]
fn the_library_row_and_thumbnail_stay_reachable_while_the_engine_runs() {
    let mut fixture = Fixture::new(brisk());
    let blobs = BlobStore::open(fixture.home.path().join("blobs")).unwrap();
    let thumbnail = blobs.put(b"a rendered preview").unwrap();

    fixture.board.set_title("Reference Board").unwrap();
    for i in 0..7 {
        fixture.add(&format!("note {i}"));
    }
    fixture.autosave.flush(&fixture.board).unwrap();
    fixture.autosave.set_thumbnail(Some(&thumbnail)).unwrap();

    let index = fixture.autosave.index().unwrap().unwrap();
    assert_eq!(index.title, "Reference Board");
    assert_eq!(index.item_count, 7);
    assert_eq!(index.thumbnail, Some(thumbnail));

    // And the same row is what a second process listing the library would see.
    let library = vellum_store::list_boards(fixture.home.path()).unwrap();
    assert_eq!(library.len(), 1);
    assert_eq!(library[0].thumbnail, Some(thumbnail));
}

#[test]
fn compacting_runs_on_the_writer_and_leaves_the_board_current() {
    let mut fixture = Fixture::new(brisk());
    for i in 0..20 {
        fixture.add(&format!("note {i}"));
        std::thread::sleep(Duration::from_millis(3));
    }
    fixture.autosave.flush(&fixture.board).unwrap();

    fixture.autosave.compact().unwrap();

    let db = BoardDb::open(fixture.path()).unwrap();
    assert_eq!(db.chunk_count().unwrap(), 1);
    assert_eq!(fixture.items_on_disk(), 20);
}

/// Back-pressure must degrade into coarser batching, never into a stall and never
/// into losing an edit.
#[test]
fn a_full_queue_defers_deltas_instead_of_blocking_or_losing_them() {
    let mut fixture = Fixture::new(AutosaveConfig {
        // Zero means every send after the first sees a full queue.
        max_queued_bytes: 0,
        coalesce: Duration::from_secs(30),
        max_staleness: Duration::from_secs(30),
        ..brisk()
    });

    let started = Instant::now();
    for i in 0..200 {
        fixture.add(&format!("note {i}"));
    }
    let recording = started.elapsed();

    assert!(recording < Duration::from_millis(500), "recording blocked for {recording:?}");
    let stats = fixture.autosave.stats();
    assert!(stats.deltas_deferred > 0, "back-pressure never engaged");
    assert!(!stats.is_durable(), "a change held back by back-pressure was reported as saved");

    // Nothing was lost: the deferred changes are folded into the next delta.
    fixture.autosave.flush(&fixture.board).unwrap();
    assert_eq!(fixture.items_on_disk(), 200);
    assert_eq!(
        fixture.on_disk().unwrap().items().unwrap(),
        fixture.board.items().unwrap(),
        "a deferred change did not survive"
    );
}

#[test]
fn recording_an_unchanged_board_costs_nothing() {
    let mut fixture = Fixture::new(brisk());
    fixture.add("note");
    let sent = fixture.autosave.stats().deltas_sent;

    for _ in 0..1_000 {
        fixture.autosave.record(&fixture.board).unwrap();
    }

    assert_eq!(fixture.autosave.stats().deltas_sent, sent);
}

// ----- version history ------------------------------------------------------

#[test]
fn a_restore_point_taken_through_the_engine_captures_the_latest_edit() {
    let mut fixture = Fixture::new(brisk());
    fixture.board.set_title("Draft").unwrap();
    fixture.add("first");

    let point = fixture.autosave.create_restore_point(&fixture.board, "first draft").unwrap();

    fixture.board.set_title("Rewritten").unwrap();
    fixture.add("second");
    fixture.autosave.flush(&fixture.board).unwrap();

    let points = fixture.autosave.restore_points().unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].id, point);
    assert_eq!(points[0].label.as_deref(), Some("first draft"));
    assert!(!points[0].automatic);
    assert_eq!(points[0].item_count, 1);

    let history = fixture.autosave.read_restore_point(point).unwrap().unwrap();
    assert_eq!(history.title(), "Draft");
    assert_eq!(history.item_count(), 1);
}

#[test]
fn restoring_through_the_engine_rewinds_the_file_the_shadow_and_the_caller() {
    let mut fixture = Fixture::new(brisk());
    fixture.board.set_title("Before").unwrap();
    fixture.add("kept");
    let point = fixture.autosave.create_restore_point(&fixture.board, "before").unwrap();

    fixture.board.set_title("After").unwrap();
    for i in 0..5 {
        fixture.add(&format!("mistake {i}"));
    }
    fixture.autosave.flush(&fixture.board).unwrap();

    fixture.board = fixture.autosave.restore(point).unwrap();

    assert_eq!(fixture.board.title(), "Before");
    assert_eq!(fixture.board.item_count(), 1);
    assert_eq!(fixture.items_on_disk(), 1);

    // Editing continues normally, on top of the restored state, with no leftovers
    // from the version that was replaced.
    fixture.add("after the restore");
    fixture.autosave.flush(&fixture.board).unwrap();
    let stored = fixture.on_disk().unwrap();
    assert_eq!(stored.item_count(), 2);
    assert_eq!(stored.items().unwrap(), fixture.board.items().unwrap());

    // And the mistake is still reachable, because the restore kept it.
    let safety = fixture
        .autosave
        .restore_points()
        .unwrap()
        .into_iter()
        .find(|p| p.label.as_deref() == Some(REPLACED_BY_RESTORE))
        .expect("the replaced state was not kept");
    assert_eq!(safety.item_count, 6);
}

/// History that only exists when the user remembers to ask for it is not history.
#[test]
fn the_engine_takes_restore_points_on_its_own_while_the_board_changes() {
    let mut fixture = Fixture::new(AutosaveConfig {
        restore_point_interval: Some(Duration::from_millis(30)),
        restore_points_kept: 3,
        ..brisk()
    });

    let started = Instant::now();
    let mut i = 0;
    while started.elapsed() < Duration::from_millis(400) {
        fixture.add(&format!("note {i}"));
        i += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
    fixture.autosave.flush(&fixture.board).unwrap();

    let stats = fixture.autosave.stats();
    assert!(stats.restore_points >= 3, "only {} restore points in 400ms", stats.restore_points);

    // Pruning keeps the history from growing without limit.
    let points = fixture.autosave.restore_points().unwrap();
    assert_eq!(points.len(), 3, "automatic history was not pruned to its limit");
    assert!(points.iter().all(|p| p.automatic));
    assert!(points[0].item_count >= points[1].item_count);
}

#[test]
fn history_is_off_when_the_interval_is_none() {
    let mut fixture = Fixture::new(brisk());
    for i in 0..30 {
        fixture.add(&format!("note {i}"));
        std::thread::sleep(Duration::from_millis(3));
    }
    fixture.autosave.flush(&fixture.board).unwrap();

    assert_eq!(fixture.autosave.stats().restore_points, 0);
    assert!(fixture.autosave.restore_points().unwrap().is_empty());
}

/// Reopening a board that autosave has been writing must resume incremental saving
/// rather than starting the file over.
#[test]
fn a_session_resumes_where_the_last_one_stopped() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(format!("board.{BOARD_EXTENSION}"));

    let mut board = Board::new();
    {
        let db = BoardDb::open(&path).unwrap();
        let mut autosave = Autosave::with_config(db, &board, brisk()).unwrap();
        for i in 0..30 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            autosave.record(&board).unwrap();
        }
        autosave.flush(&board).unwrap();
    }

    let mut db = BoardDb::open(&path).unwrap();
    let mut board = db.load().unwrap().unwrap();
    let chunks = db.chunk_count().unwrap();
    let mut autosave = Autosave::with_config(db, &board, brisk()).unwrap();

    board.set_title("resumed").unwrap();
    autosave.record(&board).unwrap();
    autosave.flush(&board).unwrap();

    let db = autosave.stop().unwrap();
    assert_eq!(db.chunk_count().unwrap(), chunks + 1, "the second session rewrote the snapshot");
    let stored = BoardDb::open(&path).unwrap().load().unwrap().unwrap();
    assert_eq!(stored.title(), "resumed");
    assert_eq!(stored.item_count(), 30);
}
