//! Sending every board on this Mac to the server the person signed in to, off the frame loop.
//!
//! *"i want to enter my account and the server link and then from that point on my
//! information on the mac and on the server will sync"*. `crate::sync` is the *steady state*
//! of that sentence: one board, one round trip, every few seconds. This module is the **first
//! run** — the one that has to get 45 boards and 3,458 pictures onto a server that has never
//! heard of any of them, and then get out of the way.
//!
//! # The wire
//!
//! ```text
//! GET  {base}/api/v1/boards                 -> [{"id":…,"title":…,"items":…,"modified":…}]
//! POST {base}/api/v1/boards                 body: the name as plain UTF-8 text
//!                                           -> {"id":…,"title":…}
//! POST {base}/api/v1/blobs/missing          content-type: application/json
//!                                           body: ["<64 hex>", …] -> the ones it lacks
//! POST {base}/api/v1/blobs/{hash}           content-type: application/octet-stream
//!                                           body: the raw bytes
//! POST {base}/api/v1/boards/{id}/sync       the frame `crate::sync` already speaks
//! ```
//!
//! ⚠ **The client names the hash and the server checks it.** `BlobStore::put` computes the
//! hash itself, so a claimed hash is never trusted — it is compared, and a body that hashes
//! to something else is refused with 400. Naming it in the path is what lets the server
//! answer *"already here"* before it reads a byte, and it is what makes the `missing` probe
//! and the upload agree about what a blob is called.
//!
//! ⚠ **`content-type: application/octet-stream` is not decoration.** It is off the CORS
//! safelist, so a page on another site cannot forge this POST — the same argument
//! `crate::signin` makes about `application/json`, and it matters more here: velmd may never
//! remove a blob, so an unbounded write route is exactly the one that must not be forgeable
//! from a page the owner merely visits.
//!
//! # Why this module never names an `Editor`
//!
//! The worker opens its own [`vellum_store::BoardDb`], loads its own [`vellum_doc::Board`],
//! exports it and drops it, all on its own thread. What crosses the channel is `Vec<u8>`,
//! `String`, `PathBuf` and numbers. `crate::sync`'s header carries the long version of the
//! reason; the short one is that a `Board` on a channel is a second live copy of a document
//! the person may be editing, with no rule about which one wins.
//!
//! # One request at a time, and that is a hard requirement rather than politeness
//!
//! `velmd`'s `MAX_CONNECTIONS` is **16 for the whole server**, refused rather than queued,
//! and every response carries `Connection: close`. A client that took even four slots would
//! lock the owner's own browser out of the boards page for the length of a 1.3 GB run. So
//! there is one worker thread, and it has one request out at any moment, by construction.
//!
//! # Idempotence, which is what makes "press Send again" the whole recovery story
//!
//! This is **not** a resumable transfer and it does not pretend to be one. It is a job made
//! of thousands of small steps, each safe to repeat:
//!
//! - A blob is content addressed, so sending it twice stores it once.
//! - A document post *merges*. Loro is a CRDT, so the same updates applied twice give the
//!   same board.
//! - The board's id is recorded in [`RemoteIds`], so a second run adopts rather than creates.
//! - `POST /api/v1/boards` is the one step that is **not** idempotent — a retry makes a
//!   second board — so it is never retried.
//!
//! # RULE ZERO
//!
//! **The push opens every board read only and writes no board file, ever.** It calls
//! `BoardDb::open` and `BoardDb::load`, and it calls no `save`, no `compact`, no `restore`,
//! no `set_thumbnail`, and it starts no `Autosave`. `BoardDb`'s `Drop` gates its one write on
//! having written, so a read-only handle writes nothing on the way out. The precedent is
//! already running: `Library::rescan` opens every `.vellum` in the folder at launch.
//!
//! The one durable local consequence of the whole feature is `remote-boards.json`, which
//! holds three strings per board and no board content.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

use vellum_doc::{Board, Item, ItemKind, Version};
use vellum_store::{BlobStore, BoardDb, Hash};

/// Named so a server log can tell a first run apart from the steady-state sync.
const USER_AGENT: &str = concat!("Velm/", env!("CARGO_PKG_VERSION"), " (push)");

/// Long enough for a home router waking a sleeping server, short enough that a wrong address
/// is reported rather than hung on. The same ten seconds `crate::sync` uses.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The deadline for one request, and it is **five minutes rather than `crate::sync`'s sixty
/// seconds** on purpose.
///
/// That module's own doc says a global deadline is the right shape for a bounded round trip
/// and the wrong shape for a long transfer: it cuts off an answer that was arriving
/// correctly. The largest blob measured on this user's store is 23.0 MB, which at 100 KB/s
/// takes about four minutes. A sixty second deadline would fail it every time on a slow link
/// and report the server as gone.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// The largest blob this will offer to a server, in bytes.
///
/// It matches the upload route's own cap. Anything larger is counted, named in the log with
/// its hash and its size, and skipped: it does **not** hold its board back and it does not
/// stop the run, because a picture that will not fit is not a reason to leave 44 boards on
/// this Mac.
///
/// Measured on the real store: 3,458 blobs, the largest 23.0 MB, three over 8 MiB. So this
/// cap refuses nothing the user actually has, which is the assertion below.
const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

/// The three largest blobs in this user's store are 22.9 MB, 20.5 MB and 12.5 MB. A cap that
/// refused them would silently ship a library of grey boxes, so it is a compile error rather
/// than a comment.
const _: () = assert!(
    MAX_BLOB_BYTES >= 24 * 1024 * 1024,
    "the cap must clear the largest blob measured on the real store"
);

/// How many hashes go in one `missing` probe.
///
/// 4,096 hashes as a JSON array of 64-character strings is about 274 KB. The route's own
/// manifest cap is 512 KiB, so one batch fits with room to spare and this user's whole store
/// is one probe per board rather than one request per picture. A client with more batches,
/// which costs one extra request per 4,096 blobs.
const HASHES_PER_PROBE: usize = 4096;

/// The most of an answer that is read. Every answer this module reads is a board list, a
/// short JSON object, or a list of hashes; a wrong address can answer with anything at all,
/// and this is a bound on *that*, on a machine that has run out of memory twice.
const MAX_ANSWER_BYTES: u64 = 4 * 1024 * 1024;

/// How long to wait before a second attempt at opening a board file.
///
/// `BoardDb::open` sets no `busy_timeout`, so a checkpoint on a board the app has open can
/// answer `SQLITE_BUSY` to this reader. One retry, then the board is failed for this run and
/// the run carries on. Adding a `busy_timeout` pragma in `vellum-store` would touch every
/// connection to every board file and is a larger change than this feature needs.
const BUSY_RETRY: Duration = Duration::from_millis(250);

/// The waits between attempts at one request. Two retries, then the item is counted and
/// named in the log.
const RETRY_WAITS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(4)];

/// How long the worker sleeps before it looks at the stop flag again.
///
/// The backoff has to be interruptible or Stop would take four seconds to answer and a quit
/// would take four seconds to finish. `crate::sync` puts its backoff on the *main* thread for
/// the same reason; this job is one shot, so it sleeps here and slices the sleep instead.
const SLEEP_SLICE: Duration = Duration::from_millis(100);

// ----- the plan ----------------------------------------------------------------------

