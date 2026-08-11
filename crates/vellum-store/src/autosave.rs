//! Continuous saving that the UI thread never waits for.
//!
//! # The problem
//!
//! Vellum exists because Miro stutters, so nothing on the frame path may touch a
//! disk. But the user also asked that *whatever the application does* be saved —
//! nothing lost, no Cmd+S. Those pull hard against each other: a save is an fsync,
//! an fsync is milliseconds, and milliseconds on the UI thread is a dropped frame.
//!
//! # The shape of the answer
//!
//! The document is a CRDT, so the work splits cleanly:
//!
//! - The UI thread, after every mutation, calls [`Autosave::record`]. That exports
//!   the operations since the last call — hundreds of bytes — and hands them to a
//!   channel. No lock is held, no file is touched, and the cost is measured in
//!   microseconds. `tests/autosave_cost.rs` keeps it there.
//! - A background thread keeps a **shadow document**: a second Loro replica that it
//!   feeds those deltas into. Because it holds a complete, current copy of the board
//!   in its own memory, it can decide entirely on its own clock when to write, and
//!   it can serialise a full snapshot without ever asking the UI thread to stop.
//!
//! The shadow costs one extra copy of the document — a few hundred KB for the
//! reference board — and buys the property that matters: the writer never needs the
//! UI thread for anything, so the UI thread never waits for the writer.
//!
//! # Coalescing, and what "nothing lost" actually promises
//!
//! Dragging a sticky emits a mutation per mouse move: hundreds per second. Writing
//! each one would be absurd, so the writer coalesces on two deadlines at once
//! ([`Coalescer`]):
//!
//! - **quiet period** — write [`AutosaveConfig::coalesce`] after the last change, so
//!   a burst becomes one write once the user's hand stops;
//! - **staleness ceiling** — write no later than [`AutosaveConfig::max_staleness`]
//!   after the *first* unwritten change, so a burst that never stops is still
//!   written continuously.
//!
//! The second is the guarantee: at any instant, at most `max_staleness` of work plus
//! one transaction is at risk, whatever the user is doing. The default is 250ms.
//! `tests/crash_recovery.rs` kills a real process mid-edit and checks it.
//!
//! # Back-pressure
//!
//! If the disk stalls, the delta queue must not grow without bound and must not
//! block the UI. Past [`AutosaveConfig::max_queued_bytes`], [`Autosave::record`]
//! stops sending and leaves its watermark where it is, so the next call exports one
//! larger delta covering everything since. Back-pressure degrades into coarser
//! coalescing — never into a stall, never into unbounded memory.

use crate::blob::Hash;
use crate::board_db::{BoardDb, BoardIndex, RestorePoint};
use crate::error::{Result, StoreError};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use vellum_doc::{Board, Version};

/// How the autosave engine trades write frequency against work at risk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutosaveConfig {
    /// Quiet period after the last change before a write is worth doing. Long
    /// enough that a drag or a burst of typing becomes one write, short enough that
    /// putting the pen down and closing the lid saves.
    pub coalesce: Duration,
    /// The most work that may ever be unwritten, measured from the first change
    /// that has not landed. This is the promise the engine makes.
    pub max_staleness: Duration,
    /// How often the writer takes a restore point while the board is changing.
    /// `None` turns automatic history off.
    pub restore_point_interval: Option<Duration>,
    /// How many automatic restore points to keep. Points the user named are never
    /// pruned.
    pub restore_points_kept: usize,
    /// Ceiling on unwritten deltas held in memory before [`Autosave::record`] starts
    /// deferring instead of queueing. Only reachable when the disk has stopped
    /// answering.
    pub max_queued_bytes: usize,
}

impl Default for AutosaveConfig {
    fn default() -> Self {
        Self {
            coalesce: Duration::from_millis(60),
            max_staleness: Duration::from_millis(250),
            restore_point_interval: Some(Duration::from_secs(300)),
            restore_points_kept: 20,
            max_queued_bytes: 8 * 1024 * 1024,
        }
    }
}

