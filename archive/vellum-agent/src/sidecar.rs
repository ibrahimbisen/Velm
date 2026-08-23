//! Where a transcript lives, which is deliberately not in the board.
//!
//! ```text
//!   <data-dir>/agents/<board-key>/<item-id>.jsonl
//! ```
//!
//! `docs/07-agent-canvas.md` §4 is the contract, and RULE ZERO is the reason for it:
//! streaming agent output into the Loro document would make every token an undo step, grow
//! the board file without bound, and put third-party text inside the file RULE ZERO exists to
//! protect. A transcript is **disposable** — deleting this whole directory loses history and
//! no board content — and a missing file means *this agent has not run yet*, never an error.
//!
//! # One line is a [`Record`], not a bare event
//!
//! ```text
//!   {"at":1770000000,"event":{"event":"text","text":"…"}}
//! ```
//!
//! [`crate::TranscriptEvent`] carries **no timestamp**, on purpose: it is the one stream four
//! consumers read, and a field only the digest uses would be on every event in memory and in
//! every equality check. The time an event *arrived* is a property of the record on disk, so
//! it lives here — which is also what makes the away-mode summary possible (`docs/07` §1's
//! `summary.rs`) without touching the event at all.
//!
//! **The clock is a parameter.** [`Sidecar::append`] takes `now`; nothing in this crate reads
//! the time itself, so *"does a digest split state from news correctly at 18:00"* is
//! arithmetic rather than a wait.
//!
//! # Three properties, and the awkward one is the third
//!
//! - **Appending is cheap**: one `write_all` of one line, no read, no rewrite.
//! - **Tailing is cheap**: the painter only ever wants the last handful of events, so
//!   [`Sidecar::tail`] seeks from the end and reads a window rather than parsing the file.
//! - **A torn last line is dropped, not fatal.** The app can be killed between the `write`
//!   and the `\n` — during a crash, a force quit, a power cut — and the next read must
//!   recover everything before that point rather than failing the whole file. This is the
//!   same posture as RULE ZERO's *"an unknown value degrades, it never aborts a load"*,
//!   applied to a file that is written continuously.
//!
//! # ⚠ The board key is FNV-1a, where `docs/07` §4 says BLAKE3
//!
//! Deliberate. `blake3` is a workspace dependency but **not** one of this crate's four, and
//! `vellum-agent` earns its "compiles and tests on a machine with no graphics stack" by
//! keeping that list short. The hash is not security-relevant: it exists so a transcript
//! follows its board and two boards never collide, and 64 bits of FNV over a canonical path
//! answers both. Nothing outside this module may depend on the algorithm — the key is an
//! opaque string.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::schedule::Timestamp;
use crate::transcript::TranscriptEvent;
use crate::{AgentError, Result};

/// The directory under the data directory that every transcript lives in.
pub const AGENTS_DIR: &str = "agents";

/// How much of the end of a file a read looks at before doubling.
///
/// 64KB holds a few hundred ordinary records. The doubling loop is what makes the number a
/// starting point rather than a limit: a transcript full of long tool outputs still yields
/// what was asked for, it just costs a second read.
const TAIL_WINDOW: u64 = 64 * 1024;

/// The furthest back from the end of a transcript a read will ever look.
///
/// ⚠ **The doubling had no ceiling, and [`MAX_SINCE_RECORDS`] is not one** — that caps the
/// records *kept*, and says nothing about the bytes read to find them. `since`'s stopping
/// condition is *"one record older than the cutoff"*, which a cutoff older than the whole file
/// can never satisfy: the window doubled past the file's length and `read_to_end` brought an
/// overnight transcript into memory whole, inside the app's own process. "Since last week" on
/// a Monday is exactly that question, and it is the ordinary way the away-mode digest is
/// asked.
///
/// 8MB is roughly the last thirty thousand ordinary records. A read that stops here answers
/// with `complete: false`, which is what that field already means.
///
/// ⚠ **What reads it, precisely, because "every caller does" was written here and is not
/// true.** One production reader does: `AgentRuntime`'s node loader turns it into the node's
/// `truncated` flag, which is what puts *"earlier output is not shown"* on the card. The other
/// caller — the away-mode digest in `crate::summary`, which is the very question that made this
/// ceiling necessary — takes `records` and **drops** `complete` on the floor. So a digest built
/// from a read that stopped at the ceiling reports what it found without saying it stopped
/// looking. That is a gap in the digest, not in this bound, and it is stated rather than
/// implied to be covered.
const MAX_TAIL_WINDOW: u64 = 8 * 1024 * 1024;