/// Everything the worker needs, built on the main thread before the spawn.
///
/// ⚠ **No derived `Debug`:** it holds a [`crate::sync::Credential`]. The rule
/// `crate::sync::Credential` and `crate::options`' `OnceLock` both exist to keep.
pub struct PushPlan {
    /// The origin, with or without a trailing slash.
    pub server: String,
    pub credential: crate::sync::Credential,
    /// The shared, content-addressed asset directory.
    pub blobs: PathBuf,
    /// Every board to send, in the order the library gave them. **Recently deleted is
    /// already filtered out by the caller**: the server has no trash, and a board restored
    /// later goes up on the next run.
    pub boards: Vec<PlanBoard>,
}

/// One board in a [`PushPlan`].
#[derive(Debug, Clone)]
pub struct PlanBoard {
    pub path: PathBuf,
    /// The title as the library index has it, which is what [`safe_name`] is derived from.
    pub title: String,
    /// The id this board was given on **this** server by an earlier run, if there was one.
    pub known_id: Option<String>,
}

// ----- what crosses back -------------------------------------------------------------

/// One thing the worker has done. Carries `String`, `PathBuf` and numbers, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushEvent {
    /// Starting this board. `done` is how many are finished, so the page reads
    /// *"Sending board 12 of 45."*
    Board { done: u32, total: u32, title: String },
    /// Progress inside the current board's pictures.
    Blobs { done: u32, total: u32 },
    /// The server named this board. **The main thread writes this to [`RemoteIds`] on the
    /// frame it lands**, so a quit loses at most the record for the board in flight.
    Recorded { board: PathBuf, id: String },
    /// One board did not go. The run carries on; the next run picks it up.
    BoardFailed { title: String, why: String },
    /// The run is over, whatever the reason.
    Finished(PushSummary),
}

/// What the whole run came to, for the one sentence the Account page shows afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PushSummary {
    pub boards_sent: u32,
    pub boards_failed: u32,
    pub boards_total: u32,
    pub blobs_sent: u32,
    /// Pictures larger than the server's cap. Counted, never fatal.
    pub blobs_too_big: u32,
    pub bytes: u64,
    /// The person pressed Stop.
    pub stopped: bool,
    /// A 401 or a 403 arrived. The session has gone and every later request would fail the
    /// same way, so the run stopped where it was.
    pub session_lost: bool,
    /// The server could not be reached at all.
    pub unreachable: bool,
    /// The worker died without saying so. Only reachable when the thread failed to start.
    pub worker_lost: bool,
}

/// A run in flight, held on the main thread.
///
/// Dropping it stops the worker at its next `send`, which is `crate::signin`'s precedent.
/// [`Push::stop`] is the other half and both are needed: Cancel wants a summary afterwards,
/// a quit does not.
pub struct Push {
    inbound: Receiver<PushEvent>,
    stop: Arc<AtomicBool>,
    /// Whether [`PushEvent::Finished`] has already been handed out, so a second drain on a
    /// dead channel reports nothing rather than inventing a second summary.
    finished: bool,
}

impl std::fmt::Debug for Push {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Push").field("finished", &self.finished).finish()
    }
}

impl Push {
    /// Everything that has come back since the last call. Never blocks.
    ///
    /// ⚠ **Capped at 64 events per frame.** A board with no pictures finishes in one request,
    /// so a fast server can put a hundred events in the channel between two frames, and a
    /// drain that took them all would make the frame that noticed as long as the ones it
    /// stood in for. Whatever is left waits one frame, which nobody can see.
    pub fn drain(&mut self) -> Vec<PushEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        for _ in 0..64 {
            match self.inbound.try_recv() {
                Ok(event) => {
                    if matches!(event, PushEvent::Finished(_)) {
                        self.finished = true;
                    }
                    let done = self.finished;
                    out.push(event);
                    if done {
                        return out;
                    }
                }
                Err(TryRecvError::Empty) => return out,
                Err(TryRecvError::Disconnected) => {
                    // The worker never started, or it went away without a summary. Without
                    // this arm the page would sit on *Sending board 1 of 45* until the app
                    // was restarted.
                    self.finished = true;
                    out.push(PushEvent::Finished(PushSummary {
                        worker_lost: true,
                        ..PushSummary::default()
                    }));
                    return out;
                }
            }
        }
        out
    }

    /// Asks the worker to stop.
    ///
    /// The flag is read **between items**: after each blob and after each board, never inside
    /// one request. So Stop takes effect within one picture, not within one board, and the
    /// sentence on the page says exactly that. A board that was stopped half way is harmless:
    /// it exists on the server, its id is recorded, and the next run finishes it.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether the summary has already been handed out.
    pub const fn is_finished(&self) -> bool {
        self.finished
    }
}

/// The main thread's whole half of the feature, in one field on `ActiveState`.
///
/// One type rather than three fields, so the application struct that owns it gains one line
/// rather than three and so nothing can hold a run without also holding the mapping that
/// makes the run idempotent.
#[derive(Debug, Default)]
pub struct PushState {
    /// A run in flight. `None` almost always, and one `Option` test per frame when it is.
    pub(crate) running: Option<Push>,
    /// The id mapping, opened on the first press and kept. A run of 45 boards reads the file
    /// once and writes it once per board.
    pub(crate) ids: Option<RemoteIds>,
    /// What the Account page draws while a run is going.
    pub(crate) progress: Option<vellum_ui::UploadProgress>,
}

impl PushState {
    /// The mapping, opening it from the boards folder the first time it is asked for.
    pub(crate) fn ids(&mut self, root: &Path) -> &mut RemoteIds {
        self.ids.get_or_insert_with(|| RemoteIds::open(root))
    }
}

/// Starts one run. Never blocks and never touches a socket on this thread.
pub fn start(plan: PushPlan) -> Push {
    let (events, inbound) = channel::<PushEvent>();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);

    let spawned = std::thread::Builder::new().name("velm-push".to_owned()).spawn(move || {
        run(plan, &events, &worker_stop);
    });
    if let Err(error) = spawned {
        log::warn!("push: the worker would not start ({error})");
        // `events` was moved into the closure that never ran, so the channel is already
        // dead and `drain` answers through its `Disconnected` arm on the next frame.
    }

    Push { inbound, stop, finished: false }
}

// ----- where the ids are kept --------------------------------------------------------

/// One board's id on one server.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct RemoteBoard {
    board: PathBuf,
    server: String,
    id: String,
}

/// The mapping from a board file to the id the server gave it, keyed by `(board, server)`.
///
/// # Why its own file rather than a key in `library.json`
///
/// `Filing` degrades an unknown key to *absent* and the next `persist` writes the struct
/// back **without it**. That is the right behaviour for a star and the wrong behaviour for
/// this: one launch of an older build would erase every id on the machine, and the next push
/// would create 45 duplicate boards that no route can delete. A separate file is immune —
/// an older build never opens it, so it cannot rewrite it.
///
/// It lives beside `library.json` in the boards folder, so it travels with the library and
/// `Library::open`'s own `create_dir_all` has already made the directory.
///
/// # What happens when it is missing or stale
///
/// - **Missing**, which is every first run and every fresh machine: [`decide_id`] falls
///   through to the stem rule and then to *create*, and the ids are written as they arrive.
///   The cost of losing this file is one extra run, never a board.
/// - **Stale**, meaning it names an id the server no longer lists: the board was deleted on
///   the server, so [`decide_id`] falls through to *create* and the record is overwritten.
///   An id is never guessed and never re-used blind.
#[derive(Debug, Default)]
pub struct RemoteIds {
    file: PathBuf,
    records: Vec<RemoteBoard>,
}

impl RemoteIds {
    /// The file name beside `library.json`.
    const FILE: &'static str = "remote-boards.json";

