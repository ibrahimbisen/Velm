//! Crash safety, proved rather than asserted.
//!
//! WAL plus `synchronous = FULL` is *supposed* to mean that a committed transaction
//! survives a process dying mid-write. That is a claim about SQLite, the filesystem,
//! the pragmas actually taking effect, and this crate using them correctly — and
//! three of those four are only testable by killing something.
//!
//! So these tests spawn a real subprocess, let it edit a real board, `SIGKILL` it
//! without warning, and then reopen the file and check three things:
//!
//! 1. the crash is **detected** — the reopened board says the last session never
//!    ended;
//! 2. **nothing reported durable is missing** — everything the victim was told had
//!    been committed is still there, exactly;
//! 3. **the loss is bounded** — work older than the autosave staleness window
//!    survived even though the victim never asked for it to be saved, and what did
//!    survive is a clean prefix of the victim's history with no torn tail.
//!
//! The victim is this same test binary, re-invoked with an environment variable and
//! the name of the `#[ignore]`d test below. That keeps the fixture in one file, and
//! means the process being killed is running exactly the code that ships.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use vellum_doc::{Board, ItemKind, NewItem, Placement, StyledText};
use vellum_store::{Autosave, AutosaveConfig, BOARD_EXTENSION, BoardDb};

/// Path of the board the victim should edit. Its presence is what turns the
/// `#[ignore]`d test below from a no-op into the victim.
const BOARD_ENV: &str = "VELLUM_CRASH_BOARD";
/// `autosave` to exercise the background writer, `sync` to exercise `BoardDb::save`
/// on its own.
const MODE_ENV: &str = "VELLUM_CRASH_MODE";

/// Staleness ceiling the victim's autosave runs with. Shorter than the shipping
/// default so a test does not have to wait a quarter of a second to prove anything.
const VICTIM_STALENESS: Duration = Duration::from_millis(120);

/// How far back from the moment of death work is required to have survived.
///
/// The engine promises `VICTIM_STALENESS` plus one transaction. This allows an order
/// of magnitude more, because the number being defended here is "bounded", not
/// "120ms": a loaded CI machine can suspend a thread for a long time, and a test
/// that flakes on that teaches nothing. The measured window is printed, and it is
/// far smaller than this.
const GRACE: Duration = Duration::from_millis(1_500);

#[test]
fn a_board_killed_mid_autosave_reopens_consistent_and_loses_only_the_recent_window() {
    let Some(home) = crash_test_home() else { return };
    let path = home.path().join(format!("victim.{BOARD_EXTENSION}"));

    let victim = Victim::spawn(&path, "autosave");
    // Long enough for many autosave cycles, so the kill lands in the middle of an
    // editing session rather than at its very start.
    let outcome = victim.run_for(Duration::from_millis(1_800)).kill();

    let survived = assert_board_recovered_cleanly(&path, &outcome);

    let lost = outcome.added.saturating_sub(survived);
    let window = outcome.window_lost(survived);
    println!(
        "autosave crash: {} items added, {} reported durable, {survived} survived \
         ({lost} lost, covering the last {window:?} before the kill)",
        outcome.added, outcome.durable
    );
    assert!(
        survived >= outcome.added_by(outcome.killed_at - GRACE),
        "work older than the staleness window was lost: {survived} of {} items",
        outcome.added
    );
    assert!(window < GRACE, "the loss window was {window:?}, which is not bounded by anything");
}

/// The same proof one layer down, with no background thread involved: every
/// `BoardDb::save` that returns has, by claim, committed. Killing the process
/// between saves must therefore never cost more than the save in flight.
#[test]
fn a_board_killed_mid_save_keeps_every_save_that_returned() {
    let Some(home) = crash_test_home() else { return };
    let path = home.path().join(format!("victim.{BOARD_EXTENSION}"));

    let victim = Victim::spawn(&path, "sync");
    let outcome = victim.run_for(Duration::from_millis(1_200)).kill();

    let survived = assert_board_recovered_cleanly(&path, &outcome);

    println!("synchronous crash: {} saves returned, {survived} survived", outcome.durable);
    // A save that returned is a commit that happened, so there is no window at all in
    // the direction that matters — that half is asserted for every mode in
    // `assert_board_recovered_cleanly`. The one item of slack in the other direction
    // is the victim's own reporting race, not the store's: the save whose commit
    // landed in the instant between returning and being able to say so.
    assert!(
        survived <= outcome.durable + 1,
        "{survived} items survived against {} acknowledged saves",
        outcome.durable
    );
}