/// The most records a time-based read will answer with.
///
/// [`Sidecar::since`] deliberately returns the state *before* its cutoff as well as the news
/// after it, so without a cap "everything since last week" is the whole file — and the
/// caller is a once-a-session digest, not a paging reader.
///
/// ⚠ A cap on records is not a cap on bytes read. See [`MAX_TAIL_WINDOW`] for the other half.
pub const MAX_SINCE_RECORDS: usize = 2_000;

/// One line of a transcript: an event and when it was appended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Seconds since the Unix epoch, supplied by the caller. Reusing
    /// [`crate::schedule::Timestamp`] rather than declaring a second time type: two of them
    /// in one crate is two things that can disagree about what a second is.
    pub at: Timestamp,
    pub event: TranscriptEvent,
}

/// Which board a transcript belongs to.
///
/// A newtype rather than a bare `String` so a board key and an item id — both strings, both
/// path components, adjacent in every signature here — cannot be passed in the wrong order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoardKey(String);

impl BoardKey {
    /// Derives the key from the board file's path.
    ///
    /// Canonicalised first, so `./board.vellum` and `/Users/…/board.vellum` are one board and
    /// not two transcripts. A path that cannot be canonicalised — the board was deleted, or
    /// the volume is gone — falls back to the path as given rather than failing: reading a
    /// transcript for a board that has moved is a better answer than refusing to read one.
    pub fn for_board(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Self(format!("{:016x}", fnv1a(canonical.to_string_lossy().as_bytes())))
    }

    /// Wraps a key that has already been derived — from the sidecar's own directory listing,
    /// or from a previous session.
    pub fn from_raw(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BoardKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What a read recovered, and what it could not.
///
/// `dropped` is reported rather than swallowed because it is the only evidence a crash left
/// behind: a transcript that quietly loses its last line every time looks like an agent that
/// never finishes its final sentence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    /// Oldest first, as they were written.
    pub records: Vec<Record>,
    /// Lines that were present and could not be parsed — a torn write, or a line from a
    /// later build carrying an event this one has never heard of.
    pub dropped: usize,
    /// Whether this is the whole file. `false` means the read stopped short, which is the
    /// normal case for a bounded tail of a long transcript.
    pub complete: bool,
}

impl Tail {
    /// The transcript of an agent that has not run yet. Not an error — see the module note.
    fn missing() -> Self {
        Self { records: Vec::new(), dropped: 0, complete: true }
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The events alone, for a caller that does not care when they happened.
    pub fn events(&self) -> impl Iterator<Item = &TranscriptEvent> {
        self.records.iter().map(|record| &record.event)
    }

    /// Where the news begins: the index of the first record at or after `since`.
    ///
    /// **The split point, not a filter.** Everything before it is still in `records`, which
    /// is the whole point — an unanswered permission request asked *before* the user walked
    /// away is still blocking now, and a read that dropped it would report that board as all
    /// quiet. That is precisely the failure the away-mode digest exists to prevent.
    pub fn first_after(&self, since: Timestamp) -> usize {
        self.records.partition_point(|record| record.at < since)
    }
}

/// The transcript store: one directory per board, one file per agent node.
#[derive(Debug, Clone)]
pub struct Sidecar {
    root: PathBuf,
}

impl Sidecar {
    /// Roots the store at `<data-dir>/agents`.
    ///
    /// The data directory is passed in rather than resolved here for the same reason the
    /// clock is: a module that reaches for `~/Library/Application Support/Vellum` cannot be
    /// tested without touching the user's real boards, and RULE ZERO's *"verify against a
    /// copy, never the original"* is much easier to honour when the copy is a parameter.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self { root: data_dir.as_ref().join(AGENTS_DIR) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where one agent's transcript lives. Creates nothing.
    pub fn path_for(&self, board: &BoardKey, item: &str) -> PathBuf {
        self.root.join(safe_name(board.as_str())).join(format!("{}.jsonl", safe_name(item)))
    }

