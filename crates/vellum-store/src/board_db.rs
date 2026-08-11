//! One SQLite database per board.
//!
//! # Why a database per board rather than one library database
//!
//! A board is the unit a user copies, renames, backs up, drops in Dropbox and one
//! day shares. Keeping each in its own file makes all of those a filesystem
//! operation instead of an export. It also bounds the blast radius: a corrupt board
//! costs one board, not the library.
//!
//! # Why SQLite rather than a plain file
//!
//! Saving must be *incremental* and *crash-safe*, and those pull in opposite
//! directions for a flat file. Rewriting a whole snapshot on every save is
//! O(document) per keystroke-debounce; appending to a log without a transaction
//! risks a torn tail. SQLite in WAL mode gives an atomic multi-row commit for free,
//! so a save appends the changes since the last one and either lands completely or
//! not at all.
//!
//! The document is therefore stored as a chunk chain: one snapshot followed by the
//! Loro update blobs recorded since. Loading replays them; once the chain grows past
//! [`MAX_UPDATE_CHUNKS`], the next save collapses it back into a single snapshot so
//! neither load time nor file size drifts upwards.
//!
//! # Why the index table exists
//!
//! The board library shows a title, a thumbnail and an item count for every board.
//! Deriving those from the documents would mean decoding every board's CRDT at
//! startup. [`BoardDb::index`] reads one small row and never touches the chunk
//! table, so listing a hundred boards costs a hundred tiny queries.
//!
//! # Crash recovery
//!
//! Atomic commits mean the file is never *torn*, but they say nothing about whether
//! the process that was writing it got to finish. A session that writes marks the
//! file dirty inside its first save transaction and clears the mark when the handle
//! closes; a mark still set at open time is proof that the previous session was
//! killed. [`BoardDb::recovery`] reports that, [`BoardDb::recover`] repairs whatever
//! it can, and `tests/crash_recovery.rs` proves both against a real `SIGKILL`.

use crate::blob::Hash;
use crate::error::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use vellum_doc::{Board, Version};

/// Layout version of the SQLite schema, kept in SQLite's own `user_version`.
///
/// v2 added the `session` table (crash detection) and the `restore_point` table
/// (version history). Both are created on open, so a v1 file is migrated by being
/// opened and nothing in it is rewritten.
pub const STORE_SCHEMA_VERSION: i64 = 2;

/// Filename extension for a board database.
pub const BOARD_EXTENSION: &str = "vellum";

/// How many incremental saves accumulate before the chain is collapsed.
///
/// The trade is load time and file size against save cost. Thirty-two keeps a
/// reopened board's replay work trivial while still meaning that a long editing
/// session writes deltas, not snapshots, almost all of the time.
pub const MAX_UPDATE_CHUNKS: i64 = 32;

/// Label given to the restore point [`BoardDb::restore`] takes of the state it is
/// about to replace, so that restoring is itself reversible.
pub const REPLACED_BY_RESTORE: &str = "before restore";

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS doc_chunk (
        seq         INTEGER PRIMARY KEY AUTOINCREMENT,
        is_snapshot INTEGER NOT NULL,
        bytes       BLOB    NOT NULL
    ) STRICT;

    CREATE TABLE IF NOT EXISTS board_index (
        id          INTEGER PRIMARY KEY CHECK (id = 1),
        title       TEXT    NOT NULL,
        item_count  INTEGER NOT NULL,
        thumbnail   TEXT,
        modified_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE IF NOT EXISTS session (
        id        INTEGER PRIMARY KEY CHECK (id = 1),
        dirty     INTEGER NOT NULL,
        opened_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE IF NOT EXISTS restore_point (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        created_ms   INTEGER NOT NULL,
        label        TEXT,
        automatic    INTEGER NOT NULL,
        title        TEXT    NOT NULL,
        item_count   INTEGER NOT NULL,
        content_hash BLOB    NOT NULL,
        snapshot     BLOB    NOT NULL
    ) STRICT;
";

/// Everything the board library needs about a board without opening its document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardIndex {
    /// The database file this describes. Empty until filled in by
    /// [`list_boards`], which is the only caller that knows about more than one.
    pub path: PathBuf,
    pub title: String,
    pub item_count: u64,
    /// Content hash of a rendered preview in the shared blob store.
    pub thumbnail: Option<Hash>,
    pub modified: SystemTime,
}

/// What opening a board file said about how the previous session ended.
///
/// Read it before doing anything else with a freshly opened board: an unclean
/// shutdown is the one case where the file on disk may be older than what the user
/// last saw, and it is the moment to say so rather than after they notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Recovery {
    /// A previous session wrote to this board and never closed it: the process was
    /// killed, or the machine lost power. Authoritative — the flag is set inside the
    /// same transaction as the first save and cleared only by an orderly close.
    pub unclean_shutdown: bool,
    /// A non-empty write-ahead log was sitting next to the board file. SQLite
    /// removes it when the last connection closes, so finding one corroborates
    /// `unclean_shutdown` — but on its own it can also mean another process has the
    /// board open right now, so it is reported rather than trusted.
    pub log_left_behind: bool,
}

impl Recovery {
    /// Whether the file was closed properly last time.
    pub fn is_clean(&self) -> bool {
        !self.unclean_shutdown
    }
}

/// The outcome of [`BoardDb::recover`].
#[derive(Debug)]
pub struct Recovered {
    pub board: Board,
    /// Stored chunks that could not be replayed and were dropped. Zero is the
    /// expected answer even after a crash: SQLite's commits are atomic, so a torn
    /// chunk should be impossible. A non-zero count means something below SQLite —
    /// the disk, the filesystem, a copy tool — damaged the file.
    pub discarded_chunks: u64,
    /// Set when the chunk chain was unusable and the board came from version
    /// history instead. Everything after that restore point is gone.
    pub from_restore_point: Option<i64>,
}

/// One entry in a board's version history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePoint {
    pub id: i64,
    pub created: SystemTime,
    /// What the user called it, or a note from the engine. `None` for the routine
    /// points autosave takes on a timer.
    pub label: Option<String>,
    /// Whether [`BoardDb::prune_restore_points`] is allowed to delete this one. A
    /// point the user named is kept until they say otherwise.
    pub automatic: bool,
    pub title: String,
    pub item_count: u64,
    /// Size of the stored snapshot, so a UI can show what keeping history costs.
    pub snapshot_bytes: u64,
}