/// Killing a process repeatedly, each time on top of the file the last one left,
/// is where a recovery bug that only bites on the second crash would show up.
#[test]
fn a_board_survives_being_killed_over_and_over() {
    let Some(home) = crash_test_home() else { return };
    let path = home.path().join(format!("victim.{BOARD_EXTENSION}"));
    let mut carried = 0u64;

    for round in 0..3 {
        let outcome = Victim::spawn(&path, "sync")
            .run_for(Duration::from_millis(600))
            .kill();

        let survived = assert_board_recovered_cleanly(&path, &outcome);
        assert!(
            survived > carried,
            "round {round} made no progress on top of the {carried} items it inherited"
        );
        carried = survived;

        // Reopening and recovering must leave the file usable by the next session,
        // which is the part a one-shot test would never check.
        let reopened = BoardDb::open(&path).unwrap();
        assert!(reopened.recovery().is_clean(), "recovery left the crash mark set");
    }
    assert!(carried > 0);
}

/// Everything the parent side of a crash test does with the file afterwards.
/// Returns how many items survived.
fn assert_board_recovered_cleanly(path: &Path, outcome: &Outcome) -> u64 {
    assert!(outcome.added > 0, "the victim never got started");

    let mut db = BoardDb::open(path).unwrap();
    let recovery = db.recovery();
    assert!(recovery.unclean_shutdown, "a killed process was not detected as a crash");
    assert!(
        db.check_integrity().unwrap(),
        "SQLite reported the file itself as damaged after a kill"
    );

    let recovered = db.recover().unwrap().expect("the killed board came back empty");
    assert_eq!(
        recovered.discarded_chunks, 0,
        "a chunk was torn: atomic commits should make that impossible"
    );
    assert_eq!(recovered.from_restore_point, None, "recovery had to fall back to history");

    let board = recovered.board;
    let survived = board.item_count() as u64;
    assert!(
        survived <= outcome.added,
        "{survived} items came back but only {} were ever added",
        outcome.added
    );
    assert!(
        survived >= outcome.durable,
        "{survived} items came back but {} had been reported durable",
        outcome.durable
    );

    // What survived has to be a clean prefix of what the victim did: no gaps, no
    // half-written item, no operation applied without the one before it.
    let notes: Vec<String> = board
        .items()
        .unwrap()
        .into_iter()
        .map(|item| match item.kind {
            ItemKind::Sticky { text, .. } => text.to_plain(),
            other => panic!("unexpected item on the recovered board: {other:?}"),
        })
        .collect();
    let expected: Vec<String> = (0..survived).map(|i| format!("note {i}")).collect();
    assert_eq!(notes, expected, "the recovered board is not a prefix of the victim's history");

    survived
}