    /// Appends one event, stamped with the time the **caller** says it is.
    ///
    /// Opens the file each time, which is right for the occasional event and wrong for a
    /// streaming turn — use [`Sidecar::appender`] there, which holds the handle open.
    pub fn append(
        &self,
        board: &BoardKey,
        item: &str,
        at: Timestamp,
        event: &TranscriptEvent,
    ) -> Result<()> {
        self.appender(board, item)?.write(at, event)
    }

    /// Opens the transcript for a run of appends.
    ///
    /// ⚠ **Heals a torn line before writing anything.** A file that does not end in a
    /// newline was interrupted mid-write, and appending straight onto it glues the new
    /// record to the fragment — one unparseable line instead of one dropped fragment and one
    /// good record. So a torn file gets its newline first: the tear costs exactly what it
    /// tore, and every append after a crash lands whole.
    pub fn appender(&self, board: &BoardKey, item: &str) -> Result<Appender> {
        let path = self.path_for(board, item);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AgentError::file(parent.display().to_string(), &error))?;
        }
        // `read` as well as `append`, only to look at the last byte. Writes still go to the
        // end whatever the read cursor does — that is what `O_APPEND` means.
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?;

        let length = file
            .metadata()
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?
            .len();
        if length > 0 {
            let mut last = [0u8; 1];
            let read = file.seek(SeekFrom::Start(length - 1)).is_ok()
                && file.read_exact(&mut last).is_ok();
            let torn = read && last[0] != b'\n';
            if torn {
                file.write_all(b"\n")
                    .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
            }
        }
        Ok(Appender { file, path })
    }

    /// The last `limit` records, cheaply. **The primary read**, and the only one the painter
    /// uses: what is on screen is the end of a conversation, not a window of time.
    pub fn tail(&self, board: &BoardKey, item: &str, limit: usize) -> Result<Tail> {
        if limit == 0 {
            return Ok(Tail { records: Vec::new(), dropped: 0, complete: false });
        }
        self.read_back(board, item, limit, |records| records.len() >= limit)
    }

    /// Everything from `since` onward, **plus whatever came before it in the same window**.
    ///
    /// Deliberately not a filter. The away-mode digest splits *state* from *news*, and the
    /// state is often older than the cutoff: a permission request asked an hour before the
    /// user walked away is still blocking, and a read that answered only `at >= since` would
    /// report that board as quiet. Use [`Tail::first_after`] to find the boundary.
    ///
    /// Bounded by [`MAX_SINCE_RECORDS`], because "since last week" is otherwise the file.
    pub fn since(&self, board: &BoardKey, item: &str, since: Timestamp) -> Result<Tail> {
        self.read_back(board, item, MAX_SINCE_RECORDS, move |records| {
            // Enough once the window reaches back past the cutoff — one record older than
            // `since` proves nothing newer was missed.
            records.first().is_some_and(|first| first.at < since)
        })
    }

