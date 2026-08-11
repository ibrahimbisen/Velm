//! What autosave costs the thread that draws.
//!
//! The premise of the whole project is that the canvas never stutters, so the claim
//! "saving does not block the UI" needs a number attached to it rather than an
//! architecture diagram. [`Autosave::record`] is the only autosave code that ever
//! runs on the UI thread; everything here measures it, on boards of the size the app
//! is built for.
//!
//! A frame at 120fps is 8.33ms. Run with `--nocapture` to see the real figures, and
//! run it `--release` if the number is going to be quoted anywhere:
//!
//! ```text
//! cargo test --release -p vellum-store --test autosave_cost -- --nocapture
//! ```

use std::time::{Duration, Instant};
use vellum_doc::{Board, ItemId, ItemKind, NewItem, Placement, StyledText};
use vellum_store::{Autosave, AutosaveConfig, BOARD_EXTENSION, BoardDb};

/// What a `record` call must typically cost.
///
/// This is the assertion with teeth. A `record` that had started waiting for a disk,
/// or whose cost had become proportional to the document, would miss it by orders of
/// magnitude and would miss it in the *median*, not just occasionally.
///
/// Two values, because `cargo test` builds without optimisation and an unoptimised
/// Loro is roughly eight times slower than the one that ships. The release figure is
/// the one worth quoting: 50µs is 0.6% of a 120Hz frame.
#[cfg(not(debug_assertions))]
const MEDIAN_BUDGET: Duration = Duration::from_micros(50);
#[cfg(debug_assertions)]
const MEDIAN_BUDGET: Duration = Duration::from_micros(500);

/// Ceiling on the 99th percentile.
///
/// Loose on purpose. At single-digit microseconds per call, the tail of this
/// distribution is the operating system's scheduler, not this crate: a developer
/// machine with a compile running on every core will suspend the measuring thread
/// for tens of milliseconds, and that number says nothing about autosave. It is
/// still asserted, because a stall long enough to drop frames every hundred
/// mutations would be a real defect — it is just not the primary claim.
#[cfg(not(debug_assertions))]
const TAIL_BUDGET: Duration = Duration::from_millis(8);
#[cfg(debug_assertions)]
const TAIL_BUDGET: Duration = Duration::from_millis(40);

const PROFILE: &str = if cfg!(debug_assertions) { "debug" } else { "release" };

/// One frame at 120Hz, for scale in the output.
const FRAME: Duration = Duration::from_micros(8_333);

struct Timings {
    samples: Vec<Duration>,
}

impl Timings {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self { samples }
    }

    fn quantile(&self, fraction: f64) -> Duration {
        let index = ((self.samples.len() as f64 - 1.0) * fraction).round() as usize;
        self.samples[index]
    }

    fn total(&self) -> Duration {
        self.samples.iter().sum()
    }

    /// The claim: typically microseconds, and never stalled long enough to matter.
    fn assert_within_budget(&self) {
        let (p50, p99) = (self.quantile(0.5), self.quantile(0.99));
        assert!(p50 < MEDIAN_BUDGET, "record cost {p50:?} at the median, over {MEDIAN_BUDGET:?}");
        assert!(p99 < TAIL_BUDGET, "record stalled for {p99:?} at p99, over {TAIL_BUDGET:?}");
    }

    fn report(&self, what: &str) {
        let (p50, p99, worst) = (self.quantile(0.5), self.quantile(0.99), self.quantile(1.0));
        println!(
            "[{PROFILE}] {what}: n={} p50={p50:?} p99={p99:?} max={worst:?} \
             total={:?} ({:.3}% of one 120Hz frame at p99)",
            self.samples.len(),
            self.total(),
            p99.as_secs_f64() / FRAME.as_secs_f64() * 100.0,
        );
    }
}

fn sticky(text: &str) -> NewItem {
    NewItem::new(
        ItemKind::Sticky { text: StyledText::plain(text), background: None },
        Placement::new(0.0, 0.0, 199.0, 228.0),
    )
}

/// A board with `items` stickies, matching the shape of an imported Miro board
/// rather than a synthetic one: nested under frames, with real text in them.
fn populated(items: usize) -> (Board, Vec<ItemId>) {
    let mut board = Board::new();
    board.set_title("Reference Board").unwrap();
    let mut ids = Vec::with_capacity(items);
    let mut frame = None;
    for i in 0..items {
        if i % 50 == 0 {
            frame = Some(board.add(sticky(&format!("frame {}", i / 50))).unwrap());
        }
        let mut item = sticky(&format!("cooling system note number {i}"));
        item.parent = frame;
        ids.push(board.add(item).unwrap());
    }
    (board, ids)
}