/// What the engine has done, for a status line or a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AutosaveStats {
    /// Deltas handed to the writer.
    pub deltas_sent: u64,
    /// Times [`Autosave::record`] declined to send because the queue was full. A
    /// non-zero count means the disk could not keep up; the changes are not lost,
    /// they are folded into the next delta.
    pub deltas_deferred: u64,
    /// Committed transactions. Compare against `deltas_sent` to see coalescing work.
    pub writes: u64,
    /// Restore points taken automatically.
    pub restore_points: u64,
    /// Deltas queued or being written, not yet committed.
    pub unwritten_deltas: u64,
    pub queued_bytes: usize,
    /// Wall time the writer thread has spent inside transactions. It is off the
    /// frame path by construction; this is how you show that.
    pub write_time: Duration,
}

impl AutosaveStats {
    /// Whether everything handed to the engine is on disk.
    pub fn is_durable(&self) -> bool {
        self.unwritten_deltas == 0
    }
}

/// A running autosave engine: a handle on the UI side, a writer thread behind it.
///
/// Dropping it stops the writer, but only after everything already recorded has been
/// written — which is why the board is safe if the app exits without ceremony.
pub struct Autosave {
    sender: mpsc::Sender<Message>,
    writer: Option<JoinHandle<BoardDb>>,
    /// The document version the writer has been told about. Deliberately not the
    /// board's own version: when back-pressure makes us skip a send, this stays put
    /// so the next export covers both changes.
    recorded: Version,
    sent: u64,
    /// Whether a change was held back by back-pressure and has not gone out since.
    /// The writer cannot know about it — it is still sitting in the caller's document
    /// — so durability has to be reported from here.
    deferred: bool,
    shared: Arc<Shared>,
    max_queued_bytes: usize,
}

impl Autosave {
    /// Starts saving `board` into `db`, with the default timings.
    pub fn start(db: BoardDb, board: &Board) -> Result<Self> {
        Self::with_config(db, board, AutosaveConfig::default())
    }

    /// Starts saving `board` into `db`.
    ///
    /// Takes ownership of the database: the writer thread is the only thing allowed
    /// to touch it, which is what makes "saving never blocks the UI" structural
    /// rather than a convention. Everything a caller still needs from it —
    /// [`Autosave::index`], [`Autosave::set_thumbnail`], version history — is
    /// forwarded, and [`Autosave::stop`] gives it back.
    ///
    /// Does no I/O itself. The board's current state is queued like any other change
    /// and lands within `max_staleness`.
    pub fn with_config(db: BoardDb, board: &Board, config: AutosaveConfig) -> Result<Self> {
        let snapshot = board.to_bytes()?;
        let shadow = Board::from_bytes(&snapshot)?;
        let recorded = board.version();

        let shared = Arc::new(Shared::default());
        let (sender, receiver) = mpsc::channel();
        let max_queued_bytes = config.max_queued_bytes;

        let writer = Writer {
            db,
            shadow,
            coalescer: Coalescer::new(config.coalesce, config.max_staleness),
            shared: Arc::clone(&shared),
            restore_point_interval: config.restore_point_interval,
            restore_points_kept: config.restore_points_kept,
            last_restore_point: Instant::now(),
            applied: 0,
            captured: 0,
            consumed: 0,
        };
        // The base state is treated as a change so that a board created and then left
        // alone still reaches the disk.
        let writer = writer.dirtied();

        let handle = thread::Builder::new()
            .name("vellum-autosave".to_owned())
            .spawn(move || writer.run(&receiver))?;

        Ok(Self {
            sender,
            writer: Some(handle),
            recorded,
            sent: 0,
            deferred: false,
            shared,
            max_queued_bytes,
        })
    }

    /// Notes that the board changed. Call after every mutation.
    ///
    /// Cheap enough to sit on the frame path: one version comparison, one delta
    /// export, one channel send. It never blocks, never allocates more than the
    /// delta, and never touches a file. Calling it when nothing changed is free.
    ///
    /// The error it returns is the *writer's* — a disk that filled up, a file that
    /// went away. It surfaces here because this is the call the UI already makes on
    /// every edit, and a save that stopped working must not be discovered at quit.
    pub fn record(&mut self, board: &Board) -> Result<()> {
        self.shared.take_failure()?;
        self.send(board, true)
    }