    /// The whole transcript. For an export or a summary, never for the painter.
    pub fn read_all(&self, board: &BoardKey, item: &str) -> Result<Tail> {
        let path = self.path_for(board, item);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Tail::missing());
            }
            Err(error) => return Err(AgentError::file(path.display().to_string(), &error)),
        };
        let mut tail = parse_lines(&bytes, false);
        tail.complete = true;
        Ok(tail)
    }

    /// Removes one agent's transcript, for a node the user deleted.
    ///
    /// Answers `false` when there was nothing there, which is the ordinary case for a node
    /// that never ran — deleting a node must not fail because it never had a transcript.
    /// This is the **only** thing in this module that removes a file, and it removes a
    /// transcript rather than a board: RULE ZERO's trash rule is about `.vellum` files, and
    /// history is explicitly disposable (`docs/07` §4).
    pub fn delete(&self, board: &BoardKey, item: &str) -> Result<bool> {
        let path = self.path_for(board, item);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(AgentError::file(path.display().to_string(), &error)),
        }
    }

    /// Reads backwards from the end of the file until `enough` is satisfied or the start is
    /// reached, then keeps at most `cap` records.
    ///
    /// The window doubles rather than being chosen, so the cost is bounded by what was asked
    /// for rather than by how long the agent has been running.
    fn read_back(
        &self,
        board: &BoardKey,
        item: &str,
        cap: usize,
        enough: impl Fn(&[Record]) -> bool,
    ) -> Result<Tail> {
        self.read_back_within(board, item, cap, MAX_TAIL_WINDOW, enough)
    }

    /// [`Self::read_back`] with the window ceiling supplied, so the bound is an offline test
    /// over a few kilobytes rather than one that needs a transcript nobody has.
    fn read_back_within(
        &self,
        board: &BoardKey,
        item: &str,
        cap: usize,
        ceiling: u64,
        enough: impl Fn(&[Record]) -> bool,
    ) -> Result<Tail> {
        let path = self.path_for(board, item);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Tail::missing());
            }
            Err(error) => return Err(AgentError::file(path.display().to_string(), &error)),
        };
        let length = file
            .metadata()
            .map_err(|error| AgentError::file(path.display().to_string(), &error))?
            .len();

        let mut window = TAIL_WINDOW;
        loop {
            let start = length.saturating_sub(window);
            // One byte *before* the window, so the leading fragment can be dropped exactly:
            // if that byte is a newline the fragment is empty and nothing is lost, and if it
            // is not, the fragment is the tail of a line that began outside the window.
            let read_from = if start > 0 { start - 1 } else { 0 };
            file.seek(SeekFrom::Start(read_from))
                .map_err(|error| AgentError::file(path.display().to_string(), &error))?;
            let mut bytes = Vec::with_capacity((length - read_from) as usize);
            file.read_to_end(&mut bytes)
                .map_err(|error| AgentError::file(path.display().to_string(), &error))?;

            let mut tail = parse_lines(&bytes, start > 0);
            tail.complete = start == 0;
            // ⚠ **The ceiling is what stops "since last week" from being the whole file.**
            // The doubling is bounded by *what was asked for* only when the question can be
            // satisfied: [`Sidecar::since`]'s `enough` is "one record older than the cutoff",
            // and a cutoff older than the oldest record in the file is never satisfied — so
            // the window doubled past the file's length and `read_to_end` brought an
            // overnight transcript into memory whole. `cap` bounds the *records* kept and
            // does nothing about the bytes read to find them.
            //
            // Answering short is correct here rather than a compromise: `complete` is already
            // `false` whenever the window did not reach the start, which is exactly what it
            // means. ⚠ Not *"and every caller reads it"*, which this comment used to say — the
            // node loader does and the away-mode digest does not. [`MAX_TAIL_WINDOW`] has the
            // detail; the correctness of stopping here does not depend on it, but anyone
            // reasoning about what the user is told does.
            if enough(&tail.records) || start == 0 || window >= ceiling {
                if tail.records.len() > cap {
                    tail.records.drain(..tail.records.len() - cap);
                    tail.complete = false;
                }
                return Ok(tail);
            }
            window = window.saturating_mul(2).min(ceiling);
        }
    }
}

/// A transcript held open for a run of appends.
///
/// **Unbuffered on purpose.** A `BufWriter` would batch lines and lose whatever was in the
/// buffer when the process died — which is exactly the case the torn-line reader exists for,
/// except a lost buffer takes *several* records rather than a partial one. One line is one
/// `write_all`, which the OS treats atomically for a small write to a file opened `O_APPEND`.
#[derive(Debug)]
pub struct Appender {
    file: File,
    path: PathBuf,
}