    /// Reads the mapping rooted at the boards folder.
    ///
    /// Never fails: a file that will not parse is logged and replaced by an empty mapping,
    /// which costs one more run of the push. Refusing to start would cost the feature.
    pub fn open(root: &Path) -> Self {
        let file = root.join(Self::FILE);
        let records = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                log::warn!("{} could not be read: {error}", file.display());
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self { file, records }
    }

    /// The id this board has on `server`, if one was recorded.
    pub fn id_for(&self, board: &Path, server: &str) -> Option<&str> {
        self.records
            .iter()
            .find(|record| record.board == board && record.server == server)
            .map(|record| record.id.as_str())
    }

    /// Records an id. Replaces rather than appends, so a second run cannot double the file.
    ///
    /// Keyed by `(board, server)`, so signing in to a second server does not overwrite the
    /// first server's id for the same board.
    pub fn record(&mut self, board: &Path, server: &str, id: &str) {
        self.records.retain(|record| !(record.board == board && record.server == server));
        self.records.push(RemoteBoard {
            board: board.to_path_buf(),
            server: server.to_owned(),
            id: id.to_owned(),
        });
        self.persist();
    }

    /// Writes the file. Failure is logged and not propagated, exactly as `Library::persist`
    /// does it: losing a record costs one more run and no board.
    fn persist(&self) {
        match serde_json::to_vec_pretty(&self.records) {
            Ok(bytes) => {
                if let Err(error) = std::fs::write(&self.file, bytes) {
                    log::warn!("writing {}: {error}", self.file.display());
                }
            }
            Err(error) => log::warn!("encoding the remote board ids: {error}"),
        }
    }
}

// ----- the id decision, which is the safety of the whole feature ---------------------

/// One row of `GET /api/v1/boards`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Row {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub items: u64,
}

/// Which board on the server this local board is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// This id. It is already on the server and it is this board.
    Use(String),
    /// Nothing on the server can safely be claimed as this board. Make a new one and read
    /// the id out of the answer.
    Create,
}

/// Whether a stem is one `slug` could have produced from a **different** title.
///
/// # This is the finding the whole feature turns on
///
/// `Library::free_path` names a file by `slug(title)`, and `manage.rs` runs the same `slug`
/// on the server. Two properties of that function make a stem an unsafe name to match on:
///
/// - It answers `"board"` for **any** title with no ASCII alphanumerics in it, and this
///   user's library already contains a file with that stem. Two unrelated boards, one on
///   each side, can both be called `board`.
/// - A collision appends `-2`, `-3`, and the two sides count different things:
///   `free_path` counts files in this folder, `free_stem` counts files in the server's. So a
///   local `-2` and a server `-2` are not the same board and there is no way to tell from
///   the name.
///
/// A wrong answer here posts a whole document into a stranger's board, which merges and does
/// not undo, across 45 irreplaceable files. So these stems are refused outright and a new
/// board is created instead. The cost of the refusal is one extra board on the server that a
/// person can delete; the cost of the wrong adoption is not recoverable.
/// **The title is what tells a suffix from a name, and without it this refuses too much.**
///
/// A trailing `-2` is ambiguous only because `free_path` *appended* it to a stem that was
/// already taken. When it did, the stem no longer matches the slug of its own title: a board
/// titled *Notes* whose file had to become `notes-2` has `slug("Notes") == "notes"`, and the
/// mismatch is the fingerprint of an appended counter.
///
/// A board genuinely titled *Alfa 1999* slugs to `alfa-1999` exactly, so the digits are part
/// of its name and there is nothing to be confused about. Refusing that one anyway was the
/// first version of this function, and it is wrong in a way that is quiet: every board named
/// after a year, a version or a model number would be re-created on the server on every run,
/// for ever, and the person would see duplicates appear and have no idea why.
///
/// `board` and `untitled` stay refused whatever their title says, because [`crate::library::slug`]
/// answers `board` for **any** title with no ASCII alphanumerics in it. There the stem does
/// match the slug, and the match means nothing: a hundred unrelated titles produce it.
fn stem_is_ambiguous(stem: &str, title: &str) -> bool {
    if stem == "board" || stem == "untitled" {
        return true;
    }
    // A trailing `-2`, `-17`, `-999`. Walked backwards over the bytes rather than split on
    // the last dash, so `a-b-2` is caught and `a-b` is not.
    let bytes = stem.as_bytes();
    let mut at = bytes.len();
    while at > 0 && bytes[at - 1].is_ascii_digit() {
        at -= 1;
    }
    let has_counter = at < bytes.len() && at > 0 && bytes[at - 1] == b'-';
    // The slug of the title, not the title: `slug` is what put the stem there in the first
    // place (`Library::free_path`), so this compares like with like.
    has_counter && crate::library::slug(title) != stem
}

/// Which server board this local board should be sent to.
///
/// Pure, so the rule that keeps 45 boards from merging into each other is a unit test rather
/// than a claim. In order:
///
/// 1. **The record.** An id [`RemoteIds`] holds for this `(board, server)`, but **only while
///    the server still lists it**. A recorded id that has gone means the board was deleted
///    there, so it falls through and the record is overwritten.
/// 2. **The stem, and never the stem alone.** A row whose id equals the file stem byte for
///    byte is adopted **only when** the row's title also equals [`safe_name`] of the local
///    title, and **never** when the stem is one [`stem_is_ambiguous`] refuses.
/// 3. **Create.** Anything else. An id is never guessed.
///
/// ⚠ **The comparison is against `safe_name(title)` and not against `title`.** The server
/// stores what its own `clean_name` produced, and this client sends what `safe_name`
/// produced, so a title holding a slash, a backslash, `..` or a control character never
/// matches its own row when the raw string is compared. Comparing the raw title would make
/// rule 2 refuse a board it should adopt, which re-creates it on every run.
pub fn decide_id(known: Option<&str>, stem: &str, title: &str, rows: &[Row]) -> Decision {
    if let Some(known) = known
        && rows.iter().any(|row| row.id == known)
    {
        return Decision::Use(known.to_owned());
    }
    if stem_is_ambiguous(stem, title) {
        return Decision::Create;
    }
    let wanted = safe_name(title);
    if rows.iter().any(|row| row.id == stem && row.title == wanted) {
        return Decision::Use(stem.to_owned());
    }
    Decision::Create
}

/// The most characters a board name may have. `manage::clean_name`'s own cap.
const MAX_NAME: usize = 200;

/// What `Untitled` is called when a title has nothing left in it.
const UNTITLED: &str = "Untitled board";

/// A board title as the create route will store it.
///
/// The Rust twin of `manage::clean_name`, run **here, before the request**, for two reasons
/// that pull the same way. Miro titles contain slashes, and a 400 from the create route would
/// reach the person as a network failure. And the stored title is what [`decide_id`] compares
/// a row against, so the two have to be derived by the same rules or a board is re-created on
/// every run.
///
/// The refusals are the server's, in the server's order: control characters, `/`, `\`, `..`,
/// a 200 character cap, and an empty result falls back to a fixed name rather than being
/// refused, because a button somebody pressed is not an API call.
///
/// The name only **seeds the id**. The real title travels inside the document and lands with
/// the sync that follows.
pub fn safe_name(title: &str) -> String {
    let mut kept = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_control() || ch == '/' || ch == '\\' {
            continue;
        }
        kept.push(ch);
    }
    // `..` is removed rather than replaced, and repeatedly: `...` leaves `.`, and `....`
    // leaves nothing. Both are ASCII, so the byte index `find` gives is a character boundary.
    while let Some(at) = kept.find("..") {
        kept.replace_range(at..at + 2, "");
    }
    // Trimmed **after** the cap as well as before it. Cutting at 200 characters can leave a
    // trailing space, the server trims it, and the two would then disagree about the stored
    // title for ever.
    let capped: String = kept.trim().chars().take(MAX_NAME).collect();
    let name = capped.trim();
    if name.is_empty() { UNTITLED.to_owned() } else { name.to_owned() }
}