    /// Blocks until every change made to `board` is committed to disk.
    ///
    /// For the handful of moments where waiting is the right answer: quitting,
    /// exporting, copying the file, handing the board to another tool. Everything
    /// else should call [`Autosave::record`] and carry on.
    pub fn flush(&mut self, board: &Board) -> Result<()> {
        self.send(board, false)?;
        self.ask(Writer::write_now)
    }

    /// What the engine has done so far. Reads atomics; safe to call every frame.
    pub fn stats(&self) -> AutosaveStats {
        let sent = self.shared.sent.load(Ordering::Acquire);
        let written = self.shared.written_seq.load(Ordering::Acquire);
        AutosaveStats {
            deltas_sent: sent,
            deltas_deferred: self.shared.deferred.load(Ordering::Relaxed),
            writes: self.shared.writes.load(Ordering::Relaxed),
            restore_points: self.shared.restore_points.load(Ordering::Relaxed),
            // A deferred change never became a delta, so it has to be counted here
            // or `is_durable` would call a board safe while the caller still holds
            // an edit the writer has never seen.
            unwritten_deltas: sent.saturating_sub(written) + u64::from(self.deferred),
            queued_bytes: self.shared.queued_bytes.load(Ordering::Relaxed),
            write_time: Duration::from_nanos(self.shared.write_nanos.load(Ordering::Relaxed)),
        }
    }

    /// The board-library row, read on the writer thread.
    pub fn index(&self) -> Result<Option<BoardIndex>> {
        self.ask(|writer| writer.db.index())
    }

    /// Points the library entry at a rendered preview. See [`BoardDb::set_thumbnail`].
    pub fn set_thumbnail(&self, thumbnail: Option<&Hash>) -> Result<()> {
        let thumbnail = thumbnail.copied();
        self.ask(move |writer| writer.db.set_thumbnail(thumbnail.as_ref()))
    }

    /// Collapses the stored chunk chain into one snapshot, off the UI thread.
    ///
    /// Everything recorded so far is written first, so the compacted file is current.
    pub fn compact(&self) -> Result<()> {
        self.ask(|writer| {
            writer.write_now()?;
            writer.db.compact(&writer.shadow)
        })
    }

    // ----- version history --------------------------------------------------

    /// Names the board's current state so the user can come back to it.
    ///
    /// Records `board` first, so the point captures the edit that prompted it rather
    /// than whatever the writer happened to have.
    pub fn create_restore_point(&mut self, board: &Board, label: &str) -> Result<i64> {
        self.send(board, false)?;
        let label = label.to_owned();
        self.ask(move |writer| {
            writer.write_now()?;
            writer.db.create_restore_point(&writer.shadow, &label)
        })
    }

    /// Every restore point, newest first. See [`BoardDb::restore_points`].
    pub fn restore_points(&self) -> Result<Vec<RestorePoint>> {
        self.ask(|writer| writer.db.restore_points())
    }

    /// Reads a restore point's document without changing the board.
    pub fn read_restore_point(&self, id: i64) -> Result<Option<Board>> {
        let snapshot = self.ask(move |writer| {
            Ok(writer.db.read_restore_point(id)?.map(|board| board.to_bytes()).transpose()?)
        })?;
        snapshot.as_deref().map(Board::from_bytes).transpose().map_err(Into::into)
    }

    /// Makes a restore point the board's current state.
    ///
    /// The caller **must** replace the board it is holding with the returned one:
    /// the engine's shadow, the file, and the returned document are all the restored
    /// state, and the old handle is none of them. Pending changes are written and
    /// captured as a safety point first, so this is reversible — see
    /// [`BoardDb::restore`].
    pub fn restore(&mut self, id: i64) -> Result<Board> {
        let snapshot = self.ask(move |writer| {
            writer.write_now()?;
            let restored = writer.db.restore(id)?;
            let bytes = restored.to_bytes()?;
            writer.adopt(restored);
            Ok(bytes)
        })?;

        let board = Board::from_bytes(&snapshot)?;
        self.recorded = board.version();
        self.deferred = false;
        Ok(board)
    }