impl Appender {
    /// Writes one record. `at` is the caller's clock; see the module note.
    pub fn write(&mut self, at: Timestamp, event: &TranscriptEvent) -> Result<()> {
        let mut line = serde_json::to_string(&Record { at, event: event.clone() })?;
        debug_assert!(!line.contains('\n'), "a record serialised across two JSONL lines");
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|error| AgentError::file(self.path.display().to_string(), &error))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Parses a byte range of JSONL, dropping a leading fragment and a torn final line.
///
/// `drop_first` is set when the range began mid-file. Everything else is positional: a line
/// that does not parse is counted and skipped, because one bad line in the middle must not
/// cost the ones after it.
fn parse_lines(bytes: &[u8], drop_first: bool) -> Tail {
    if bytes.is_empty() {
        return Tail::missing();
    }

    let mut lines: VecDeque<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    let mut dropped = 0;

    // `split` always yields a final element: empty when the input ends with a newline (every
    // complete line was written with one), and the torn remainder when it does not.
    if let Some(last) = lines.pop_back()
        && !last.is_empty()
    {
        dropped += 1;
    }
    if drop_first {
        lines.pop_front();
    }

    let mut records = Vec::with_capacity(lines.len());
    for line in lines {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Record>(line) {
            Ok(record) => records.push(record),
            Err(_) => dropped += 1,
        }
    }
    Tail { records, dropped, complete: false }
}

/// Makes a string safe as one path component.
///
/// Item ids arrive from the document — a Loro `TreeID`, which renders as `counter@peer` —
/// so in practice nothing here is ever replaced. It is a refusal rather than a preference all
/// the same: this function decides *where in the filesystem* a transcript is written, and an
/// id is the one value here that a board file supplies.
///
/// **Safe by construction, not by inspection.** Only alphanumerics, `_`, `-`, `@` and a
/// *non-doubled* dot survive; everything else becomes `_`. So there is no separator to walk
/// through and no `..` to walk up with, rather than a check afterwards that a later edit
/// could forget to keep. A single dot is allowed because ordinary names carry one and a lone
/// dot cannot traverse anything.
///
/// **When anything was replaced, a short hash of the original is appended.** Without it
/// `1/2` and `1_2` would be one file, and one agent's transcript would silently overwrite
/// another's.
fn safe_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut changed = false;
    for character in raw.chars() {
        // The dot rule is the traversal rule: `..` is the only spelling that walks upward,
        // and refusing the *second* dot is what makes it unwritable.
        let keep = character.is_ascii_alphanumeric()
            || matches!(character, '_' | '-' | '@')
            || (character == '.' && !out.ends_with('.'));
        if keep {
            out.push(character);
        } else {
            out.push('_');
            changed = true;
        }
    }
    // A name of nothing, or of a single dot, names a directory rather than a file.
    if out.is_empty() || out == "." {
        out.push('_');
        changed = true;
    }
    if changed {
        let short = fnv1a(raw.as_bytes()) & 0xffff_ffff;
        out.push('-');
        out.push_str(&format!("{short:08x}"));
    }
    debug_assert!(
        !out.contains("..") && !out.contains('/') && !out.contains('\\'),
        "a sanitised name can still traverse: {out}"
    );
    out
}