/// The subprocess. Does nothing unless [`BOARD_ENV`] is set, so a plain
/// `cargo test -- --ignored` cannot start an editing session that never ends.
#[test]
#[ignore = "spawned as a subprocess by the crash tests; not a test on its own"]
fn crash_victim() {
    let Ok(path) = std::env::var(BOARD_ENV) else {
        return;
    };
    let mode = std::env::var(MODE_ENV).unwrap_or_default();
    let db = BoardDb::open(&path).unwrap();
    // Picking up where the previous life left off is what makes repeated kills a
    // real test rather than three independent ones.
    let mut db = db;
    let mut board = db.recover().unwrap().map_or_else(Board::new, |r| r.board);
    let carried = board.item_count() as u64;

    match mode.as_str() {
        "autosave" => {
            let mut autosave = Autosave::with_config(
                db,
                &board,
                AutosaveConfig {
                    coalesce: Duration::from_millis(20),
                    max_staleness: VICTIM_STALENESS,
                    restore_point_interval: None,
                    ..AutosaveConfig::default()
                },
            )
            .unwrap();

            for n in carried.. {
                board.add(note(n)).unwrap();
                // Announced *before* it is handed over, so the parent's "added" count
                // can never be behind the file. A kill in the other order would leave
                // an item on disk that was never reported, and the test would read
                // that as the board inventing work.
                report("ADDED", n + 1);
                autosave.record(&board).unwrap();
                // Occasionally ask for a guarantee, so the parent has a number it can
                // hold this crate to with no tolerance at all.
                if (n + 1).is_multiple_of(40) {
                    autosave.flush(&board).unwrap();
                    report("DURABLE", n + 1);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        _ => {
            for n in carried.. {
                board.add(note(n)).unwrap();
                report("ADDED", n + 1);
                db.save(&board).unwrap();
                report("DURABLE", n + 1);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn note(n: u64) -> NewItem {
    NewItem::new(
        ItemKind::Sticky { text: StyledText::plain(format!("note {n}")), background: None },
        Placement::new(n as f64 * 8.0, 0.0, 199.0, 228.0),
    )
}

/// One line per event, flushed, so the parent's clock reading is close to the
/// victim's.
fn report(kind: &str, n: u64) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{kind} {n}");
    let _ = out.flush();
}

/// A running victim process and the stream of what it has managed to do.
struct Victim {
    child: Child,
    events: Receiver<(Instant, Event)>,
}

enum Event {
    Added(u64),
    Durable(u64),
}

/// What the victim had achieved at the moment it was killed.
struct Outcome {
    /// Items it reported adding.
    added: u64,
    /// Items it was told were committed.
    durable: u64,
    killed_at: Instant,
    /// When each item was added, so the bounded-loss claim can be checked against a
    /// clock rather than against a guess.
    timeline: Vec<(Instant, u64)>,
}

impl Outcome {
    /// How many items had been added by `moment`.
    fn added_by(&self, moment: Instant) -> u64 {
        self.timeline
            .iter()
            .take_while(|(at, _)| *at <= moment)
            .map(|(_, n)| *n)
            .last()
            .unwrap_or(0)
    }

    /// How much wall time separates the newest surviving item from the kill — the
    /// staleness window, measured rather than assumed.
    fn window_lost(&self, survived: u64) -> Duration {
        self.timeline
            .iter()
            .find(|(_, n)| *n == survived)
            .map_or(Duration::ZERO, |(at, _)| self.killed_at.saturating_duration_since(*at))
    }
}

impl Victim {
    fn spawn(path: &Path, mode: &str) -> Self {
        let child = Command::new(victim_binary())
            .args(["--ignored", "--exact", "--nocapture", "--test-threads=1", "crash_victim"])
            .env(BOARD_ENV, path)
            .env(MODE_ENV, mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the crash victim");
        Self::reading(child)
    }

    fn reading(mut child: Child) -> Self {
        let stdout = child.stdout.take().expect("piped");
        let (sender, events) = mpsc::channel();
        // A thread rather than polling, so the timestamp on each event is when the
        // victim produced it and not when this test next looked.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let mut parts = line.split_whitespace();
                let event = match (parts.next(), parts.next().and_then(|n| n.parse().ok())) {
                    (Some("ADDED"), Some(n)) => Event::Added(n),
                    (Some("DURABLE"), Some(n)) => Event::Durable(n),
                    // Test-harness chatter.
                    _ => continue,
                };
                if sender.send((Instant::now(), event)).is_err() {
                    return;
                }
            }
        });
        Self { child, events }
    }

    /// Lets the victim work, collecting what it reports, then hands back a killer.
    fn run_for(self, duration: Duration) -> Self {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        self
    }

    fn kill(mut self) -> Outcome {
        // `Child::kill` is `SIGKILL` on Unix and `TerminateProcess` on Windows:
        // no unwinding, no destructors, no flush. Exactly the case that matters.
        self.child.kill().expect("could not kill the victim");
        let killed_at = Instant::now();
        self.child.wait().expect("the victim did not die");

        let mut outcome =
            Outcome { added: 0, durable: 0, killed_at, timeline: Vec::new() };
        // Drain what is already in the pipe. Anything the victim printed after
        // `killed_at` cannot exist, so the timeline is complete by construction.
        while let Ok((at, event)) = self.events.recv_timeout(Duration::from_millis(200)) {
            match event {
                Event::Added(n) => {
                    outcome.added = outcome.added.max(n);
                    outcome.timeline.push((at, n));
                }
                Event::Durable(n) => outcome.durable = outcome.durable.max(n),
            }
        }
        outcome
    }
}

/// The test binary itself. Under `cargo test` that is what `current_exe` is.
fn victim_binary() -> PathBuf {
    std::env::current_exe().expect("a test binary has a path")
}

/// `None` when this process *is* the victim, so the parent tests do not fork
/// recursively, and the victim's own copies of them do nothing.
fn crash_test_home() -> Option<tempfile::TempDir> {
    if std::env::var_os(BOARD_ENV).is_some() {
        return None;
    }
    Some(tempfile::tempdir().expect("a writable temporary directory"))
}