    // ----- shutdown ---------------------------------------------------------

    /// Writes everything outstanding, stops the writer and hands the database back.
    ///
    /// The orderly way out. Dropping the handle does the same thing, minus the
    /// chance to see an error.
    ///
    /// An `Err` still means the writer stopped and the file was closed properly —
    /// it reports what went wrong on the way, not that anything was left open.
    pub fn stop(mut self) -> Result<BoardDb> {
        self.halt()
    }

    fn halt(&mut self) -> Result<BoardDb> {
        let Some(writer) = self.writer.take() else {
            return Err(StoreError::AutosaveStopped);
        };
        // A disconnected channel is a writer that already stopped; the join below is
        // what reports why.
        let _ = self.sender.send(Message::Stop);
        let db = writer.join().map_err(|_| StoreError::AutosaveStopped)?;
        self.shared.take_failure()?;
        Ok(db)
    }

    /// Exports and queues everything since the last delta the writer was told about.
    ///
    /// `yield_to_backpressure` is the difference between the frame path, which must
    /// never grow the queue without bound, and the deliberate waits, which must get
    /// the data across whatever the queue looks like.
    fn send(&mut self, board: &Board, yield_to_backpressure: bool) -> Result<()> {
        let version = board.version();
        if version == self.recorded {
            return Ok(());
        }
        if yield_to_backpressure
            && self.shared.queued_bytes.load(Ordering::Relaxed) >= self.max_queued_bytes
        {
            // The watermark stays put, so nothing is lost: the next send exports
            // this change together with whatever comes after it.
            self.shared.deferred.fetch_add(1, Ordering::Relaxed);
            self.deferred = true;
            return Ok(());
        }

        let delta = board.export_since(&self.recorded)?;
        self.sent += 1;
        self.shared.queued_bytes.fetch_add(delta.len(), Ordering::Relaxed);
        self.shared.sent.store(self.sent, Ordering::Release);
        self.recorded = version;
        self.deferred = false;

        self.sender
            .send(Message::Delta { seq: self.sent, bytes: delta })
            .map_err(|_| StoreError::AutosaveStopped)
    }

    /// Runs `task` on the writer thread and waits for its answer.
    fn ask<T: Send + 'static>(
        &self,
        task: impl FnOnce(&mut Writer) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (reply, answer) = mpsc::channel();
        self.sender
            .send(Message::Task(Box::new(move |writer| {
                let _ = reply.send(task(writer));
            })))
            .map_err(|_| StoreError::AutosaveStopped)?;
        answer.recv().map_err(|_| StoreError::AutosaveStopped)?
    }
}

/// Stopping on drop is what makes "the app was closed" and "the app was quit" the
/// same thing for the file. The join can wait for one transaction; that is the price
/// of not losing the last edit, and it is paid at exit, not on the frame path.
impl Drop for Autosave {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = self.halt();
        }
    }
}

impl std::fmt::Debug for Autosave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Autosave").field("stats", &self.stats()).finish_non_exhaustive()
    }
}

/// When a burst of changes should become a write.
///
/// Two deadlines, and the earlier one wins: settle quickly when the user pauses,
/// but never let the oldest unwritten change get older than the ceiling. Split out
/// from the writer because timing policy is the part worth testing without threads,
/// clocks or files in the way.
#[derive(Debug)]
struct Coalescer {
    coalesce: Duration,
    max_staleness: Duration,
    oldest: Option<Instant>,
    newest: Option<Instant>,
}

impl Coalescer {
    fn new(coalesce: Duration, max_staleness: Duration) -> Self {
        Self { coalesce, max_staleness, oldest: None, newest: None }
    }

    fn note(&mut self, at: Instant) {
        self.oldest.get_or_insert(at);
        self.newest = Some(at);
    }

    /// When the pending changes must be written by, or `None` when there are none.
    fn deadline(&self) -> Option<Instant> {
        let oldest = self.oldest?;
        let newest = self.newest?;
        Some((newest + self.coalesce).min(oldest + self.max_staleness))
    }

    fn is_due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    fn is_dirty(&self) -> bool {
        self.oldest.is_some()
    }