/// FNV-1a, 64-bit. See the module note on why this is not BLAKE3.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{RequestId, TurnId, TurnOutcome};

    fn text(body: &str) -> TranscriptEvent {
        TranscriptEvent::Text { text: body.to_owned() }
    }

    fn sidecar() -> (tempfile::TempDir, Sidecar) {
        let directory = tempfile::tempdir().expect("a scratch directory");
        let sidecar = Sidecar::new(directory.path());
        (directory, sidecar)
    }

    /// The whole reason a transcript is not in the document: it must be writable and readable
    /// with no ceremony, and a node that never ran must not look like a failure.
    #[test]
    fn a_transcript_that_was_never_written_reads_as_empty_rather_than_failing() {
        let (_scratch, sidecar) = sidecar();
        let board = BoardKey::from_raw("b");

        let tail = sidecar.tail(&board, "1@2", 50).expect("a missing file is not an error");
        assert!(tail.is_empty());
        assert!(tail.complete, "an absent transcript is completely read");
        assert!(sidecar.read_all(&board, "1@2").unwrap().is_empty());
        assert!(sidecar.since(&board, "1@2", 0).unwrap().is_empty());
        assert!(!sidecar.delete(&board, "1@2").unwrap(), "there was nothing to delete");
    }

    /// The record shape: the event round-trips **and so does the time it was appended**,
    /// which is the only place that time exists — `TranscriptEvent` carries none.
    #[test]
    fn records_round_trip_in_order_and_keep_the_time_they_were_appended() {
        let (_scratch, sidecar) = sidecar();
        let board = BoardKey::from_raw("b");
        let written = vec![
            Record {
                at: 1_770_000_000,
                event: TranscriptEvent::TurnStarted { turn: TurnId(1), prompt: "go".into() },
            },
            Record { at: 1_770_000_004, event: text("first") },
            Record { at: 1_770_000_009, event: text("second") },
            Record {
                at: 1_770_000_030,
                event: TranscriptEvent::TurnEnded {
                    turn: TurnId(1),
                    outcome: TurnOutcome::Completed,
                },
            },
        ];
        let mut appender = sidecar.appender(&board, "1@2").unwrap();
        for record in &written {
            appender.write(record.at, &record.event).unwrap();
        }
        drop(appender);

        let all = sidecar.read_all(&board, "1@2").unwrap();
        assert_eq!(all.records, written);
        assert_eq!(all.dropped, 0);
        assert_eq!(all.events().count(), 4);

        // The shape on disk is the one the digest reads, so it is asserted rather than
        // assumed: a bare event would parse as nothing at all.
        let raw = std::fs::read_to_string(sidecar.path_for(&board, "1@2")).unwrap();
        let first: serde_json::Value =
            serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(first["at"], 1_770_000_000_u64);
        assert_eq!(first["event"]["event"], "turn_started");

        assert!(sidecar.delete(&board, "1@2").unwrap());
    }

    /// **The crash case.** Written as a valid file, then truncated mid-line — which is what a
    /// force quit or a power cut leaves behind. Everything before the tear must come back.
    ///
    /// A/B: with the `pop_back` guard removed, the torn half-line reaches `from_slice`, and
    /// this fails on `dropped` staying 0 while the last record silently vanishes — so the
    /// assertion is about the *recovery*, not merely about the count.
    #[test]
    fn a_torn_final_line_is_dropped_and_everything_before_it_survives() {
        let (_scratch, sidecar) = sidecar();
        let board = BoardKey::from_raw("b");
        let mut appender = sidecar.appender(&board, "1@2").unwrap();
        for index in 0..5 {
            appender.write(1_770_000_000 + index, &text(&format!("line {index}"))).unwrap();
        }
        let path = appender.path().to_path_buf();
        drop(appender);

        // Tear the file: keep every complete line, then half of a sixth.
        let intact = std::fs::metadata(&path).unwrap().len();
        let sixth =
            serde_json::to_string(&Record { at: 1_770_000_005, event: text("line 5") }).unwrap();
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&sixth.as_bytes()[..sixth.len() / 2]).unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() > intact, "the tear was written");

        let all = sidecar.read_all(&board, "1@2").unwrap();
        assert_eq!(all.records.len(), 5, "a torn tail cost a complete record");
        assert_eq!(all.records[4].event, text("line 4"));
        assert_eq!(all.records[4].at, 1_770_000_004, "the timestamp did not survive");
        assert_eq!(all.dropped, 1, "the torn line was not reported");

        // And the file keeps working: the next append lands after the tear, which costs the
        // torn line and nothing else.
        sidecar.append(&board, "1@2", 1_770_000_100, &text("after the crash")).unwrap();
        let after = sidecar.read_all(&board, "1@2").unwrap();
        assert_eq!(after.records.len(), 6);
        assert_eq!(after.records[5].event, text("after the crash"));
    }

    /// A tail is bounded by what was asked for, and it must be the **last** n — the painter
    /// draws the end of a conversation. The file is deliberately larger than the first
    /// window, so the doubling loop is exercised rather than skipped.
    #[test]
    fn a_tail_reads_the_last_records_without_reading_the_file() {
        let (_scratch, sidecar) = sidecar();
        let board = BoardKey::from_raw("b");
        let mut appender = sidecar.appender(&board, "1@2").unwrap();
        for index in 0..2000u64 {
            appender
                .write(1_770_000_000 + index, &text(&format!("{index} {}", "x".repeat(700))))
                .unwrap();
        }
        drop(appender);

        let tail = sidecar.tail(&board, "1@2", 10).unwrap();
        assert_eq!(tail.records.len(), 10);
        assert!(!tail.complete, "a 10-record window is not the whole file");
        assert_eq!(tail.records[9].at, 1_770_001_999, "the tail was not the end of the file");
        assert_eq!(tail.records[0].at, 1_770_001_990, "the tail was not contiguous");

        // 200 records is past the first 64KB window, so this is the doubling loop rather
        // than a single read. Without it the loop is never exercised and a window that
        // stopped short would look correct at every limit the painter happens to use.
        let deeper = sidecar.tail(&board, "1@2", 200).unwrap();
        assert_eq!(deeper.records.len(), 200, "the window did not grow to hold what was asked");
        assert!(!deeper.complete);

        // Asking for more than exists answers everything, and says so.
        let everything = sidecar.tail(&board, "1@2", 5000).unwrap();
        assert_eq!(everything.records.len(), 2000);
        assert!(everything.complete);
    }

    /// **The state-vs-news split.** A permission request asked before the cutoff is still
    /// blocking now, so a time-based read that hard-filtered to `at >= since` would report
    /// this board as all quiet — the exact failure the away-mode digest exists to prevent.
    ///
    /// A/B: with `since` filtering instead of splitting, `records[0]` is the `Text` at 500
    /// and the assertion that the question is still visible fails.
    #[test]
    fn a_read_since_a_cutoff_still_shows_the_state_that_was_already_true() {
        let (_scratch, sidecar) = sidecar();
        let board = BoardKey::from_raw("b");
        let asking = TranscriptEvent::PermissionRequest {
            id: RequestId("p1".into()),
            summary: "write to src/main.rs".into(),
            detail: String::new(),
        };
        let mut appender = sidecar.appender(&board, "1@2").unwrap();
        appender.write(100, &text("long before")).unwrap();
        appender.write(200, &asking).unwrap();
        appender.write(500, &text("while they were away")).unwrap();
        appender.write(600, &text("and again")).unwrap();
        drop(appender);

        let since = sidecar.since(&board, "1@2", 400).unwrap();
        assert!(
            since.records.iter().any(|record| record.event == asking),
            "the question that is still blocking was filtered out"
        );

        // And the caller can tell news from state without a second read.
        let boundary = since.first_after(400);
        assert_eq!(since.records[boundary].event, text("while they were away"));
        assert_eq!(since.records.len() - boundary, 2, "two records arrived after the cutoff");
        assert!(since.records[..boundary].iter().all(|record| record.at < 400));

        // A cutoff before the file starts is the whole file, and one after its end is the
        // state and no news — neither is an error.
        assert_eq!(sidecar.since(&board, "1@2", 0).unwrap().records.len(), 4);
        let nothing_new = sidecar.since(&board, "1@2", 9_000).unwrap();
        assert_eq!(nothing_new.first_after(9_000), nothing_new.records.len());
    }

    /// The window's leading fragment must be dropped **exactly**. Read one byte early and a
    /// window that happens to land on a line boundary loses a whole record — a bug that would
    /// only ever appear at one file size in a few thousand.
    #[test]
    fn a_window_landing_on_a_line_boundary_loses_nothing() {
        let first = serde_json::to_string(&Record { at: 1, event: text("a") }).unwrap();
        let second = serde_json::to_string(&Record { at: 2, event: text("b") }).unwrap();
        let file = format!("{first}\n{second}\n");
        let boundary = first.len();
        assert_eq!(file.as_bytes()[boundary], b'\n');

        // `drop_first` with the byte before the window being the newline itself: the leading
        // fragment is empty, so the second record survives whole.
        let tail = parse_lines(&file.as_bytes()[boundary..], true);
        assert_eq!(tail.records, vec![Record { at: 2, event: text("b") }]);
        assert_eq!(tail.dropped, 0);

        let whole = parse_lines(file.as_bytes(), false);
        assert_eq!(whole.records.len(), 2);
    }

    /// ⚠ **A cutoff older than the file read the whole file into memory.**
    ///
    /// `since`'s stopping condition is *"one record older than the cutoff"*, and a cutoff
    /// older than the oldest record can never satisfy it — so the window doubled past the
    /// file's length and `read_to_end` brought an overnight transcript in whole, inside the
    /// application's own process. `MAX_SINCE_RECORDS` looks like the bound and is not: it caps
    /// the records *kept* and says nothing about the bytes read to find them.
    ///
    /// Driven through the ceiling rather than the constant, so this is a few kilobytes of
    /// fixture instead of a transcript nobody has. The assertion is that the read **stops**
    /// and says it was short — answering `complete: false` is what that field already means.
    /// (What *reads* that answer is a shorter list than it looks: see [`MAX_TAIL_WINDOW`].)
    #[test]
    fn a_cutoff_older_than_the_file_stops_at_the_window_ceiling() {
        let (_scratch, sidecar) = sidecar();
        let board = BoardKey::from_raw("b");

        let mut appender = sidecar.appender(&board, "1@2").unwrap();
        let padding = "x".repeat(1_000);
        for index in 0..400u64 {
            // Long records, so a few hundred of them comfortably outrun the ceiling below —
            // which must sit *above* `TAIL_WINDOW`, or the loop never doubles and the clamp
            // on the doubling is not what is being tested.
            appender.write(1_000 + index, &text(&padding)).unwrap();
        }
        drop(appender);
        let length = std::fs::metadata(sidecar.path_for(&board, "1@2")).unwrap().len();
        assert!(length > 256 * 1024, "the fixture is too small to outrun a ceiling: {length}");

        // A cutoff before the file begins: nothing can ever be "older than the cutoff", so
        // this is the question that used to walk the doubling all the way to the file.
        let never_satisfied = |records: &[Record]| records.first().is_some_and(|r| r.at < 1);
        let stopped = sidecar
            .read_back_within(&board, "1@2", MAX_SINCE_RECORDS, 128 * 1024, never_satisfied)
            .unwrap();
        assert!(!stopped.complete, "a read that stopped short claimed to be complete");
        assert!(!stopped.records.is_empty(), "the ceiling refused everything rather than bounding");
        assert!(
            stopped.records.len() < 400,
            "the ceiling read the whole file anyway: {} records",
            stopped.records.len()
        );
        // The **end** is what is kept, which is what a tail read is for.
        assert_eq!(stopped.records.last().unwrap().at, 1_399);

        // The other half: a ceiling above the file still answers completely, so this is a
        // bound rather than a behaviour change.
        let whole = sidecar
            .read_back_within(&board, "1@2", MAX_SINCE_RECORDS, 64 * 1024 * 1024, never_satisfied)
            .unwrap();
        assert!(whole.complete, "a file that fits inside the ceiling must read whole");
        assert_eq!(whole.records.len(), 400);
    }

    /// Two boards must never share a transcript, and one board must keep its own across a
    /// relative path and an absolute one.
    #[test]
    fn a_board_key_follows_the_board_and_separates_two_of_them() {
        let scratch = tempfile::tempdir().unwrap();
        let one = scratch.path().join("one.vellum");
        let two = scratch.path().join("two.vellum");
        std::fs::write(&one, b"").unwrap();
        std::fs::write(&two, b"").unwrap();

        assert_ne!(BoardKey::for_board(&one), BoardKey::for_board(&two));
        // The same file reached two ways is one key — canonicalisation, not string equality.
        let round_about = scratch.path().join("./one.vellum");
        assert_eq!(BoardKey::for_board(&one), BoardKey::for_board(round_about));

        // A board that is not on disk still gets a key rather than an error.
        let gone = BoardKey::for_board(scratch.path().join("deleted.vellum"));
        assert_eq!(gone.as_str().len(), 16);
    }

    /// An item id decides a filename, so it must not be able to decide a *directory*. The
    /// hash suffix is what stops the sanitised forms of two different ids colliding.
    #[test]
    fn an_id_cannot_escape_its_directory_and_two_sanitised_ids_stay_apart() {
        let (_scratch, sidecar) = sidecar();
        let board = BoardKey::from_raw("b");

        for hostile in ["../../etc/passwd", "..", "../sibling", "a/../../b", ".", "/etc/passwd"] {
            let path = sidecar.path_for(&board, hostile);
            assert!(
                path.starts_with(sidecar.root().join("b")),
                "`{hostile}` escaped its board's directory: {}",
                path.display()
            );
            // The filename is one component with nothing to traverse in it — asserted on
            // the *name*, because a `..` that the join happened to normalise away today is
            // still a traversal the next platform performs.
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(!name.contains(".."), "`{hostile}` kept a traversal: {name}");
            assert!(!name.contains('/') && !name.contains('\\'), "{name}");
            assert_eq!(path.components().count(), sidecar.root().components().count() + 2);
        }

        // Two ids that sanitise to the same characters must still be two files, or one
        // agent's transcript silently overwrites another's.
        assert_ne!(safe_name("1/2"), safe_name("1_2"));
        assert_ne!(safe_name(".."), safe_name("._"));
        // An ordinary Loro id is left exactly as it is — the sanitiser must not rename every
        // transcript on the machine.
        assert_eq!(safe_name("42@7"), "42@7");
    }
}