/// A board's on-disk database.
pub struct BoardDb {
    conn: Connection,
    path: PathBuf,
    /// The document version already on disk. `None` means this handle has neither
    /// loaded nor saved yet, so it cannot know what the file contains and the next
    /// save must be a full snapshot.
    stored: Option<Version>,
    recovery: Recovery,
    /// Whether this handle has written, and therefore owns the dirty mark. Handles
    /// that only read — the board library opens every file — must not clear a mark
    /// left by a session that crashed, or the evidence would evaporate before the
    /// user ever opened the board.
    wrote: bool,
}

impl BoardDb {
    /// Opens a board database, creating and initialising the file if it is not there.
    ///
    /// Opening never writes, so listing a library of a hundred boards cannot disturb
    /// any of them, and a crash mark survives until an editing session clears it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        // Sampled before the connection exists: opening in WAL mode creates the log
        // itself, which would make every board look like it had crashed.
        let log_left_behind = write_ahead_log_present(&path);
        let conn = Connection::open(&path)?;

        // WAL so a save is one append plus one fsync rather than a rewrite of the
        // journal; `synchronous = FULL` so a committed save survives power loss
        // rather than merely a process crash. Boards save on a debounce, so paying
        // an fsync per save is invisible; losing the last minute of work is not.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = FULL;
             PRAGMA foreign_keys = ON;",
        )?;

        let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found > STORE_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                found,
                supported: STORE_SCHEMA_VERSION,
            });
        }
        conn.execute_batch(SCHEMA)?;
        if found < STORE_SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
        }

        let unclean_shutdown = conn
            .query_row("SELECT dirty FROM session WHERE id = 1", [], |row| row.get::<_, i64>(0))
            .optional()?
            .is_some_and(|dirty| dirty != 0);

        Ok(Self {
            conn,
            path,
            stored: None,
            recovery: Recovery { unclean_shutdown, log_left_behind },
            wrote: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What opening the file said about the previous session. See [`Recovery`].
    pub fn recovery(&self) -> Recovery {
        self.recovery
    }

    /// Reads the board back, replaying the snapshot and every update after it.
    ///
    /// `Ok(None)` means the file exists but nothing has been saved into it yet —
    /// the normal state of a freshly created board file.
    ///
    /// Strict: a chunk that will not replay fails the whole load, because silently
    /// opening a board that is missing its last hour of work is worse than not
    /// opening it. [`BoardDb::recover`] is the deliberate, reporting alternative.
    pub fn load(&mut self) -> Result<Option<Board>> {
        let mut statement =
            self.conn.prepare("SELECT is_snapshot, bytes FROM doc_chunk ORDER BY seq")?;
        let mut chunks = statement.query([])?;

        let Some(first) = chunks.next()? else {
            return Ok(None);
        };
        if first.get::<_, i64>(0)? != 1 {
            return Err(StoreError::MissingSnapshot);
        }
        let mut board = Board::from_bytes(&first.get::<_, Vec<u8>>(1)?)?;

        while let Some(chunk) = chunks.next()? {
            board.apply(&chunk.get::<_, Vec<u8>>(1)?)?;
        }

        // Everything in the file is now in the document, so the next save only has
        // to write what happens from here.
        self.stored = Some(board.version());
        Ok(Some(board))
    }

    /// Opens the board as far as the file allows, and says how far that was.
    ///
    /// The recovery path, for use after [`BoardDb::recovery`] reports an unclean
    /// shutdown — or any time [`BoardDb::load`] refuses. It replays the longest
    /// prefix of the chunk chain that applies cleanly, falls back to the newest
    /// usable restore point if even the snapshot is unreadable, and rewrites the
    /// file as a single snapshot of whatever it salvaged so the damaged tail cannot
    /// be replayed again tomorrow.
    ///
    /// `Ok(None)` means the file is empty and undamaged: there was nothing to
    /// recover because nothing was ever saved.
    ///
    /// Recovering also **acknowledges** the crash: the caller has now been told, so
    /// the mark is cleared and the next open reports a clean file. What this handle
    /// reports through [`BoardDb::recovery`] does not change.
    pub fn recover(&mut self) -> Result<Option<Recovered>> {
        let outcome = self.replay_and_repair();
        if outcome.is_ok() && self.recovery.unclean_shutdown {
            self.acknowledge_crash()?;
        }
        outcome
    }

    fn replay_and_repair(&mut self) -> Result<Option<Recovered>> {
        let total = self.chunk_count()?;
        let (recovered, replayed) = self.replay_longest_prefix()?;

        let Some(board) = recovered else {
            // The snapshot itself is unusable, so the deltas after it are worthless
            // too. Version history is the only remaining ladder down.
            return match self.newest_usable_restore_point()? {
                Some((id, board)) => {
                    self.overwrite_with_snapshot(&board)?;
                    Ok(Some(Recovered {
                        board,
                        discarded_chunks: total,
                        from_restore_point: Some(id),
                    }))
                }
                None if total == 0 => Ok(None),
                None => Err(StoreError::MissingSnapshot),
            };
        };

        // Saturating rather than plain: an underflow panic would be a crash inside
        // the code whose whole job is surviving one.
        let discarded = total.saturating_sub(replayed);
        if discarded > 0 {
            self.overwrite_with_snapshot(&board)?;
        } else {
            self.stored = Some(board.version());
        }
        Ok(Some(Recovered { board, discarded_chunks: discarded, from_restore_point: None }))
    }

    /// Writes everything that changed since this handle last saw the file.
    ///
    /// Cheap in the common case: a snapshot is written only on the first save
    /// through this handle, or when the update chain has grown past
    /// [`MAX_UPDATE_CHUNKS`]. Saving an unchanged board does nothing at all, which
    /// is what the returned flag reports — autosave uses it to avoid counting a
    /// no-op as a disk write.
    pub fn save(&mut self, board: &Board) -> Result<bool> {
        self.write(board, false)
    }

    /// Forces the chunk chain to collapse into a single snapshot.
    ///
    /// Saving already does this on its own schedule; this is for the moments where
    /// a compact file matters more than a fast write — before copying or sharing a
    /// board, or after a bulk import. Pair it with [`BoardDb::vacuum`] to give the
    /// freed pages back to the filesystem.
    pub fn compact(&mut self, board: &Board) -> Result<()> {
        self.write(board, true).map(|_| ())
    }

    /// Returns the file's unused pages to the filesystem.
    ///
    /// Separate from [`BoardDb::compact`] because it rewrites the entire database:
    /// worth doing after deleting restore points, never worth doing on a timer.
    pub fn vacuum(&mut self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Asks SQLite whether the file's own structure is sound.
    ///
    /// This checks the database, not the documents inside it; a board can pass this
    /// and still hold a Loro chunk that will not replay, which is what
    /// [`BoardDb::recover`] is for.
    pub fn check_integrity(&self) -> Result<bool> {
        let verdict: String = self.conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
        Ok(verdict.eq_ignore_ascii_case("ok"))
    }

    fn write(&mut self, board: &Board, force_snapshot: bool) -> Result<bool> {
        let version = board.version();
        // Comparing versions, not export sizes: an export with nothing new in it is
        // still a non-empty Loro header.
        if !force_snapshot && self.stored.as_ref() == Some(&version) {
            return Ok(false);
        }

        // A snapshot is unavoidable when this handle does not know what is already on
        // disk, and is worth taking once the chain has grown long enough that replaying
        // it would cost more than rewriting it. Decided before serialising anything, so
        // a compacting save never pays to encode updates it is about to discard.
        let snapshot = force_snapshot
            || self.stored.is_none()
            || pending_updates(&self.conn)? + 1 >= MAX_UPDATE_CHUNKS;

        let bytes = match &self.stored {
            Some(stored) if !snapshot => board.export_since(stored)?,
            _ => board.to_bytes()?,
        };

        let wrote = self.wrote;
        let transaction = self.conn.transaction()?;
        if snapshot {
            transaction.execute("DELETE FROM doc_chunk", [])?;
        }
        transaction.execute(
            "INSERT INTO doc_chunk (is_snapshot, bytes) VALUES (?1, ?2)",
            params![i64::from(snapshot), bytes],
        )?;
        upsert_index(&transaction, board)?;
        if !wrote {
            mark_session_dirty(&transaction)?;
        }
        transaction.commit()?;

        self.wrote = true;
        self.stored = Some(version);
        Ok(true)
    }

    /// The board-library row for this board, read without touching the document.
    ///
    /// `Ok(None)` before the first save.
    pub fn index(&self) -> Result<Option<BoardIndex>> {
        let row = self
            .conn
            .query_row(
                "SELECT title, item_count, thumbnail, modified_ms FROM board_index WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((title, item_count, thumbnail, modified_ms)) = row else {
            return Ok(None);
        };
        Ok(Some(BoardIndex {
            path: self.path.clone(),
            title,
            item_count: item_count.max(0) as u64,
            thumbnail: thumbnail.as_deref().map(str::parse).transpose()?,
            modified: UNIX_EPOCH + Duration::from_millis(modified_ms.max(0) as u64),
        }))
    }

    /// Points the library entry at a rendered preview in the blob store.
    ///
    /// Separate from [`BoardDb::save`] because a thumbnail is produced by the
    /// renderer, asynchronously and often long after the edit that invalidated it.
    pub fn set_thumbnail(&mut self, thumbnail: Option<&Hash>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO board_index (id, title, item_count, thumbnail, modified_ms)
             VALUES (1, '', 0, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET thumbnail = excluded.thumbnail",
            params![thumbnail.map(|h| h.to_hex()), now_millis()],
        )?;
        Ok(())
    }

    /// How many chunks the document is currently split across: 1 right after a
    /// snapshot, growing by one per incremental save. Exposed so a maintenance pass
    /// can find files worth compacting.
    pub fn chunk_count(&self) -> Result<u64> {
        let count: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM doc_chunk", [], |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }

    /// Records that this session ended properly, so the next open does not report a
    /// crash. Called for you when the handle drops; call it explicitly when you want
    /// to know that the write succeeded.
    pub fn close(mut self) -> Result<()> {
        self.mark_session_clean()?;
        Ok(())
    }

    fn mark_session_clean(&mut self) -> Result<()> {
        if self.wrote {
            self.conn.execute("UPDATE session SET dirty = 0 WHERE id = 1", [])?;
            self.wrote = false;
        }
        Ok(())
    }

    /// Clears a mark this handle did not set. Only [`BoardDb::recover`] may do this:
    /// it is the point at which the previous session's failure has been dealt with
    /// and reported, and a warning that repeats forever is not a warning.
    fn acknowledge_crash(&mut self) -> Result<()> {
        self.conn.execute("UPDATE session SET dirty = 0 WHERE id = 1", [])?;
        self.wrote = false;
        Ok(())
    }

    // ----- version history ------------------------------------------------

    /// Stores a named point the user can come back to. Kept until deleted.
    ///
    /// Restore points hold a full document snapshot rather than a version marker.
    /// Loro retains the history either way, but a marker would only stay meaningful
    /// while the chunk chain that contains it does, and the chain is collapsed on a
    /// schedule — history that a routine save can silently delete is not history.
    pub fn create_restore_point(&mut self, board: &Board, label: &str) -> Result<i64> {
        self.insert_restore_point(board, Some(label), false)
    }

    /// Stores a point the engine took on its own, eligible for pruning.
    pub fn create_automatic_restore_point(&mut self, board: &Board) -> Result<i64> {
        self.insert_restore_point(board, None, true)
    }

    fn insert_restore_point(
        &mut self,
        board: &Board,
        label: Option<&str>,
        automatic: bool,
    ) -> Result<i64> {
        let snapshot = board.to_bytes()?;
        let digest = blake3::hash(&snapshot).as_bytes().to_vec();

        // Best-effort deduplication: a board that has not changed since the last
        // point re-encodes to the same bytes, and an unchanged snapshot is not a
        // point in history worth paying for. A miss costs one duplicate, not
        // correctness.
        let newest = self
            .conn
            .query_row(
                "SELECT id, content_hash FROM restore_point ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        if let Some((id, existing)) = newest
            && existing == digest
        {
            return Ok(id);
        }

        let wrote = self.wrote;
        let transaction = self.conn.transaction()?;
        transaction.execute(
            "INSERT INTO restore_point
                 (created_ms, label, automatic, title, item_count, content_hash, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                now_millis(),
                label,
                i64::from(automatic),
                board.title(),
                board.item_count() as i64,
                digest,
                snapshot,
            ],
        )?;
        if !wrote {
            mark_session_dirty(&transaction)?;
        }
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        self.wrote = true;
        Ok(id)
    }

    /// Every restore point, newest first, without reading a single snapshot.
    pub fn restore_points(&self) -> Result<Vec<RestorePoint>> {
        let mut statement = self.conn.prepare(
            "SELECT id, created_ms, label, automatic, title, item_count, LENGTH(snapshot)
             FROM restore_point ORDER BY id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(RestorePoint {
                id: row.get(0)?,
                created: UNIX_EPOCH + Duration::from_millis(row.get::<_, i64>(1)?.max(0) as u64),
                label: row.get(2)?,
                automatic: row.get::<_, i64>(3)? != 0,
                title: row.get(4)?,
                item_count: row.get::<_, i64>(5)?.max(0) as u64,
                snapshot_bytes: row.get::<_, i64>(6)?.max(0) as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Reads a restore point's document without changing what the board is now.
    /// This is what a history preview renders.
    pub fn read_restore_point(&self, id: i64) -> Result<Option<Board>> {
        let Some(snapshot) = self.restore_snapshot(id)? else {
            return Ok(None);
        };
        Ok(Some(Board::from_bytes(&snapshot)?))
    }

    /// Makes a restore point the board's current state, and returns it.
    ///
    /// The state being replaced is captured as a restore point first, labelled
    /// [`REPLACED_BY_RESTORE`], so a restore is never a one-way door.
    ///
    /// The chunk chain is then rewritten to hold the restored snapshot alone. The
    /// caller must replace whatever `Board` it was holding with the returned one:
    /// the old handle refers to a document this file no longer describes.
    ///
    /// A note for when LAN sync lands: this replaces local state rather than
    /// appending an inverse change, so a peer that still holds the newer operations
    /// will merge them back in. Restoring is a single-user operation until that is
    /// dealt with.
    pub fn restore(&mut self, id: i64) -> Result<Board> {
        let Some(snapshot) = self.restore_snapshot(id)? else {
            return Err(StoreError::NoSuchRestorePoint(id));
        };

        // Best-effort: a board too damaged to load is exactly the board most worth
        // restoring, so an unreadable current state must not block the way out.
        if let Ok(Some(current)) = self.load() {
            self.insert_restore_point(&current, Some(REPLACED_BY_RESTORE), true)?;
        }

        let board = Board::from_bytes(&snapshot)?;
        self.overwrite_with_snapshot(&board)?;
        Ok(board)
    }

    pub fn delete_restore_point(&mut self, id: i64) -> Result<bool> {
        let deleted = self.conn.execute("DELETE FROM restore_point WHERE id = ?1", params![id])?;
        Ok(deleted > 0)
    }

    /// Keeps the newest `keep` automatic restore points and deletes the rest,
    /// returning how many went. Points the user named are never touched.
    pub fn prune_restore_points(&mut self, keep: usize) -> Result<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM restore_point
              WHERE automatic = 1
                AND id NOT IN (
                    SELECT id FROM restore_point WHERE automatic = 1 ORDER BY id DESC LIMIT ?1
                )",
            params![keep as i64],
        )?;
        Ok(deleted as u64)
    }

    fn restore_snapshot(&self, id: i64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row("SELECT snapshot FROM restore_point WHERE id = ?1", params![id], |row| {
                row.get(0)
            })
            .optional()?)
    }

    fn newest_usable_restore_point(&self) -> Result<Option<(i64, Board)>> {
        let mut statement =
            self.conn.prepare("SELECT id, snapshot FROM restore_point ORDER BY id DESC")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let snapshot: Vec<u8> = row.get(1)?;
            if let Ok(board) = Board::from_bytes(&snapshot) {
                return Ok(Some((id, board)));
            }
        }
        Ok(None)
    }

    // ----- recovery internals ---------------------------------------------

    /// Replays the chain until something will not apply, returning the board built
    /// so far and how many chunks went into it.
    fn replay_longest_prefix(&self) -> Result<(Option<Board>, u64)> {
        let mut statement =
            self.conn.prepare("SELECT is_snapshot, bytes FROM doc_chunk ORDER BY seq")?;
        let mut rows = statement.query([])?;
        let mut board: Option<Board> = None;
        let mut replayed = 0u64;

        while let Some(row) = rows.next()? {
            let is_snapshot: i64 = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            match &mut board {
                None => {
                    if is_snapshot != 1 {
                        break;
                    }
                    match Board::from_bytes(&bytes) {
                        Ok(recovered) => board = Some(recovered),
                        Err(_) => break,
                    }
                }
                Some(board) => {
                    if board.apply(&bytes).is_err() {
                        break;
                    }
                }
            }
            replayed += 1;
        }
        Ok((board, replayed))
    }

    fn overwrite_with_snapshot(&mut self, board: &Board) -> Result<()> {
        let bytes = board.to_bytes()?;
        let version = board.version();

        let wrote = self.wrote;
        let transaction = self.conn.transaction()?;
        transaction.execute("DELETE FROM doc_chunk", [])?;
        transaction
            .execute("INSERT INTO doc_chunk (is_snapshot, bytes) VALUES (1, ?1)", params![bytes])?;
        upsert_index(&transaction, board)?;
        if !wrote {
            mark_session_dirty(&transaction)?;
        }
        transaction.commit()?;

        self.wrote = true;
        self.stored = Some(version);
        Ok(())
    }
}

/// A handle that wrote and then dropped is a session that ended in an orderly way,
/// which is precisely what a crash is not. Best-effort: there is nothing useful to
/// do with an error here, and failing to clear the mark only costs a spurious
/// recovery report next time.
impl Drop for BoardDb {
    fn drop(&mut self) {
        let _ = self.mark_session_clean();
    }
}

impl std::fmt::Debug for BoardDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoardDb")
            .field("path", &self.path)
            .field("recovery", &self.recovery)
            .finish_non_exhaustive()
    }
}

/// The thumbnail is not derived from the document, so it is carried across rather
/// than overwritten by a save that knows nothing about it.
fn upsert_index(transaction: &Transaction<'_>, board: &Board) -> Result<()> {
    transaction.execute(
        "INSERT INTO board_index (id, title, item_count, thumbnail, modified_ms)
         VALUES (1, ?1, ?2, (SELECT thumbnail FROM board_index WHERE id = 1), ?3)
         ON CONFLICT(id) DO UPDATE SET
             title       = excluded.title,
             item_count  = excluded.item_count,
             modified_ms = excluded.modified_ms",
        params![board.title(), board.item_count() as i64, now_millis()],
    )?;
    Ok(())
}

/// Claims the file for this session, inside the same transaction as the write that
/// prompted it. Committing the mark together with the data is what makes it proof:
/// there is no window in which data is on disk but the file still looks idle.
fn mark_session_dirty(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute(
        "INSERT INTO session (id, dirty, opened_ms) VALUES (1, 1, ?1)
         ON CONFLICT(id) DO UPDATE SET dirty = 1, opened_ms = excluded.opened_ms",
        params![now_millis()],
    )?;
    Ok(())
}

fn write_ahead_log_present(path: &Path) -> bool {
    let mut log = path.as_os_str().to_os_string();
    log.push("-wal");
    std::fs::metadata(log).is_ok_and(|meta| meta.len() > 0)
}

fn pending_updates(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM doc_chunk WHERE is_snapshot = 0", [], |row| {
        row.get(0)
    })?)
}