    fn cleared(&mut self) {
        self.oldest = None;
        self.newest = None;
    }

    /// Treats everything pending as if it had just arrived, which turns a failed
    /// write into a retry one `coalesce` later instead of a spin.
    fn restarted(&mut self, at: Instant) {
        self.oldest = Some(at);
        self.newest = Some(at);
    }
}

#[derive(Debug, Default)]
struct Shared {
    sent: AtomicU64,
    deferred: AtomicU64,
    writes: AtomicU64,
    restore_points: AtomicU64,
    written_seq: AtomicU64,
    queued_bytes: AtomicUsize,
    write_nanos: AtomicU64,
    /// Guards the mutex below so that the frame path, where a failure has never
    /// happened, never takes a lock the writer thread could be holding.
    failed: AtomicBool,
    failure: Mutex<Option<StoreError>>,
}

impl Shared {
    /// Records a failure for the UI to collect. First one wins: it is the one that
    /// explains the rest.
    fn fail(&self, error: StoreError) {
        if let Ok(mut failure) = self.failure.lock() {
            failure.get_or_insert(error);
            self.failed.store(true, Ordering::Release);
        }
    }

    /// Surfaces and clears whatever last failed, so an error is reported exactly
    /// once and cannot be silently swallowed.
    fn take_failure(&self) -> Result<()> {
        if !self.failed.load(Ordering::Acquire) {
            return Ok(());
        }
        match self.failure.lock() {
            Ok(mut failure) => {
                self.failed.store(false, Ordering::Release);
                match failure.take() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }
            // The writer panicked while holding the slot. It is gone either way.
            Err(_) => Err(StoreError::AutosaveStopped),
        }
    }
}

enum Message {
    Delta { seq: u64, bytes: Vec<u8> },
    /// Work that needs the database, run where the database lives.
    Task(Box<dyn FnOnce(&mut Writer) + Send>),
    Stop,
}

/// The background half: owns the database and a replica of the document.
struct Writer {
    db: BoardDb,
    /// A second Loro replica, fed the UI thread's deltas. It exists so that deciding
    /// *when* to write, encoding an incremental update and encoding a whole snapshot
    /// can all happen here, with no reference to the document the user is editing.
    shadow: Board,
    coalescer: Coalescer,
    shared: Arc<Shared>,
    restore_point_interval: Option<Duration>,
    restore_points_kept: usize,
    last_restore_point: Instant,
    /// Deltas folded into the shadow, and the count at the last restore point.
    /// Counting is enough to know whether history has moved on, and costs nothing
    /// next to comparing version vectors on every loop iteration.
    applied: u64,
    captured: u64,
    /// Sequence number of the newest delta in the shadow, published as durable once
    /// it is committed.
    consumed: u64,
}

impl Writer {
    fn dirtied(mut self) -> Self {
        self.coalescer.note(Instant::now());
        self
    }