// ----- which blobs a board references ------------------------------------------------

/// Every asset a board's items name, parsed and deduplicated.
///
/// ⚠ **The match is exhaustive with no `_` arm, and that is the whole design of this
/// function.** `crates/velmd/src/migrate.rs` promises the same thing in a doc comment and
/// has a `_ => {}` arm anyway, which is why `velmd blobs` silently skips every PDF and every
/// embed poster frame. A `_` here would do the same the first time somebody adds a kind that
/// carries a picture, and nothing would fail — the boards would simply arrive with holes in
/// them. Listing all nineteen makes that a compile error.
///
/// The eight kinds carrying an opaque `model`, `spec` or `board` string are listed by name
/// with nothing taken out of them. **That is a recorded decision, not an oversight**: those
/// tokens are parsed by their own crates and are not provably free of asset ids from
/// `item.rs`. If one of them ever carries a picture, this is the function that has to learn
/// about it.
///
/// Each id is parsed as a [`Hash`] and dropped when it does not parse. An `asset_id` is board
/// content and `POST /sync` lets anybody put `"../../../tmp/x"` there, so the parse is the
/// whole defence and it is applied before the string reaches a URL.
fn hashes_of(items: &[Item]) -> BTreeSet<Hash> {
    /// One asset id, parsed and kept, or dropped. A free function rather than a closure so
    /// its lifetime is inferred per call rather than once for the whole loop.
    fn take(out: &mut BTreeSet<Hash>, id: Option<&str>) {
        if let Some(id) = id
            && let Ok(hash) = id.parse::<Hash>()
        {
            out.insert(hash);
        }
    }

    let mut out = BTreeSet::new();
    for item in items {
        match &item.kind {
            ItemKind::Image { asset_id, .. } => take(&mut out, Some(asset_id.as_str())),
            ItemKind::Document { asset_id, .. } => take(&mut out, Some(asset_id.as_str())),
            ItemKind::LinkPreview { thumbnail, favicon, .. } => {
                take(&mut out, thumbnail.as_deref());
                take(&mut out, favicon.as_deref());
            }
            ItemKind::Embed { thumbnail, favicon, .. } => {
                take(&mut out, thumbnail.as_deref());
                take(&mut out, favicon.as_deref());
            }
            // No pictures. Listed one by one so a twentieth kind cannot join them silently.
            ItemKind::Sticky { .. }
            | ItemKind::Text { .. }
            | ItemKind::Ink { .. }
            | ItemKind::Connector { .. }
            | ItemKind::Frame { .. }
            | ItemKind::Shape { .. }
            | ItemKind::Group => {}
            // Opaque model or spec strings. See the warning above: nothing is taken out of
            // them, and that is a decision somebody has to revisit deliberately.
            ItemKind::Table { .. }
            | ItemKind::Chart { .. }
            | ItemKind::MindMap { .. }
            | ItemKind::Kanban { .. }
            | ItemKind::Agent { .. }
            | ItemKind::FileTree { .. }
            | ItemKind::AgentNote { .. }
            | ItemKind::Browser { .. } => {}
        }
    }
    out
}

/// [`hashes_of`] over a loaded board.
///
/// ⚠ **The board thumbnail is deliberately not here.** No route sets the server's thumbnail:
/// the browser reads it from `BoardIndex`, which `velmd` computes itself, and a grep of
/// `crates/velmd/src` finds no writer at all. Sending it would upload bytes nothing will
/// ever ask for.
pub(crate) fn blob_hashes(board: &Board) -> BTreeSet<Hash> {
    match board.items() {
        Ok(items) => hashes_of(&items),
        Err(error) => {
            log::warn!("push: reading the items of a board failed ({error})");
            BTreeSet::new()
        }
    }
}

// ----- the worker --------------------------------------------------------------------

/// The HTTP agent for the run. One, so connections are pooled across every request.
///
/// Its own agent rather than `crate::sync::agent`, and the difference is the deadline: that
/// one is sixty seconds, which a 23 MB blob on a slow link cannot finish inside. Everything
/// else is the same on purpose — the same `http_status_as_error(false)`, so a 415 arrives as
/// a *response* carrying velmd's own sentence rather than as a transport error reading "415".
fn upload_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into()
}

/// One answer, read.
struct Answer {
    status: u16,
    body: String,
}

impl Answer {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 401 or 403. The session has gone and every later request would fail the same way.
    const fn session_lost(&self) -> bool {
        self.status == 401 || self.status == 403
    }

    /// One sentence fit for a log line, with the status in front of it.
    fn detail(&self) -> String {
        let quoted: String = self.body.trim().chars().take(200).collect();
        if quoted.is_empty() {
            format!("HTTP {}", self.status)
        } else {
            format!("HTTP {}: {quoted}", self.status)
        }
    }
}

/// Everything one run holds. A struct rather than a dozen parameters, because clippy counts
/// `self` toward its threshold of seven and every method below would otherwise be past it.
struct Worker<'a> {
    http: ureq::Agent,
    /// The origin with no trailing slash, so `{base}/api/v1/…` never doubles a separator.
    base: String,
    credential: crate::sync::Credential,
    blobs: Option<BlobStore>,
    events: &'a Sender<PushEvent>,
    stop: &'a AtomicBool,
    /// Hashes this run has already sent or already proved present. One picture on twenty
    /// boards is one upload.
    settled: BTreeSet<Hash>,
    /// Whether the `missing` probe route answered at all. `false` puts the run into the
    /// degraded mode described at [`Worker::missing`].
    probe_works: bool,
    summary: PushSummary,
}

/// The whole run, on the worker thread.
fn run(plan: PushPlan, events: &Sender<PushEvent>, stop: &AtomicBool) {
    let blobs = match BlobStore::open(&plan.blobs) {
        Ok(store) => Some(store),
        Err(error) => {
            // Not fatal. The documents still go up, and the pictures follow on a later run
            // once whatever is wrong with the folder is fixed.
            log::warn!("push: the blob store would not open ({error}); sending documents only");
            None
        }
    };
    let total = u32::try_from(plan.boards.len()).unwrap_or(u32::MAX);
    let mut worker = Worker {
        http: upload_agent(),
        base: plan.server.trim_end_matches('/').to_owned(),
        credential: plan.credential,
        blobs,
        events,
        stop,
        settled: BTreeSet::new(),
        probe_works: true,
        summary: PushSummary { boards_total: total, ..PushSummary::default() },
    };
    worker.go(&plan.boards);
    let _ = events.send(PushEvent::Finished(worker.summary));
}