/// Milliseconds since the epoch, or zero for a clock set before 1970. A wrong
/// timestamp sorts the library oddly; a panic loses the save.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Lists every board in a directory, cheaply.
///
/// Files that are not readable board databases are skipped rather than failing the
/// whole listing: one damaged board must not blank the library. A caller that needs
/// to know why a specific file did not appear should open it with [`BoardDb::open`].
pub fn list_boards(directory: impl AsRef<Path>) -> Result<Vec<BoardIndex>> {
    let mut boards = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != BOARD_EXTENSION) {
            continue;
        }
        if let Ok(db) = BoardDb::open(&path)
            && let Ok(Some(index)) = db.index()
        {
            boards.push(index);
        }
    }
    boards.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
    Ok(boards)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{ItemKind, NewItem, Placement, StyledText};

    fn sticky(text: &str) -> NewItem {
        NewItem::new(
            ItemKind::Sticky { text: StyledText::plain(text), background: None },
            Placement::new(0.0, 0.0, 199.0, 228.0),
        )
    }

    fn temp_db() -> (tempfile::TempDir, BoardDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = BoardDb::open(dir.path().join(format!("board.{BOARD_EXTENSION}"))).unwrap();
        (dir, db)
    }

    /// Crash safety rests entirely on these two pragmas, and a typo in a pragma
    /// name is silently ignored by SQLite — so the settings are read back rather
    /// than assumed.
    #[test]
    fn board_files_are_opened_in_wal_mode_with_full_synchronisation() {
        let (_dir, db) = temp_db();

        let journal: String =
            db.conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        assert_eq!(journal.to_lowercase(), "wal");

        // 2 is `FULL` (0 off, 1 normal, 2 full, 3 extra): a committed transaction is
        // on the platter before `save` returns, so a power cut costs nothing that was
        // reported as saved. `EXTRA` adds nothing in WAL mode.
        let synchronous: i64 =
            db.conn.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
        assert_eq!(synchronous, 2);
    }

    #[test]
    fn a_fresh_file_holds_no_board() {
        let (_dir, mut db) = temp_db();
        assert!(db.load().unwrap().is_none());
        assert!(db.index().unwrap().is_none());
        assert_eq!(db.chunk_count().unwrap(), 0);
    }

    #[test]
    fn a_board_survives_a_save_and_load() {
        let (dir, mut db) = temp_db();
        let mut board = Board::new();
        board.set_title("Engine bay").unwrap();
        let frame = board.add(sticky("frame")).unwrap();
        board.add(sticky("nested").with_parent(frame)).unwrap();
        db.save(&board).unwrap();
        drop(db);

        let mut reopened =
            BoardDb::open(dir.path().join(format!("board.{BOARD_EXTENSION}"))).unwrap();
        let loaded = reopened.load().unwrap().unwrap();

        assert_eq!(loaded.title(), "Engine bay");
        assert_eq!(loaded.items().unwrap(), board.items().unwrap());
    }

    /// The point of the chunk chain: a save after a small edit appends a small
    /// delta instead of rewriting the document.
    #[test]
    fn subsequent_saves_append_updates_rather_than_snapshots() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        for i in 0..40 {
            board.add(sticky(&format!("note {i}"))).unwrap();
        }

        db.save(&board).unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1, "the first save should be a snapshot");
        let after_snapshot = file_size(db.path());

        let id = board.add(sticky("one more")).unwrap();
        board.translate(id, 5.0, 5.0).unwrap();
        db.save(&board).unwrap();

        assert_eq!(db.chunk_count().unwrap(), 2);
        let growth = file_size(db.path()) - after_snapshot;
        assert!(growth < after_snapshot, "an incremental save grew the file by {growth} bytes");
    }

    #[test]
    fn a_chunk_chain_replays_to_the_same_board() {
        let (_dir, mut db) = temp_db();
        let path = db.path().to_path_buf();
        let mut board = Board::new();

        db.save(&board).unwrap();
        for i in 0..8 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            db.save(&board).unwrap();
        }
        assert_eq!(db.chunk_count().unwrap(), 9);
        drop(db);

        let mut reopened = BoardDb::open(&path).unwrap();
        let loaded = reopened.load().unwrap().unwrap();
        assert_eq!(loaded.items().unwrap(), board.items().unwrap());
        assert_eq!(loaded.item_count(), 8);
    }

    #[test]
    fn the_chain_collapses_once_it_grows_too_long() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        db.save(&board).unwrap();

        for i in 0..MAX_UPDATE_CHUNKS + 4 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            db.save(&board).unwrap();
            assert!(
                db.chunk_count().unwrap() <= MAX_UPDATE_CHUNKS as u64,
                "the chain grew past its limit at save {i}"
            );
        }

        let loaded = db.load().unwrap().unwrap();
        assert_eq!(loaded.item_count(), (MAX_UPDATE_CHUNKS + 4) as usize);
    }

    #[test]
    fn compacting_leaves_exactly_one_snapshot() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        db.save(&board).unwrap();
        for i in 0..5 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            db.save(&board).unwrap();
        }
        assert_eq!(db.chunk_count().unwrap(), 6);

        db.compact(&board).unwrap();

        assert_eq!(db.chunk_count().unwrap(), 1);
        assert_eq!(db.load().unwrap().unwrap().items().unwrap(), board.items().unwrap());
    }

    #[test]
    fn saving_an_unchanged_board_writes_nothing() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.add(sticky("note")).unwrap();
        assert!(db.save(&board).unwrap(), "the first save had something to write");

        assert!(!db.save(&board).unwrap());
        assert!(!db.save(&board).unwrap());

        assert_eq!(db.chunk_count().unwrap(), 1);
    }

    /// The library listing must be answerable without decoding the CRDT, so it is
    /// checked on a handle that has never loaded the board.
    #[test]
    fn the_index_is_readable_without_loading_the_document() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.set_title("Reference Board").unwrap();
        for i in 0..7 {
            board.add(sticky(&format!("note {i}"))).unwrap();
        }
        db.save(&board).unwrap();

        let fresh = BoardDb::open(db.path()).unwrap();
        let index = fresh.index().unwrap().unwrap();

        assert_eq!(index.title, "Reference Board");
        assert_eq!(index.item_count, 7);
        assert_eq!(index.thumbnail, None);
        assert!(index.modified > UNIX_EPOCH);
        assert_eq!(index.path, db.path());
    }

    #[test]
    fn the_index_tracks_later_edits() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.set_title("Draft").unwrap();
        db.save(&board).unwrap();

        board.set_title("Final").unwrap();
        board.add(sticky("note")).unwrap();
        db.save(&board).unwrap();

        let index = db.index().unwrap().unwrap();
        assert_eq!(index.title, "Final");
        assert_eq!(index.item_count, 1);
    }

    #[test]
    fn a_thumbnail_can_be_set_before_or_after_a_save_and_survives_both() {
        let (_dir, mut db) = temp_db();
        let hash = Hash::of(b"a rendered preview");

        db.set_thumbnail(Some(&hash)).unwrap();
        assert_eq!(db.index().unwrap().unwrap().thumbnail, Some(hash));

        let mut board = Board::new();
        board.set_title("Titled after the fact").unwrap();
        db.save(&board).unwrap();

        let index = db.index().unwrap().unwrap();
        assert_eq!(index.thumbnail, Some(hash), "saving cleared the thumbnail");
        assert_eq!(index.title, "Titled after the fact");

        db.set_thumbnail(None).unwrap();
        assert_eq!(db.index().unwrap().unwrap().thumbnail, None);
    }

    #[test]
    fn a_board_file_without_a_leading_snapshot_is_reported_as_damaged() {
        let (_dir, mut db) = temp_db();
        db.save(&Board::new()).unwrap();
        db.conn.execute("UPDATE doc_chunk SET is_snapshot = 0", []).unwrap();

        assert!(matches!(db.load(), Err(StoreError::MissingSnapshot)));
    }

    #[test]
    fn a_file_from_a_future_build_refuses_to_open() {
        let (dir, db) = temp_db();
        let path = db.path().to_path_buf();
        db.conn.pragma_update(None, "user_version", STORE_SCHEMA_VERSION + 1).unwrap();
        drop(db);

        assert!(matches!(BoardDb::open(&path), Err(StoreError::UnsupportedSchema { .. })));
        drop(dir);
    }

    #[test]
    fn the_library_lists_boards_newest_first_and_ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        for (index, title) in ["oldest", "middle", "newest"].iter().enumerate() {
            let mut db =
                BoardDb::open(dir.path().join(format!("{index}.{BOARD_EXTENSION}"))).unwrap();
            let mut board = Board::new();
            board.set_title(title).unwrap();
            board.add(sticky("note")).unwrap();
            db.save(&board).unwrap();
            // The index is timestamped to the millisecond; make the order legible.
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(dir.path().join("notes.txt"), b"not a board").unwrap();
        std::fs::write(dir.path().join("half-written.vellum"), b"not a database").unwrap();

        let boards = list_boards(dir.path()).unwrap();

        let titles: Vec<_> = boards.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles, ["newest", "middle", "oldest"]);
        assert!(boards.iter().all(|b| b.item_count == 1));
    }

    #[test]
    fn listing_an_empty_directory_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_boards(dir.path()).unwrap().is_empty());
    }

    // ----- crash detection --------------------------------------------------

    #[test]
    fn a_board_closed_properly_opens_clean() {
        let (dir, mut db) = temp_db();
        let path = db.path().to_path_buf();
        let mut board = Board::new();
        board.add(sticky("note")).unwrap();
        db.save(&board).unwrap();
        db.close().unwrap();

        let reopened = BoardDb::open(&path).unwrap();
        assert!(reopened.recovery().is_clean());
        assert!(!reopened.recovery().unclean_shutdown);
        drop(dir);
    }

    /// A handle dropped without `close` is still an orderly end — the process ran
    /// its destructors. What must not clear the mark is a process that never got to.
    #[test]
    fn dropping_a_handle_also_ends_the_session_cleanly() {
        let (dir, mut db) = temp_db();
        let path = db.path().to_path_buf();
        db.save(&Board::new()).unwrap();
        drop(db);

        assert!(BoardDb::open(&path).unwrap().recovery().is_clean());
        drop(dir);
    }

    /// Stands in for a killed process: the dirty mark is committed with the data and
    /// nothing ever clears it. The real proof, against an actual `SIGKILL`, is in
    /// `tests/crash_recovery.rs`.
    #[test]
    fn a_session_that_never_closed_is_reported_as_unclean() {
        let (dir, mut db) = temp_db();
        let path = db.path().to_path_buf();
        db.save(&Board::new()).unwrap();
        db.conn.execute("UPDATE session SET dirty = 1", []).unwrap();
        db.wrote = false; // as if this handle's destructor never ran
        drop(db);

        let reopened = BoardDb::open(&path).unwrap();
        assert!(reopened.recovery().unclean_shutdown);
        assert!(!reopened.recovery().is_clean());
        drop(dir);
    }

    /// The board library opens every file it lists. Doing so must not look like an
    /// editing session, or a crash would be forgotten before the user came back.
    #[test]
    fn opening_a_board_without_writing_leaves_the_crash_mark_alone() {
        let (dir, mut db) = temp_db();
        let path = db.path().to_path_buf();
        db.save(&Board::new()).unwrap();
        db.conn.execute("UPDATE session SET dirty = 1", []).unwrap();
        db.wrote = false;
        drop(db);

        for _ in 0..3 {
            let listing = BoardDb::open(&path).unwrap();
            assert!(listing.recovery().unclean_shutdown);
        }
        assert!(BoardDb::open(&path).unwrap().recovery().unclean_shutdown);
        drop(dir);
    }

    #[test]
    fn a_fresh_board_file_is_not_a_crash() {
        let (_dir, db) = temp_db();
        assert_eq!(db.recovery(), Recovery::default());
    }

    #[test]
    fn a_sound_file_passes_the_integrity_check() {
        let (_dir, mut db) = temp_db();
        db.save(&Board::new()).unwrap();
        assert!(db.check_integrity().unwrap());
    }

    // ----- recovery ---------------------------------------------------------

    #[test]
    fn recovering_an_undamaged_board_discards_nothing() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        db.save(&board).unwrap();
        for i in 0..5 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            db.save(&board).unwrap();
        }

        let recovered = db.recover().unwrap().unwrap();

        assert_eq!(recovered.discarded_chunks, 0);
        assert_eq!(recovered.from_restore_point, None);
        assert_eq!(recovered.board.items().unwrap(), board.items().unwrap());
        assert_eq!(db.chunk_count().unwrap(), 6, "an intact chain was rewritten for no reason");
    }

    #[test]
    fn recovering_an_empty_file_finds_nothing_to_recover() {
        let (_dir, mut db) = temp_db();
        assert!(db.recover().unwrap().is_none());
    }

    /// A crash that has been recovered from is dealt with. Reporting it on every
    /// subsequent open would train the user to ignore the one that matters.
    #[test]
    fn recovering_acknowledges_the_crash_it_recovered_from() {
        let (dir, mut db) = temp_db();
        let path = db.path().to_path_buf();
        db.save(&Board::new()).unwrap();
        db.conn.execute("UPDATE session SET dirty = 1", []).unwrap();
        db.wrote = false;
        drop(db);

        let mut crashed = BoardDb::open(&path).unwrap();
        assert!(crashed.recovery().unclean_shutdown);
        crashed.recover().unwrap().unwrap();
        // This handle still reports what it found when it opened.
        assert!(crashed.recovery().unclean_shutdown);
        drop(crashed);

        assert!(BoardDb::open(&path).unwrap().recovery().is_clean());
        drop(dir);
    }

    /// SQLite's atomic commits should make this impossible, so it stands for damage
    /// from below: a bad sector, a truncated copy, a sync tool splicing files.
    #[test]
    fn a_damaged_tail_chunk_is_dropped_and_the_prefix_survives() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        db.save(&board).unwrap();
        for i in 0..4 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            db.save(&board).unwrap();
        }
        db.conn
            .execute(
                "UPDATE doc_chunk SET bytes = ?1 WHERE seq = (SELECT MAX(seq) FROM doc_chunk)",
                params![b"not a loro update".to_vec()],
            )
            .unwrap();

        let recovered = db.recover().unwrap().unwrap();

        assert_eq!(recovered.discarded_chunks, 1);
        assert_eq!(recovered.board.item_count(), 3, "lost more than the damaged chunk");
        // The damaged chunk is gone from the file, not merely skipped this once.
        assert_eq!(db.chunk_count().unwrap(), 1);
        assert_eq!(db.load().unwrap().unwrap().item_count(), 3);
    }

    #[test]
    fn a_strict_load_still_refuses_what_recovery_repairs() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        db.save(&board).unwrap();
        board.add(sticky("note")).unwrap();
        db.save(&board).unwrap();
        db.conn
            .execute(
                "UPDATE doc_chunk SET bytes = ?1 WHERE seq = (SELECT MAX(seq) FROM doc_chunk)",
                params![b"garbage".to_vec()],
            )
            .unwrap();

        assert!(db.load().is_err(), "a strict load accepted a damaged chain");
        assert!(db.recover().unwrap().is_some());
    }

    /// The last ladder down: the snapshot at the head of the chain is unreadable, so
    /// the only thing left is version history.
    #[test]
    fn an_unreadable_snapshot_falls_back_to_the_newest_restore_point() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.set_title("Cooling system").unwrap();
        for i in 0..3 {
            board.add(sticky(&format!("note {i}"))).unwrap();
        }
        db.save(&board).unwrap();
        let point = db.create_restore_point(&board, "end of day").unwrap();

        board.add(sticky("added after the restore point")).unwrap();
        db.save(&board).unwrap();
        db.conn.execute("UPDATE doc_chunk SET bytes = ?1", params![b"shredded".to_vec()]).unwrap();

        let recovered = db.recover().unwrap().unwrap();

        assert_eq!(recovered.from_restore_point, Some(point));
        assert_eq!(recovered.discarded_chunks, 2);
        assert_eq!(recovered.board.item_count(), 3);
        assert_eq!(recovered.board.title(), "Cooling system");
        assert_eq!(db.load().unwrap().unwrap().item_count(), 3);
    }

    #[test]
    fn an_unreadable_snapshot_with_no_history_is_an_error() {
        let (_dir, mut db) = temp_db();
        db.save(&Board::new()).unwrap();
        db.conn.execute("UPDATE doc_chunk SET bytes = ?1", params![b"shredded".to_vec()]).unwrap();

        assert!(matches!(db.recover(), Err(StoreError::MissingSnapshot)));
    }

    /// Recovery has to leave the handle able to keep saving incrementally, or the
    /// first save after a crash would rewrite the whole document.
    #[test]
    fn saving_continues_incrementally_after_a_recovery() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        db.save(&board).unwrap();
        board.add(sticky("note")).unwrap();
        db.save(&board).unwrap();

        let mut board = db.recover().unwrap().unwrap().board;
        let chunks = db.chunk_count().unwrap();

        board.add(sticky("after recovery")).unwrap();
        db.save(&board).unwrap();

        assert_eq!(db.chunk_count().unwrap(), chunks + 1, "recovery forced a full rewrite");
        assert_eq!(db.load().unwrap().unwrap().item_count(), 2);
    }

    // ----- version history --------------------------------------------------

    #[test]
    fn a_restore_point_keeps_the_document_it_was_taken_from() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.set_title("Draft").unwrap();
        board.add(sticky("first")).unwrap();
        db.save(&board).unwrap();

        let point = db.create_restore_point(&board, "first draft").unwrap();

        board.set_title("Rewritten").unwrap();
        board.add(sticky("second")).unwrap();
        db.save(&board).unwrap();

        let history = db.read_restore_point(point).unwrap().unwrap();
        assert_eq!(history.title(), "Draft");
        assert_eq!(history.item_count(), 1);
        // Reading history left the present alone.
        assert_eq!(db.load().unwrap().unwrap().item_count(), 2);
    }

    #[test]
    fn restore_points_are_listed_newest_first_with_their_metadata() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.set_title("Morning").unwrap();
        board.add(sticky("a")).unwrap();
        db.create_restore_point(&board, "morning").unwrap();

        board.set_title("Afternoon").unwrap();
        board.add(sticky("b")).unwrap();
        db.create_automatic_restore_point(&board).unwrap();

        let points = db.restore_points().unwrap();

        assert_eq!(points.len(), 2);
        assert_eq!(points[0].title, "Afternoon");
        assert_eq!(points[0].item_count, 2);
        assert_eq!(points[0].label, None);
        assert!(points[0].automatic);
        assert!(points[0].snapshot_bytes > 0);
        assert_eq!(points[1].label.as_deref(), Some("morning"));
        assert!(!points[1].automatic);
        assert!(points[0].created >= points[1].created);
    }

    #[test]
    fn an_unchanged_board_does_not_get_a_second_restore_point() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.add(sticky("note")).unwrap();

        let first = db.create_automatic_restore_point(&board).unwrap();
        let second = db.create_automatic_restore_point(&board).unwrap();

        assert_eq!(first, second);
        assert_eq!(db.restore_points().unwrap().len(), 1);
    }

    #[test]
    fn restoring_makes_an_old_state_current_and_keeps_the_one_it_replaced() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.set_title("Before").unwrap();
        board.add(sticky("kept")).unwrap();
        db.save(&board).unwrap();
        let point = db.create_restore_point(&board, "before the mistake").unwrap();

        board.set_title("After").unwrap();
        for i in 0..4 {
            board.add(sticky(&format!("mistake {i}"))).unwrap();
        }
        db.save(&board).unwrap();

        let restored = db.restore(point).unwrap();

        assert_eq!(restored.title(), "Before");
        assert_eq!(restored.item_count(), 1);
        assert_eq!(db.load().unwrap().unwrap().item_count(), 1);
        assert_eq!(db.index().unwrap().unwrap().title, "Before");
        assert_eq!(db.chunk_count().unwrap(), 1);

        // The state that was replaced is still reachable, so this is reversible.
        let safety = db
            .restore_points()
            .unwrap()
            .into_iter()
            .find(|p| p.label.as_deref() == Some(REPLACED_BY_RESTORE))
            .expect("no safety point was taken");
        assert_eq!(safety.item_count, 5);
        assert_eq!(db.restore(safety.id).unwrap().title(), "After");
    }

    /// A restore leaves the handle in a state where the next edit is a delta on top
    /// of the restored snapshot, not a fresh rewrite.
    #[test]
    fn editing_continues_normally_after_a_restore() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.add(sticky("original")).unwrap();
        db.save(&board).unwrap();
        let point = db.create_restore_point(&board, "original").unwrap();
        board.add(sticky("later")).unwrap();
        db.save(&board).unwrap();

        let mut restored = db.restore(point).unwrap();
        restored.add(sticky("after the restore")).unwrap();
        db.save(&restored).unwrap();

        assert_eq!(db.chunk_count().unwrap(), 2);
        assert_eq!(db.load().unwrap().unwrap().item_count(), 2);
    }

    #[test]
    fn restoring_a_point_that_is_not_there_is_an_error() {
        let (_dir, mut db) = temp_db();
        assert!(matches!(db.restore(404), Err(StoreError::NoSuchRestorePoint(404))));
        assert!(db.read_restore_point(404).unwrap().is_none());
    }

    #[test]
    fn pruning_keeps_the_newest_automatic_points_and_every_named_one() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        let mut automatic = Vec::new();
        for i in 0..10 {
            board.add(sticky(&format!("note {i}"))).unwrap();
            automatic.push(db.create_automatic_restore_point(&board).unwrap());
            if i == 2 {
                board.add(sticky("named")).unwrap();
                db.create_restore_point(&board, "keep me").unwrap();
            }
        }

        let deleted = db.prune_restore_points(3).unwrap();

        assert_eq!(deleted, 7);
        let left = db.restore_points().unwrap();
        assert_eq!(left.len(), 4);
        assert_eq!(left.iter().filter(|p| p.automatic).count(), 3);
        assert!(left.iter().any(|p| p.label.as_deref() == Some("keep me")));
        let newest: Vec<_> = left.iter().filter(|p| p.automatic).map(|p| p.id).collect();
        assert_eq!(newest, vec![automatic[9], automatic[8], automatic[7]]);
    }

    #[test]
    fn deleting_a_restore_point_reports_whether_it_was_there() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        board.add(sticky("note")).unwrap();
        let point = db.create_restore_point(&board, "only").unwrap();

        assert!(db.delete_restore_point(point).unwrap());
        assert!(!db.delete_restore_point(point).unwrap());
        assert!(db.restore_points().unwrap().is_empty());
    }

    #[test]
    fn vacuuming_reclaims_the_space_deleted_history_was_using() {
        let (_dir, mut db) = temp_db();
        let mut board = Board::new();
        for i in 0..200 {
            board.add(sticky(&format!("a reasonably long sticky body, number {i}"))).unwrap();
        }
        db.save(&board).unwrap();
        for i in 0..8 {
            board.add(sticky(&format!("more {i}"))).unwrap();
            db.save(&board).unwrap();
            db.create_automatic_restore_point(&board).unwrap();
        }
        let with_history = checkpointed_size(&db);

        db.prune_restore_points(0).unwrap();
        db.vacuum().unwrap();

        assert!(db.restore_points().unwrap().is_empty());
        let shrunk = checkpointed_size(&db);
        assert!(shrunk < with_history, "vacuum left {shrunk} of {with_history} bytes");
        assert_eq!(db.load().unwrap().unwrap().item_count(), 208);
    }

    /// v2 added tables; a file written by a v1 build must still open, keep its
    /// document, and gain the new ones.
    #[test]
    fn a_file_from_an_older_schema_is_migrated_in_place() {
        let (dir, mut db) = temp_db();
        let path = db.path().to_path_buf();
        let mut board = Board::new();
        board.add(sticky("written by v1")).unwrap();
        db.save(&board).unwrap();
        db.conn.execute_batch("DROP TABLE session; DROP TABLE restore_point;").unwrap();
        db.conn.pragma_update(None, "user_version", 1).unwrap();
        db.wrote = false;
        drop(db);

        let mut reopened = BoardDb::open(&path).unwrap();

        let version: i64 =
            reopened.conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, STORE_SCHEMA_VERSION);
        assert!(reopened.recovery().is_clean());
        assert!(reopened.restore_points().unwrap().is_empty());
        assert_eq!(reopened.load().unwrap().unwrap().item_count(), 1);
        drop(dir);
    }

    fn file_size(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// In WAL mode a committed row lives in the log until a checkpoint folds it into
    /// the database, so the database file on its own says nothing about how much has
    /// been stored. Anything measuring file size has to fold it in first.
    fn checkpointed_size(db: &BoardDb) -> u64 {
        db.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
        file_size(db.path())
    }
}