fn engine(board: &Board) -> (tempfile::TempDir, Autosave) {
    let home = tempfile::tempdir().unwrap();
    let db = BoardDb::open(home.path().join(format!("board.{BOARD_EXTENSION}"))).unwrap();
    // Shipping defaults: the measurement is worthless if it is taken against timings
    // chosen to make it look good.
    let autosave = Autosave::with_config(db, board, AutosaveConfig::default()).unwrap();
    (home, autosave)
}

/// The frame-path case: a drag on a board the size of the reference import.
#[test]
fn recording_a_drag_stays_far_inside_a_frame() {
    let (mut board, ids) = populated(600);
    let (_home, mut autosave) = engine(&board);
    let dragged = ids[300];

    // Warm the writer up so the first sample is not measuring thread start-up.
    for _ in 0..20 {
        board.translate(dragged, 0.5, 0.5).unwrap();
        autosave.record(&board).unwrap();
    }

    let mut samples = Vec::with_capacity(2_000);
    for _ in 0..2_000 {
        board.translate(dragged, 0.5, 0.25).unwrap();
        let started = Instant::now();
        autosave.record(&board).unwrap();
        samples.push(started.elapsed());
    }

    let timings = Timings::of(samples);
    timings.report("record, drag on a 612-item board");
    autosave.flush(&board).unwrap();
    println!("  {} mutations became {} disk writes", 2_020, autosave.stats().writes);

    timings.assert_within_budget();
}

/// The cost must come from the *change*, not from the document, or every board would
/// get slower to edit as it grew — which is the specific way Miro degrades.
#[test]
fn recording_costs_the_same_on_a_small_board_and_a_large_one() {
    let mut medians = Vec::new();
    for size in [100usize, 4_000] {
        let (mut board, ids) = populated(size);
        let (_home, mut autosave) = engine(&board);
        let dragged = ids[size / 2];

        for _ in 0..20 {
            board.translate(dragged, 0.5, 0.5).unwrap();
            autosave.record(&board).unwrap();
        }

        let mut samples = Vec::with_capacity(1_000);
        for _ in 0..1_000 {
            board.translate(dragged, 0.5, 0.25).unwrap();
            let started = Instant::now();
            autosave.record(&board).unwrap();
            samples.push(started.elapsed());
        }
        let timings = Timings::of(samples);
        timings.report(&format!("record, {size}-item board"));
        medians.push(timings.quantile(0.5));
    }

    let (small, large) = (medians[0], medians[1]);
    // Forty times the items. If the cost tracked document size at all, this would be
    // nowhere near a small constant factor.
    assert!(
        large < small * 3 + Duration::from_micros(20),
        "recording cost {large:?} on a 4000-item board against {small:?} on a 100-item one"
    );
}

/// Adding items is the other common mutation, and it is the one that grows the
/// document, so it is measured separately.
#[test]
fn recording_an_insertion_stays_far_inside_a_frame() {
    let (mut board, _) = populated(600);
    let (_home, mut autosave) = engine(&board);

    let mut samples = Vec::with_capacity(1_000);
    for i in 0..1_000 {
        board.add(sticky(&format!("bulk sticky {i}"))).unwrap();
        let started = Instant::now();
        autosave.record(&board).unwrap();
        samples.push(started.elapsed());
    }

    let timings = Timings::of(samples);
    timings.report("record, insertion on a 612-item board");
    timings.assert_within_budget();
}

/// The writer thread is where the milliseconds go. Showing the two side by side is
/// the actual claim: the expensive part is not on the frame path.
#[test]
fn the_writer_absorbs_the_time_the_ui_thread_does_not_spend() {
    let (mut board, ids) = populated(600);
    let (_home, mut autosave) = engine(&board);
    let dragged = ids[10];

    let started = Instant::now();
    for _ in 0..3_000 {
        board.translate(dragged, 0.25, 0.25).unwrap();
        autosave.record(&board).unwrap();
    }
    let on_the_ui_thread = started.elapsed();
    autosave.flush(&board).unwrap();

    let stats = autosave.stats();
    println!(
        "3000 mutations: {on_the_ui_thread:?} on the UI thread, \
         {:?} on the writer across {} transactions",
        stats.write_time, stats.writes
    );
    assert!(stats.is_durable());
    assert!(stats.writes >= 1);
}