    fn run(mut self, receiver: &mpsc::Receiver<Message>) -> BoardDb {
        loop {
            let message = match self.deadline() {
                Some(deadline) => {
                    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(message) => Some(message),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                // Nothing pending and nothing scheduled: sleep until there is.
                None => match receiver.recv() {
                    Ok(message) => Some(message),
                    Err(_) => break,
                },
            };

            match message {
                Some(Message::Delta { seq, bytes }) => self.absorb(seq, bytes),
                Some(Message::Task(task)) => task(&mut self),
                Some(Message::Stop) => break,
                None => {}
            }

            // Checked after every message as well as on every timeout: during a
            // sustained burst the channel always has something ready, so the timeout
            // branch would never be reached and the staleness ceiling would be a
            // deadline nothing ever enforced.
            let now = Instant::now();
            if self.coalescer.is_due(now) {
                self.write_if_due();
            }
            if self.restore_point_due(now) {
                self.take_restore_point();
            }
        }

        // Whatever is still pending goes out before the database is handed back:
        // this is the path a quitting app takes.
        self.write_if_due();
        self.db
    }

    fn absorb(&mut self, seq: u64, bytes: Vec<u8>) {
        self.shared.queued_bytes.fetch_sub(bytes.len(), Ordering::Relaxed);
        match self.shadow.apply(&bytes) {
            Ok(()) => {
                self.applied += 1;
                self.consumed = seq;
                self.coalescer.note(Instant::now());
            }
            // The shadow rejecting a delta means the two replicas have diverged, so
            // continuing would write a document that is not what the user sees.
            Err(error) => self.shared.fail(error.into()),
        }
    }

    /// The next moment this thread has something to do.
    fn deadline(&self) -> Option<Instant> {
        let write = self.coalescer.deadline();
        let history = self.restore_point_deadline();
        match (write, history) {
            (Some(write), Some(history)) => Some(write.min(history)),
            (write, history) => write.or(history),
        }
    }

    fn restore_point_deadline(&self) -> Option<Instant> {
        let interval = self.restore_point_interval?;
        (self.applied != self.captured).then(|| self.last_restore_point + interval)
    }

    fn restore_point_due(&self, now: Instant) -> bool {
        self.restore_point_deadline().is_some_and(|deadline| now >= deadline)
    }

    /// Writes if anything is pending. Errors are recorded rather than returned: this
    /// is the timer path, and there is no caller waiting to be told.
    fn write_if_due(&mut self) {
        if !self.coalescer.is_dirty() {
            return;
        }
        if let Err(error) = self.write_now() {
            self.shared.fail(error);
        }
    }

    /// Commits the shadow's current state, whether or not a deadline has passed.
    fn write_now(&mut self) -> Result<()> {
        let started = Instant::now();
        match self.db.save(&self.shadow) {
            Ok(wrote) => {
                self.coalescer.cleared();
                self.shared.written_seq.store(self.consumed, Ordering::Release);
                if wrote {
                    self.shared.writes.fetch_add(1, Ordering::Relaxed);
                    self.shared.write_nanos.fetch_add(
                        started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                        Ordering::Relaxed,
                    );
                }
                Ok(())
            }
            Err(error) => {
                // Keep the changes pending and try again after one quiet period, so
                // a disk that comes back finds the work still waiting for it.
                self.coalescer.restarted(Instant::now());
                Err(error)
            }
        }
    }

    fn take_restore_point(&mut self) {
        // The deadline moves first and unconditionally. A failure that left it in the
        // past would make `restore_point_due` true forever, and this loop would spin a
        // core at 100% for as long as the app was open.
        self.last_restore_point = Instant::now();
        self.captured = self.applied;

        let outcome = self
            .db
            .create_automatic_restore_point(&self.shadow)
            .and_then(|_| self.db.prune_restore_points(self.restore_points_kept));
        match outcome {
            Ok(_) => {
                self.shared.restore_points.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => self.shared.fail(error),
        }
    }

    /// Replaces the replica after the file was rewritten under it.
    ///
    /// Everything the caller had sent is already folded in — this runs from a task,
    /// and tasks queue behind deltas — so the restored state is durable by
    /// definition and the watermarks move up to meet it rather than backwards.
    fn adopt(&mut self, board: Board) {
        self.shadow = board;
        self.coalescer.cleared();
        self.captured = self.applied;
        self.last_restore_point = Instant::now();
        self.consumed = self.shared.sent.load(Ordering::Acquire).max(self.consumed);
        self.shared.written_seq.store(self.consumed, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn coalescer() -> (Instant, Coalescer) {
        (Instant::now(), Coalescer::new(Duration::from_millis(60), Duration::from_millis(250)))
    }

    #[test]
    fn a_clean_coalescer_has_nothing_to_do() {
        let (base, coalescer) = coalescer();
        assert!(!coalescer.is_dirty());
        assert_eq!(coalescer.deadline(), None);
        assert!(!coalescer.is_due(at(base, 10_000)));
    }

    #[test]
    fn a_single_change_is_written_after_the_quiet_period() {
        let (base, mut coalescer) = coalescer();
        coalescer.note(base);

        assert_eq!(coalescer.deadline(), Some(at(base, 60)));
        assert!(!coalescer.is_due(at(base, 59)));
        assert!(coalescer.is_due(at(base, 60)));
    }

    /// The behaviour a drag depends on: hundreds of changes, one deadline, and it
    /// keeps moving as long as they keep coming.
    #[test]
    fn changes_arriving_together_push_the_quiet_deadline_back() {
        let (base, mut coalescer) = coalescer();
        for millis in (0..100).step_by(5) {
            coalescer.note(at(base, millis));
        }

        assert!(!coalescer.is_due(at(base, 150)));
        assert_eq!(coalescer.deadline(), Some(at(base, 95 + 60)));
    }

    /// The promise. A burst that never settles still gets written, on time.
    #[test]
    fn the_staleness_ceiling_overrides_a_burst_that_never_settles() {
        let (base, mut coalescer) = coalescer();
        for millis in 0..400 {
            coalescer.note(at(base, millis));
        }

        assert_eq!(coalescer.deadline(), Some(at(base, 250)));
        assert!(coalescer.is_due(at(base, 250)));
    }

    #[test]
    fn the_ceiling_is_measured_from_the_oldest_unwritten_change() {
        let (base, mut coalescer) = coalescer();
        coalescer.note(base);
        coalescer.note(at(base, 200));
        assert_eq!(coalescer.deadline(), Some(at(base, 250)), "the ceiling moved with the burst");

        coalescer.cleared();
        coalescer.note(at(base, 300));
        assert_eq!(coalescer.deadline(), Some(at(base, 360)));
    }

    #[test]
    fn writing_clears_the_deadline_until_something_changes_again() {
        let (base, mut coalescer) = coalescer();
        coalescer.note(base);
        coalescer.cleared();

        assert!(!coalescer.is_dirty());
        assert_eq!(coalescer.deadline(), None);

        coalescer.note(at(base, 500));
        assert_eq!(coalescer.deadline(), Some(at(base, 560)));
    }

    /// A failed write must retry, but at the coalescing rate — not as fast as the
    /// loop can spin.
    #[test]
    fn a_restart_defers_the_retry_by_one_quiet_period() {
        let (base, mut coalescer) = coalescer();
        coalescer.note(base);
        assert!(coalescer.is_due(at(base, 300)));

        coalescer.restarted(at(base, 300));

        assert!(coalescer.is_dirty());
        assert!(!coalescer.is_due(at(base, 359)));
        assert!(coalescer.is_due(at(base, 360)));
    }

    /// A ceiling below the quiet period is a legitimate ask — "write essentially
    /// immediately" — and must not invert the two deadlines.
    #[test]
    fn a_ceiling_shorter_than_the_quiet_period_still_wins() {
        let base = Instant::now();
        let mut coalescer = Coalescer::new(Duration::from_millis(60), Duration::from_millis(10));
        coalescer.note(base);

        assert_eq!(coalescer.deadline(), Some(at(base, 10)));
    }

    /// A save that stopped working must be told to someone. The frame path takes
    /// this route on every mutation, so it also has to be reported exactly once
    /// rather than latching and drowning the log.
    #[test]
    fn a_writer_failure_is_reported_once_and_then_cleared() {
        let shared = Shared::default();
        assert!(shared.take_failure().is_ok());

        shared.fail(StoreError::MissingSnapshot);
        // The first failure is the one that explains the others.
        shared.fail(StoreError::NoSuchRestorePoint(7));

        match shared.take_failure() {
            Err(StoreError::MissingSnapshot) => {}
            other => panic!("expected the first failure, got {other:?}"),
        }
        assert!(shared.take_failure().is_ok(), "the same failure was reported twice");

        shared.fail(StoreError::AutosaveStopped);
        assert!(matches!(shared.take_failure(), Err(StoreError::AutosaveStopped)));
    }

    /// The engine is created wherever the document lives and may be moved with it;
    /// the database has to reach the writer thread at all. Both are structural, so
    /// they are asserted at compile time rather than left to the next refactor.
    #[test]
    fn the_handle_and_the_database_can_move_between_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<Autosave>();
        assert_send::<BoardDb>();
    }

    #[test]
    fn stats_report_nothing_before_anything_is_recorded() {
        let stats = AutosaveStats::default();
        assert!(stats.is_durable());
        assert_eq!(stats.write_time, Duration::ZERO);
    }
}