impl Worker<'_> {
    /// Every board, one at a time, each finished before the next is started.
    ///
    /// Not all blobs and then all documents: a run that stops half way then leaves whole,
    /// correct boards behind it, and *"Sending board 12 of 45."* is a true sentence rather
    /// than a guess at a fraction.
    fn go(&mut self, boards: &[PlanBoard]) {
        let Some(rows) = self.rows() else {
            // A refused credential has already said so. Anything else is *"could not reach
            // your server"*, which is a different sentence and a different thing to do.
            if !self.summary.session_lost {
                self.summary.unreachable = true;
            }
            return;
        };
        let total = self.summary.boards_total;
        for (index, board) in boards.iter().enumerate() {
            if self.stopped() {
                self.summary.stopped = true;
                return;
            }
            let done = u32::try_from(index).unwrap_or(u32::MAX);
            if !self.emit(PushEvent::Board { done, total, title: board.title.clone() }) {
                return;
            }
            match self.one_board(board, &rows) {
                Outcome::Sent => self.summary.boards_sent += 1,
                Outcome::Failed(why) => {
                    self.summary.boards_failed += 1;
                    log::warn!("push: {} did not go ({why})", board.title);
                    let event = PushEvent::BoardFailed { title: board.title.clone(), why };
                    if !self.emit(event) {
                        return;
                    }
                }
                Outcome::Stopped => {
                    self.summary.stopped = true;
                    return;
                }
                Outcome::SessionLost => {
                    self.summary.session_lost = true;
                    return;
                }
            }
        }
    }

    /// One board: decide its id, send its pictures, then send its document.
    ///
    /// **Pictures first, then the document.** A board that syncs before its pictures arrive
    /// draws grey boxes in the browser, and `GET /api/v1/blobs/{hash}` answers 404 until the
    /// next run.
    fn one_board(&mut self, plan: &PlanBoard, rows: &[Row]) -> Outcome {
        let Some(board) = self.load(&plan.path) else {
            return Outcome::Failed("this board would not open".to_owned());
        };
        let stem = plan
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();

        let id = match decide_id(plan.known_id.as_deref(), &stem, &plan.title, rows) {
            Decision::Use(id) => id,
            Decision::Create => match self.create_board(&plan.title) {
                Created::Id(id) => id,
                Created::SessionLost => return Outcome::SessionLost,
                Created::Failed(why) => return Outcome::Failed(why),
            },
        };
        // ⚠ **Recorded before anything else happens**, so a quit one instant after a create
        // still leaves the id on this Mac and the next run adopts rather than duplicating.
        // Adopting by stem is written down too: it turns the next run's rule 2 into rule 1,
        // which is the cheap rule and the one that cannot be ambiguous.
        if !self.emit(PushEvent::Recorded { board: plan.path.clone(), id: id.clone() }) {
            return Outcome::Stopped;
        }

        match self.send_blobs(&board) {
            Outcome::Sent => {}
            other => return other,
        }
        self.push_document(&id, &board)
    }

    /// Opens a board read only and loads its document.
    ///
    /// One retry after [`BUSY_RETRY`], then `None`. `BoardDb::open` sets no `busy_timeout`,
    /// so a checkpoint on a board the app has open can refuse this reader once. It never
    /// panics and never unwraps: `[profile.release]` sets `panic = "abort"`, so a panic here
    /// would kill the application rather than fail one board.
    fn load(&self, path: &Path) -> Option<Board> {
        // ⚠ **Asked before `BoardDb::open`, and this is a RULE ZERO guard rather than an
        // optimisation.** `open` *creates* the file it is pointed at, so a board deleted from
        // Finder between the library's last rescan and this moment would be re-created here
        // as an empty `.vellum` and then pushed to the server as a board with nothing in it.
        // Skipping it costs one line in the log.
        if !path.exists() {
            log::warn!("push: {} is no longer there", path.display());
            return None;
        }
        for attempt in 0..2 {
            if attempt > 0 && !self.wait(BUSY_RETRY) {
                return None;
            }
            match BoardDb::open(path).and_then(|mut db| db.load()) {
                Ok(Some(board)) => return Some(board),
                Ok(None) => {
                    log::warn!("push: {} holds no snapshot", path.display());
                    return None;
                }
                Err(error) => log::warn!("push: opening {} ({error})", path.display()),
            }
        }
        None
    }

    /// `GET {base}/api/v1/boards`, already filtered by what this account may see.
    fn rows(&mut self) -> Option<Vec<Row>> {
        let url = format!("{}/api/v1/boards", self.base);
        let answer = match self.ask(self.http.get(url.as_str())) {
            Ok(answer) => answer,
            Err(why) => {
                log::warn!("push: listing boards ({why})");
                return None;
            }
        };
        if answer.session_lost() {
            self.summary.session_lost = true;
            return None;
        }
        if !answer.ok() {
            log::warn!("push: listing boards ({})", answer.detail());
            return None;
        }
        match serde_json::from_str::<Vec<Row>>(&answer.body) {
            Ok(rows) => Some(rows),
            Err(error) => {
                log::warn!("push: the board list is not a list of boards ({error})");
                None
            }
        }
    }

    /// `POST {base}/api/v1/boards`, body the name as plain UTF-8 text.
    ///
    /// ⚠ **Never retried.** The route is deliberately not idempotent: a retry makes a second
    /// board, and there is no route that can delete one. A create that timed out fails this
    /// board for this run, and the next run adopts it by stem or creates once more, which is
    /// a duplicate a person can delete rather than a merge nobody can undo.
    fn create_board(&mut self, title: &str) -> Created {
        #[derive(serde::Deserialize)]
        struct Made {
            id: String,
        }
        let url = format!("{}/api/v1/boards", self.base);
        let name = safe_name(title);
        let request = self.post(&url).header("content-type", "text/plain");
        let answer = match self.send(request, name.into_bytes()) {
            Ok(answer) => answer,
            Err(why) => return Created::Failed(why),
        };
        if answer.session_lost() {
            return Created::SessionLost;
        }
        if !answer.ok() {
            return Created::Failed(answer.detail());
        }
        match serde_json::from_str::<Made>(&answer.body) {
            Ok(made) if !made.id.is_empty() => Created::Id(made.id),
            Ok(_) => Created::Failed("the server named the new board nothing".to_owned()),
            Err(error) => Created::Failed(format!("the create answer did not parse: {error}")),
        }
    }

    /// Every picture this board references that the server does not already have.
    fn send_blobs(&mut self, board: &Board) -> Outcome {
        let Some(blobs) = self.blobs.clone() else { return Outcome::Sent };
        let wanted: Vec<Hash> =
            blob_hashes(board).into_iter().filter(|hash| !self.settled.contains(hash)).collect();
        if wanted.is_empty() {
            return Outcome::Sent;
        }
        let missing = self.missing(&wanted);
        // Anything the server already has is settled for the rest of the run, so a picture on
        // twenty boards is probed once.
        for hash in &wanted {
            if !missing.contains(hash) {
                self.settled.insert(*hash);
            }
        }

        let total = u32::try_from(missing.len()).unwrap_or(u32::MAX);
        for (index, hash) in missing.iter().enumerate() {
            if self.stopped() {
                return Outcome::Stopped;
            }
            let done = u32::try_from(index).unwrap_or(u32::MAX);
            if !self.emit(PushEvent::Blobs { done, total }) {
                return Outcome::Stopped;
            }
            match self.put_blob(&blobs, hash) {
                Sent::Ok => {
                    self.settled.insert(*hash);
                }
                Sent::Skipped => {
                    // Counted and named in the log, and it does **not** hold the board back.
                    // A picture that will not fit is not a reason to leave a board behind.
                    self.settled.insert(*hash);
                }
                Sent::SessionLost => return Outcome::SessionLost,
            }
        }
        if total > 0 && !self.emit(PushEvent::Blobs { done: total, total }) {
            return Outcome::Stopped;
        }
        Outcome::Sent
    }

    /// Which of these the server lacks, in one request per [`HASHES_PER_PROBE`].
    ///
    /// # The degraded mode, stated rather than hidden
    ///
    /// A server whose `missing` route answers 404 or 405 is one built before this feature.
    /// The run then falls back to one `HEAD /api/v1/blobs/{hash}` per picture, which is
    /// 3,458 requests on this user's store and makes the server read and re-hash every file
    /// it already has, because `HEAD_ONLY` suppresses the body and not the read. It is still
    /// far cheaper than the other degraded mode, which is sending 1.3 GB on every run. The
    /// log says which mode is in use.
    fn missing(&mut self, wanted: &[Hash]) -> Vec<Hash> {
        if !self.probe_works {
            return self.missing_by_head(wanted);
        }
        let url = format!("{}/api/v1/blobs/missing", self.base);
        let mut out = Vec::new();
        for batch in wanted.chunks(HASHES_PER_PROBE) {
            let hexes: Vec<String> = batch.iter().map(|hash| hash.to_hex()).collect();
            let body = match serde_json::to_vec(&hexes) {
                Ok(body) => body,
                Err(error) => {
                    log::warn!("push: encoding the probe failed ({error})");
                    return batch.to_vec();
                }
            };
            let request = self.post(&url).header("content-type", "application/json");
            let answer = match self.send(request, body) {
                Ok(answer) => answer,
                Err(why) => {
                    // A probe that could not be made is not a reason to skip the pictures.
                    // Assume the worst and send them; the server stores each one once.
                    log::warn!("push: probing for missing pictures ({why})");
                    out.extend_from_slice(batch);
                    continue;
                }
            };
            if answer.status == 404 || answer.status == 405 {
                log::warn!(
                    "push: this server has no batch probe, so every picture is asked for one \
                     at a time"
                );
                self.probe_works = false;
                // `wanted` and not `batch`: the fallback covers every hash, including the
                // ones earlier batches already answered for, so whatever `out` holds is
                // replaced rather than added to.
                return self.missing_by_head(wanted);
            }
            if !answer.ok() {
                log::warn!("push: probing for missing pictures ({})", answer.detail());
                out.extend_from_slice(batch);
                continue;
            }
            out.extend(hashes_in(&answer.body));
        }
        out
    }

    /// One `HEAD` per picture. See [`Worker::missing`] for when this runs and what it costs.
    fn missing_by_head(&self, wanted: &[Hash]) -> Vec<Hash> {
        let mut out = Vec::new();
        for hash in wanted {
            if self.stopped() {
                return out;
            }
            let url = format!("{}/api/v1/blobs/{}", self.base, hash.to_hex());
            match self.ask(self.http.head(url.as_str())) {
                // 200 means it is there. Anything else, including a failure to ask, is read
                // as absent: sending a blob the server already has costs bandwidth, and
                // skipping one it lacks costs a grey box on the board.
                Ok(answer) if answer.ok() => {}
                _ => out.push(*hash),
            }
        }
        out
    }

    /// `POST {base}/api/v1/blobs/{hash}`, the raw bytes as the body.
    ///
    /// The hash is read out of the local store with [`BlobStore::get`], which re-hashes on
    /// the way out, so a blob that has rotted on this Mac is named here rather than uploaded
    /// under a name it no longer answers to. The server checks the hash again against the
    /// path, which is what makes a claimed hash a claim and not a fact.
    fn put_blob(&mut self, blobs: &BlobStore, hash: &Hash) -> Sent {
        let size = match blobs.size_of(hash) {
            Ok(Some(size)) => size,
            Ok(None) => {
                log::warn!("push: {hash} is on a board and not in this store");
                return Sent::Skipped;
            }
            Err(error) => {
                log::warn!("push: measuring {hash} ({error})");
                return Sent::Skipped;
            }
        };
        if size > MAX_BLOB_BYTES {
            log::warn!("push: {hash} is {size} bytes, past the {MAX_BLOB_BYTES}-byte ceiling");
            self.summary.blobs_too_big += 1;
            return Sent::Skipped;
        }
        let bytes = match blobs.get(hash) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Sent::Skipped,
            Err(error) => {
                log::warn!("push: reading {hash} ({error})");
                return Sent::Skipped;
            }
        };

        let url = format!("{}/api/v1/blobs/{}", self.base, hash.to_hex());
        for attempt in 0..=RETRY_WAITS.len() {
            let request = self.post(&url).header("content-type", "application/octet-stream");
            let last = attempt == RETRY_WAITS.len();
            match self.send(request, bytes.clone()) {
                Ok(answer) if answer.ok() => {
                    self.summary.blobs_sent += 1;
                    self.summary.bytes = self.summary.bytes.saturating_add(size);
                    return Sent::Ok;
                }
                Ok(answer) if answer.session_lost() => return Sent::SessionLost,
                // Counted as a picture that is too large, never as a board failure.
                Ok(answer) if answer.status == 413 => {
                    log::warn!("push: {hash} is too large for this server ({})", answer.detail());
                    self.summary.blobs_too_big += 1;
                    return Sent::Skipped;
                }
                Ok(answer) if last => {
                    log::warn!("push: {hash} ({})", answer.detail());
                    return Sent::Skipped;
                }
                // A 503 is pressure rather than a failure, and so is anything else that is
                // left: waited out and asked again, then given up on and named in the log.
                Ok(answer) => log::warn!("push: {hash} ({}); trying again", answer.detail()),
                Err(why) if last => {
                    log::warn!("push: {hash} ({why})");
                    return Sent::Skipped;
                }
                Err(why) => log::warn!("push: {hash} ({why}); trying again"),
            }
            if let Some(wait) = RETRY_WAITS.get(attempt)
                && !self.wait(*wait)
            {
                return Sent::Skipped;
            }
        }
        Sent::Skipped
    }

    /// The whole document, as the frame `crate::sync` already speaks.
    ///
    /// `Version::empty()` yields the whole document as updates, which is exactly what a fresh
    /// `Sync` sends on its first round trip. **No server change is needed for this step**, and
    /// `velmd`'s refusal to write into a board that holds no snapshot stays exactly as it is,
    /// because the id decision above guarantees the board exists first.
    fn push_document(&mut self, id: &str, board: &Board) -> Outcome {
        let updates = match board.export_since(&Version::empty()) {
            Ok(updates) => updates,
            Err(error) => return Outcome::Failed(format!("this board would not export: {error}")),
        };
        // Through `crate::sync`'s own builder, so there is exactly one spelling of this URL
        // in the application and the percent-encoding cannot come to disagree with itself.
        let url = crate::sync::endpoint_for(&self.base, id);
        let body = crate::sync::frame(&board.version().encode(), &updates);

        for attempt in 0..=RETRY_WAITS.len() {
            let request = self.post(&url).header("content-type", "application/octet-stream");
            let last = attempt == RETRY_WAITS.len();
            match self.send(request, body.clone()) {
                Ok(answer) if answer.ok() => return Outcome::Sent,
                Ok(answer) if answer.session_lost() => return Outcome::SessionLost,
                Ok(answer) if last => return Outcome::Failed(answer.detail()),
                Ok(answer) => log::warn!("push: {id} ({}); trying again", answer.detail()),
                Err(why) if last => return Outcome::Failed(why),
                Err(why) => log::warn!("push: {id} ({why}); trying again"),
            }
            if let Some(wait) = RETRY_WAITS.get(attempt)
                && !self.wait(*wait)
            {
                return Outcome::Stopped;
            }
        }
        Outcome::Failed("this board would not send".to_owned())
    }

    // ----- the small mechanics -------------------------------------------------------

    /// A `POST` with the credential's one header already on it.
    ///
    /// At most one header, and which one is `crate::sync::auth_header`'s decision rather than
    /// this function's. It is pure and already covered by that module's own tests, so the
    /// promise that a bearer token setup keeps working here is asserted rather than claimed.
    fn post(&self, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        let request = self.http.post(url);
        match crate::sync::auth_header(&self.credential) {
            Some((name, value)) => request.header(name, value),
            None => request,
        }
    }

    /// `GET` or `HEAD`, with the credential, reading the answer as text.
    fn ask(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    ) -> Result<Answer, String> {
        let request = match crate::sync::auth_header(&self.credential) {
            Some((name, value)) => request.header(name, value),
            None => request,
        };
        match request.call() {
            Ok(response) => Ok(read_answer(response)),
            Err(error) => Err(error.to_string()),
        }
    }

    /// One request, on this thread. **One at a time, for the whole run.**
    fn send(
        &self,
        request: ureq::RequestBuilder<ureq::typestate::WithBody>,
        body: Vec<u8>,
    ) -> Result<Answer, String> {
        match request.send(body) {
            Ok(response) => Ok(read_answer(response)),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Whether the person pressed Stop.
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleeps, in slices, and answers `false` when the run should end.
    ///
    /// A flat `sleep` would make Stop take four seconds to answer and would hold a quit open
    /// for the same time. `crate::sync` avoids the question by putting its backoff on the
    /// main thread; this job is one shot, so it sleeps here and checks the flag as it goes.
    fn wait(&self, total: Duration) -> bool {
        let mut left = total;
        while left > Duration::ZERO {
            if self.stopped() {
                return false;
            }
            let slice = left.min(SLEEP_SLICE);
            std::thread::sleep(slice);
            left -= slice;
        }
        !self.stopped()
    }

    /// Sends one event. `false` means the main thread has gone and the run should end.
    fn emit(&self, event: PushEvent) -> bool {
        self.events.send(event).is_ok()
    }
}

/// How one board ended.
enum Outcome {
    Sent,
    Failed(String),
    Stopped,
    SessionLost,
}

/// How one create ended.
enum Created {
    Id(String),
    Failed(String),
    SessionLost,
}

/// How one blob ended. A skip is never a board failure.
enum Sent {
    Ok,
    Skipped,
    SessionLost,
}

/// Reads a response into a status and at most [`MAX_ANSWER_BYTES`] of text.
fn read_answer(response: ureq::http::Response<ureq::Body>) -> Answer {
    let status = response.status().as_u16();
    let mut body = Vec::new();
    let _ = response.into_body().into_reader().take(MAX_ANSWER_BYTES).read_to_end(&mut body);
    // Lossy, and truncated by **characters** wherever it is quoted: a proxy in front of velmd
    // can answer with anything, and slicing a multi-byte character in half aborts the process.
    Answer { status, body: String::from_utf8_lossy(&body).into_owned() }
}

/// Every 64-character hex run in a body, as hashes.
///
/// ⚠ **Deliberately shape blind.** The probe's answer is a list of hashes and the two halves
/// of this feature are written by different hands; this reads a JSON array, a newline list
/// and a comma list identically, because a hash is 64 hex characters and nothing else in any
/// of those framings is. A token that is not exactly 64 hex characters is ignored, so a
/// `{"missing":[…]}` wrapper costs nothing either.
pub(crate) fn hashes_in(body: &str) -> Vec<Hash> {
    let mut out = Vec::new();
    for token in body.split(|ch: char| !ch.is_ascii_hexdigit()) {
        if token.len() == 64
            && let Ok(hash) = token.parse::<Hash>()
        {
            out.push(hash);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{CardMode, NewItem, Placement, StyledText};

    /// A row, so the id tests read as the rule they are checking rather than as struct
    /// literals. Takes arguments, which is the habit `velmd`'s own test scan enforces.
    fn row(id: &str, title: &str, items: u64) -> Row {
        Row { id: id.to_owned(), title: title.to_owned(), items }
    }

    /// **The test that would have caught the trap.**
    ///
    /// `manage.rs`'s `slug` answers `"board"` for any title with no ASCII alphanumerics in
    /// it, and this user's library already holds a file with that stem. Adopting on the stem
    /// would post one board's whole document into another board that merely shares the
    /// fallback name, which merges and does not undo.
    #[test]
    fn the_fallback_stem_is_never_adopted() {
        let rows = vec![row("board", "Some other person's board", 240)];
        assert_eq!(decide_id(None, "board", "Some other person's board", &rows), Decision::Create);
        // Even when the titles agree, which is the tempting case.
        let same = vec![row("board", "Notes", 0)];
        assert_eq!(decide_id(None, "board", "Notes", &same), Decision::Create);
        assert_eq!(decide_id(None, "untitled", "Untitled", &same), Decision::Create);
    }

    /// A local `-2` counts files in this folder and a server `-2` counts files in the
    /// server's, so the two suffixes are not the same board and nothing can tell from the
    /// name. Refused outright.
    #[test]
    fn a_numeric_suffix_is_never_adopted() {
        let rows = vec![row("notes-2", "Notes", 12), row("alfa-1999-156-v6-17", "Alfa", 3)];
        assert_eq!(decide_id(None, "notes-2", "Notes", &rows), Decision::Create);
        assert_eq!(decide_id(None, "alfa-1999-156-v6-17", "Alfa", &rows), Decision::Create);
        // The refusal is about the suffix and not about digits: a stem that merely ends in a
        // digit is still adoptable, or every board named after a year would be re-created.
        let year = vec![row("alfa-1999", "Alfa 1999", 3)];
        assert_eq!(
            decide_id(None, "alfa-1999", "Alfa 1999", &year),
            Decision::Use("alfa-1999".to_owned())
        );
    }

    /// The server stores `clean_name`'s output and this client sends `safe_name`'s, so a
    /// title holding a slash never equals its own row when the raw string is compared.
    ///
    /// Both halves are asserted. The refusal is the safety; the adoption is what stops the
    /// board being re-created on every single run, which is the defect the raw comparison
    /// would have produced.
    #[test]
    fn a_title_with_a_slash_is_compared_after_cleaning() {
        let title = "Engine bay / wiring";
        // A row carrying the raw title is **not** this board: the server cannot have stored
        // that string, so whatever wrote it is something else.
        let raw = vec![row("engine-bay-wiring", title, 40)];
        assert_eq!(decide_id(None, "engine-bay-wiring", title, &raw), Decision::Create);
        // A row carrying what the server would really have stored is adopted.
        let cleaned = vec![row("engine-bay-wiring", &safe_name(title), 40)];
        assert_eq!(
            decide_id(None, "engine-bay-wiring", title, &cleaned),
            Decision::Use("engine-bay-wiring".to_owned())
        );
    }

    /// A row whose title is somebody else's is not this board, however well the stem matches.
    #[test]
    fn a_stem_match_with_a_different_title_creates() {
        let rows = vec![row("notes", "Payroll", 900)];
        assert_eq!(decide_id(None, "notes", "Notes", &rows), Decision::Create);
    }

    /// The record outranks the stem, and a record the server no longer lists is dropped
    /// rather than trusted. An id is never guessed.
    #[test]
    fn the_record_wins_and_a_stale_record_falls_through() {
        let rows = vec![row("notes-from-the-migration", "Notes", 12), row("notes", "Notes", 12)];
        assert_eq!(
            decide_id(Some("notes-from-the-migration"), "notes", "Notes", &rows),
            Decision::Use("notes-from-the-migration".to_owned())
        );
        // The recorded board was deleted on the server. Fall through to the stem rule.
        let gone = vec![row("notes", "Notes", 12)];
        assert_eq!(
            decide_id(Some("notes-from-the-migration"), "notes", "Notes", &gone),
            Decision::Use("notes".to_owned())
        );
        // And with nothing to fall through to, create.
        assert_eq!(
            decide_id(Some("notes-from-the-migration"), "notes", "Notes", &[]),
            Decision::Create
        );
    }

    /// An empty server is the first run, and every board is created.
    #[test]
    fn an_empty_server_creates_everything() {
        assert_eq!(decide_id(None, "notes", "Notes", &[]), Decision::Create);
    }

    /// The Rust twin of `manage::clean_name`. Each refusal is the server's, so a name that
    /// passes here cannot come back as a 400 that reads like a network failure.
    #[test]
    fn a_name_is_cleaned_the_way_the_server_cleans_it() {
        assert_eq!(safe_name(""), UNTITLED);
        assert_eq!(safe_name("   "), UNTITLED);
        assert_eq!(safe_name("a/b"), "ab");
        assert_eq!(safe_name("a\\b"), "ab");
        assert_eq!(safe_name("a..b"), "ab");
        assert_eq!(safe_name("a\u{7}b"), "ab", "a control character is dropped");
        assert_eq!(safe_name("  Notes  "), "Notes");
        assert_eq!(safe_name(&"x".repeat(300)).chars().count(), MAX_NAME);
        // A title that is nothing but refusals still has a name.
        assert_eq!(safe_name("///"), UNTITLED);
        // The cap counts characters, not bytes, exactly as `clean_name` does.
        assert_eq!(safe_name(&"é".repeat(300)).chars().count(), MAX_NAME);
    }

    /// Every kind that carries a picture is collected, and a hostile `asset_id` never
    /// reaches a URL. This is the test that stops the `migrate.rs` defect being written a
    /// second time.
    #[test]
    fn every_asset_bearing_kind_is_collected_and_a_bad_one_is_dropped() {
        let one = Hash::of(b"one").to_hex();
        let two = Hash::of(b"two").to_hex();
        let three = Hash::of(b"three").to_hex();
        let four = Hash::of(b"four").to_hex();

        let mut board = Board::new();
        let place = Placement::new(0.0, 0.0, 10.0, 10.0);
        board
            .add(NewItem::new(
                ItemKind::Image { asset_id: one.clone(), crop: None },
                place,
            ))
            .expect("a fresh board accepts an image");
        board
            .add(NewItem::new(
                ItemKind::Document {
                    asset_id: two.clone(),
                    page_count: 1,
                    current_page: 0,
                },
                place,
            ))
            .expect("a fresh board accepts a document");
        board
            .add(NewItem::new(
                ItemKind::Embed {
                    title: None,
                    url: None,
                    description: None,
                    provider: None,
                    html: None,
                    thumbnail: Some(three.clone()),
                    favicon: Some(four.clone()),
                    mode: CardMode::Card,
                },
                place,
            ))
            .expect("a fresh board accepts an embed");
        board
            .add(NewItem::new(
                ItemKind::LinkPreview {
                    title: None,
                    url: None,
                    description: None,
                    thumbnail: Some("../../../etc/passwd".to_owned()),
                    provider: None,
                    favicon: None,
                    mode: CardMode::Card,
                },
                place,
            ))
            .expect("a fresh board accepts a link card");
        board
            .add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("no picture"), background: None },
                place,
            ))
            .expect("a fresh board accepts a sticky");

        let hashes = blob_hashes(&board);
        assert_eq!(hashes.len(), 4, "one per asset id that parses: {hashes:?}");
        for hex in [&one, &two, &three, &four] {
            let hash = hex.parse::<Hash>().expect("this test wrote it");
            assert!(hashes.contains(&hash), "{hex} was not collected");
        }
        // The traversal string parses as nothing, so it never reaches a URL.
        assert!(!hashes.iter().any(|hash| hash.to_hex().contains("passwd")));
    }

    /// The probe's answer is read the same way whatever framing it arrives in, because the
    /// two halves of this feature are written separately and a hash is unmistakable.
    #[test]
    fn the_probe_answer_is_read_in_any_framing() {
        let one = Hash::of(b"one");
        let two = Hash::of(b"two");
        let json = format!("[\"{}\",\"{}\"]", one.to_hex(), two.to_hex());
        let lines = format!("{}\n{}\n", one.to_hex(), two.to_hex());
        for body in [json, lines] {
            assert_eq!(hashes_in(&body), vec![one, two], "{body}");
        }
        assert!(hashes_in("").is_empty());
        assert!(hashes_in("nothing here").is_empty());
        // A short run is not a hash, however hexadecimal it looks.
        assert!(hashes_in("abc123").is_empty());
    }

    /// The mapping survives a restart, replaces rather than appends, and keeps one id per
    /// server. Without the last of those, signing in to a second server would overwrite the
    /// first server's id and the next run would create a duplicate there.
    #[test]
    fn a_recorded_id_survives_a_restart_and_is_kept_per_server() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let board = dir.path().join("notes.vellum");
        let first = "https://one.example/";
        let second = "https://two.example/";

        let mut ids = RemoteIds::open(dir.path());
        assert_eq!(ids.id_for(&board, first), None, "nothing is recorded yet");
        ids.record(&board, first, "notes");
        ids.record(&board, second, "notes-2");
        // Replaced rather than appended.
        ids.record(&board, first, "notes-3");

        let reopened = RemoteIds::open(dir.path());
        assert_eq!(reopened.id_for(&board, first), Some("notes-3"));
        assert_eq!(reopened.id_for(&board, second), Some("notes-2"));
        assert_eq!(reopened.records.len(), 2, "one record per server: {:?}", reopened.records);
    }

    /// A damaged file degrades to an empty mapping rather than failing the run. The cost is
    /// one more push; the alternative is a feature that cannot start.
    #[test]
    fn a_damaged_mapping_degrades_to_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join(RemoteIds::FILE), b"{ not json").expect("the test writes");
        let ids = RemoteIds::open(dir.path());
        assert!(ids.records.is_empty());
    }

    /// Dropping the handle stops the worker, and a run whose worker never started still
    /// answers rather than leaving the page on *Sending*.
    #[test]
    fn a_drain_of_a_dead_channel_finishes_rather_than_waiting() {
        let (events, inbound) = channel::<PushEvent>();
        drop(events);
        let mut push = Push { inbound, stop: Arc::new(AtomicBool::new(false)), finished: false };
        let drained = push.drain();
        assert_eq!(drained.len(), 1);
        assert!(matches!(&drained[0], PushEvent::Finished(summary) if summary.worker_lost));
        assert!(push.is_finished());
        assert!(push.drain().is_empty(), "and it does not invent a second summary");
    }

    /// Stop is a flag the worker reads between items, so setting it never blocks the caller.
    #[test]
    fn stopping_does_not_block_the_caller() {
        let (events, inbound) = channel::<PushEvent>();
        let push = Push { inbound, stop: Arc::new(AtomicBool::new(false)), finished: false };
        push.stop();
        assert!(push.stop.load(Ordering::Relaxed));
        drop(events);
    }
}
