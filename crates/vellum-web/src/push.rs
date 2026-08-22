//! The send half: what this tab has done, on its way to the board of record.
//!
//! [`crate::live`] is the receive half and its header says, in the present tense, that this
//! client *"sends no delta, ever"*. **That sentence describes `live.rs` and it no longer
//! describes the application.** The user asked for editing in a browser, explicitly, having
//! been told the risk this file exists to bound. So the read-only claim moves from *"the
//! client cannot write"* to *"the client writes through exactly one path, and that path is
//! built so the worst it can cost is the last few seconds of somebody's typing"*.
//!
//! # 🛑 RULE ZERO — why an editing browser client is safe, in six parts
//!
//! Every one of these is a property of code that exists, not an intention. Where a part is
//! weaker than it sounds it is marked, because a safety argument with one unmarked soft spot
//! in it is worse than no argument at all.
//!
//! 1. **Every edit pushes immediately.** There is no batching window, no debounce, no timer
//!    that groups edits and no *Save* button to forget. [`Pusher::note_edit`] marks the tab
//!    dirty and the very next [`Pusher::tick`] sends — and the edit path is expected to call
//!    `tick` in the same breath, which makes "immediately" the same frame rather than the
//!    next quarter second. The only thing that ever groups two edits into one request is a
//!    request already in flight, which is a round trip rather than a policy.
//! 2. **The board of record is the server's.** `velmd` runs the real `vellum-store` — SQLite
//!    in WAL mode, `BoardDb::save`, `db.close()` — so an accepted push is on disk before the
//!    reply is written. The tab is never the authority on a board; it is a client that has
//!    told the authority what it did.
//! 3. **The first change any board takes from the web is preceded by a labelled restore
//!    point.** `velmd::sync`'s `before_the_web` takes it once per board *ever*, and a
//!    labelled point is never pruned — not by that server, which prunes nothing, and not by
//!    the desktop app's `prune_restore_points`, which only takes automatic ones. So there is
//!    always a way back to the board as it was before a browser touched it.
//! 4. **A Loro update merges; it cannot remove what it did not add.** Applying a delta unions
//!    two operation histories. There is no byte sequence in the update format that truncates
//!    a document or replaces it with a different one. So the worst a lost push can cost is
//!    *the edits in it*, and the worst a hostile push can cost is ordinary vandalism that is
//!    visible on the canvas and reversible from (3).
//! 5. **A failed push loses nothing, by construction rather than by a retry queue.**
//!    [`Shared::since`] only advances on a reply that arrived and parsed, so the next attempt
//!    re-derives the same delta from the board itself. There is no queue of pending edits
//!    here to drop, mis-order or overflow — the *document* is the queue.
//! 6. **The tail is flushed on the way out.** `pagehide` and `visibilitychange: hidden` both
//!    fire [`flush`], which posts everything unacknowledged with `navigator.sendBeacon` — the
//!    only fire-and-forget a browser will still deliver after the page is gone.
//!
//! # ⚠ Where the argument is weaker than it sounds, stated once and plainly
//!
//! - **The beacon is best effort and has no result.** `sendBeacon` answers a `bool` meaning
//!   *queued*, never *delivered*; nothing here can know whether the last flush arrived. The
//!   browser also caps the whole beacon quota at [`BEACON_LIMIT`] on Chrome and Safari, so a
//!   large tail is refused outright — logged, and there is nothing else to be done about it.
//! - **So the honest worst case is not "the last edit".** It is *everything since the last
//!   acknowledged push* — one round trip's worth of work — lost if the tab dies while a push
//!   is out and the beacon does not make it. On a healthy link that is a fraction of a
//!   second. On a bad one it is however long the backoff has walked out to, and
//!   [`PushStatus::Offline`] exists to say so on the page rather than let it go quiet.
//! - **Unacknowledged edits live only in this tab.** There is no local persistence here — no
//!   IndexedDB, no `localStorage`, no snapshot on unload. A tab killed with a full backoff
//!   pending loses that window. That is the residual risk the user accepted; it is bounded by
//!   (1) and reported by [`Pusher::status`], and it is not zero.
//!
//! # The wire, and the half of it this speaks
//!
//! ```text
//! POST /api/v1/boards/{id}/sync
//!   request   [u32 le: length of the version vector][vv bytes][delta bytes]
//!   response  [u32 le: length of the version vector][vv bytes][update bytes]
//! ```
//!
//! The same endpoint `live.rs` polls, and the same one `vellum-app`'s `Sync` posts to. This
//! module fills the delta that `live::request_body` deliberately has no parameter for.
//!
//! ⚠ **`vv` governs the download and `since` governs the upload, and they are different
//! questions.** `vv` is what *this tab* has — `board.version()`, taken fresh — and it decides
//! what comes back. `since` is what the *server* had at the last reply it gave us, and it
//! decides what goes up. Swapping them compiles, still syncs, and quietly re-uploads the whole
//! document on every request. `vellum_app::editor::Editor::sync_now` states the same law over
//! the same two values; this is the browser's copy of it.
//!
//! ⚠ **The reply's version vector is kept here, where `live.rs` throws it away.** That is the
//! whole difference between the two files: a reader's only marker is its own version, so it
//! needs no memory at all. A writer that forgot the server's version would export since
//! nothing and send the entire board every time it moved a sticky.
//!
//! # Nothing here ever holds the viewer across an `await`
//!
//! ⚠ `crate::VIEWER` is a `thread_local RefCell<Option<Rc<RefCell<Viewer>>>>` that the frame
//! loop borrows mutably sixty times a second, and `[profile.release]` sets `panic = "abort"`
//! on this target — so a second mutable borrow does not raise an error somebody could report,
//! it kills the tab. The split is the same one `live.rs` makes and for the same reason:
//!
//! - **[`Pusher`] owns the queue**, in its own `Rc<RefCell<Shared>>`. The future spawned by
//!   [`Pusher::tick`] captures that `Rc` and a `String`, and nothing else. It cannot see a
//!   `Viewer`, a `Board` or a `Projection` — not in this version and not in a later one
//!   written by somebody who has not read this paragraph, because there is no such value in
//!   any signature it can reach.
//! - **[`tick`] and [`flush`] are the only things here that touch the viewer**, both are
//!   entered from a browser callback with nothing borrowed, and neither awaits. `tick` takes
//!   the borrow mutably through `try_borrow_mut` and gives up quietly if a frame is
//!   mid-flight; `flush` needs only `try_borrow`, because [`Pusher::tail`] is `&self` and the
//!   board is `&Board` — so the unload path cannot contend with anything that writes.
//!
//! # ⚠ Driven by `setInterval`, not by `requestAnimationFrame`
//!
//! `requestAnimationFrame` stops firing in a hidden tab. A pusher hung off the frame loop
//! could not retry a failed push while somebody was reading their mail, and replies already
//! in the queue would sit unapplied until they came back. `live.rs`'s header argues this at
//! length for the read path; it matters more here, because the thing that stops happening is
//! somebody's work reaching the server.
//!
//! # One request out at a time, per client
//!
//! [`Pusher`] and [`crate::live::Live`] each bound themselves to one request in flight, and
//! they are **not** coordinated with each other — so a tab being edited can have two requests
//! against the same endpoint at once. That is harmless and deliberately left alone: `apply`
//! is commutative and idempotent, `velmd` holds a per-board lock so the two serialise on the
//! server, and a push carries the current `vv` so its own reply already brings down whatever
//! the poll would have. Coordinating them would mean one shared in-flight flag across two
//! modules, which is a lock in exchange for one saved round trip.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_time::Instant;

use vellum_doc::{Board, Version};

use crate::Viewer;

/// How often the driver wakes.
///
/// **Not** a push rate — a push goes out when there is something to push, and this is only
/// how quickly that is noticed by the *timer*. The edit path is expected to call
/// [`Pusher::tick`] directly, so in normal use an edit never waits for this at all; the timer
/// is what retries a failed push and what applies a reply while the tab is hidden.
///
/// The same 250ms `live::TICK_MS` uses, deliberately: two timers at the same cadence are one
/// rhythm, and a tick with nothing to do here is an `Instant::now()`, four compares and a
/// look at an empty `Vec`.
const TICK_MS: i32 = 250;

/// How long one round trip has, enforced by `AbortSignal.timeout`.
///
/// ⚠ **Every `await` on a browser callback needs one of these.** `docs/08-web.md` §6 records
/// the probe page's three hangs — `mapAsync`, `toBlob` and `createImageBitmap` — and the
/// lesson that bounding only the two that were *observed* to hang left the third to hang
/// next. A `fetch` against a server that accepts the connection and then says nothing never
/// settles, and here that is worse than it is on the read path: a promise that never settled
/// would leave `in_flight` set for the life of the tab, so **nothing this tab did afterwards
/// would ever reach the server**, silently, with the status line still saying *saving*.
///
/// One signal covers all three awaits in [`round_trip`] — the fetch, the body read and the
/// error-text read — because aborting a request aborts its body stream with it.
const REQUEST_TIMEOUT_MS: u32 = 30_000;

/// The belt behind [`REQUEST_TIMEOUT_MS`]'s braces.
///
/// If the abort signal ever fails to fire — a browser that does not implement it, a promise
/// settled by something outside the fetch — this is what puts `in_flight` back down. Longer
/// than the request timeout so that in normal operation it never runs; a request it does
/// catch is counted as a failure, and the answer that eventually arrives is discarded by its
/// attempt id (see [`Shared::attempts`]).
const WATCHDOG: Duration = Duration::from_millis(REQUEST_TIMEOUT_MS as u64 + 10_000);

/// The most a request body may be, in bytes.
///
/// ⚠ **This is a copy of `velmd::sync::MAX_BODY`, and it has to be.** `vellum-web` cannot
/// depend on `velmd` — one is a wasm library and the other is a native server with SQLite in
/// it — so the number lives twice and the two must not drift. The server checks it against
/// `Content-Length` and answers **413 before reading a byte**, which is the case this exists
/// to prevent: `since` only advances on a reply, so a body the server refuses is re-derived
/// unchanged on every later attempt. Unbounded, that is the same oversized request for ever.
///
/// Checked against the **assembled frame**, which is exactly the bytes `Content-Length` will
/// count, so the client's answer and the server's cannot disagree by a header's worth.
///
/// In practice it is unreachable from a browser: the document never carries pixels (an image
/// is an `asset_id` and its bytes live in the blob store, which this client cannot write to
/// at all), so a delta is geometry, text and style. `velmd`'s own note puts a week offline at
/// kilobytes.
const MAX_PUSH_BYTES: usize = 8 * 1024 * 1024;

/// What a browser will accept for one `sendBeacon`, near enough.
///
/// The specification lets an implementation impose a quota and both Chrome and Safari settle
/// on 64 KiB across all in-flight beacons. A tail past this is **not** refused here — it is
/// attempted anyway, because the quota is the browser's number rather than ours and a
/// build that refused early would be certain to fail where the browser might not — but it is
/// logged, because a silently dropped flush is the one failure in this file with no other
/// evidence.
const BEACON_LIMIT: usize = 64 * 1024;

/// The most a reply may be, in bytes, before it is copied into wasm memory.
///
/// The point is not to bound a board, it is to bound a *wrong address*: a URL that answers
/// with a media stream must not be read into a tab's linear memory, which — unlike a native
/// heap — never gives a page back to the operating system. The same 32MB `live.rs` and
/// `vellum_app::sync` both use.
const MAX_REPLY_BYTES: u32 = 32 * 1024 * 1024;

/// How many replies may wait to be applied.
///
/// Reached only when [`tick`] cannot get the viewer for several ticks running. It is here so
/// that if something ever does cause that, the queue stops growing rather than growing
/// without bound: `tick` refuses to send while it is full, and `since` has not moved, so
/// nothing is lost.
const MAX_QUEUED: usize = 8;

/// The first wait after a failure. Doubles per consecutive failure up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_secs(2);

/// The longest this will ever wait between attempts.
///
/// A minute rather than an hour, for `vellum-app`'s reason: the common failure is a device
/// that has moved between networks, and on this path every second of that wait is somebody's
/// work sitting in a tab that only they can see.
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Consecutive failures after which the page should say *offline* rather than *retrying*.
///
/// ⚠ [`PushStatus::Offline`] does **not** mean this has given up — nothing here ever gives
/// up. It means the backoff has walked out far enough that the person editing deserves to be
/// told their work is only in this tab, rather than being shown a hopeful word while it goes
/// nowhere.
const OFFLINE_AFTER: u32 = 5;

/// What the page can say about the tab's unsent work.
///
/// A value rather than a log line, because *"what you have typed is not saved anywhere but
/// here"* is the one thing in this application a person has to be able to see.
/// [`push_status`] is how it reaches JavaScript.
///
/// `Debug` is safe and is not derived by reflex: every field is a count or a bounded sentence
/// taken from the server's own error body. **No board content reaches this type** — updates
/// go from the queue into `Board::apply` and are never formatted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushStatus {
    /// Everything this tab has done is on the server. `pushes` counts the round trips it took.
    Clean { pushes: u64 },
    /// Work is waiting, and a push is out or about to be.
    Sending { pending: u64 },
    /// Failing, and still trying. `failures` is the consecutive count.
    Retrying { failures: u32, pending: u64, reason: String },
    /// Failing long enough that the backoff has reached its ceiling. Still trying.
    Offline { pending: u64, reason: String },
    /// ⚠ The delta outgrew what the server will read, and it is a **dead end** rather than a
    /// slow retry.
    ///
    /// It has its own word for that reason. The delta only shrinks by reaching the server,
    /// and it cannot reach the server, so calling this *retrying* would be a hopeful sentence
    /// over a loop that cannot succeed. It is re-derived on the backoff schedule — at most
    /// once a minute, so it costs one export rather than four a second — and it would clear
    /// against a server whose own cap had been raised, which is the only route out that does
    /// not involve losing the work.
    TooLarge { bytes: usize, cap: usize, pending: u64 },
}

impl PushStatus {
    /// One line, for the page.
    pub fn line(&self) -> String {
        match self {
            Self::Clean { pushes: 0 } => "no local edits".to_owned(),
            Self::Clean { pushes } => format!("saved · {pushes} push(es)"),
            Self::Sending { pending } => format!("saving {pending} edit(s)"),
            Self::Retrying { failures, pending, reason } => {
                format!("not saved yet · {pending} edit(s), attempt {failures} · {reason}")
            }
            Self::Offline { pending, reason } => {
                format!("OFFLINE · {pending} edit(s) are only in this tab · {reason}")
            }
            Self::TooLarge { bytes, cap, pending } => format!(
                "TOO LARGE · {pending} edit(s) came to {bytes} bytes, past this server's \
                 {cap}-byte limit; they cannot be saved from this tab"
            ),
        }
    }

    /// Whether this tab is holding work nothing else has a copy of.
    ///
    /// The one question a caller actually wants answered — *"is it safe to close this?"* —
    /// asked without matching on five variants and getting the new one wrong later.
    pub fn unsaved(&self) -> bool {
        match self {
            Self::Clean { .. } => false,
            Self::Sending { pending }
            | Self::Retrying { pending, .. }
            | Self::Offline { pending, .. }
            | Self::TooLarge { pending, .. } => *pending > 0,
        }
    }
}

/// The request that is out, if one is.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    attempt: u64,
    began: Instant,
    /// ⚠ [`Shared::edits_noted`] as it stood when this went out — **not** as it stands now.
    ///
    /// This is what makes an edit made *during* a round trip survive that round trip's
    /// success. Acknowledging the live counter instead would mark work as sent that was
    /// never in the body.
    edits: u64,
}

/// Everything the spawned future and the main task both touch.
///
/// ⚠ This is the *whole* boundary. A future spawned by [`Pusher::tick`] captures an
/// `Rc<RefCell<Shared>>` and an endpoint, so there is no path from a network answer to the
/// document except through this struct and then through [`tick`], on the main task, with
/// nothing awaited.
#[derive(Default)]
struct Shared {
    /// Updates that came back **on a push** and have not been applied yet.
    ///
    /// A push carries this tab's current version vector, so its reply brings down whatever a
    /// poll would have. Dropping them because "this is the send half" would make a board
    /// being edited the one board that stops receiving.
    queue: Vec<Vec<u8>>,
    /// The request that is out. `None` means the next tick may send.
    in_flight: Option<InFlight>,
    /// Requests issued, ever.
    ///
    /// Names the one that is out, so an answer from an attempt the watchdog already gave up
    /// on is discarded rather than clearing a *newer* request's flag, acknowledging edits it
    /// never carried, or reporting its result.
    attempts: u64,
    /// Local edits reported by [`Pusher::note_edit`], ever.
    ///
    /// ⚠ **Two counters rather than a `dirty` flag, and the difference is the trap.** A flag
    /// has to be cleared when a push goes out and set again when it fails, and the failure
    /// path is exactly the one nobody exercises — so an edit stranded by a failed push waits
    /// for an *unrelated* later edit to be noticed again. With counters there is nothing to
    /// re-set: a failed push touches neither, so `edits_noted > edits_acked` is still true and
    /// the next attempt re-derives the same delta from the board for free.
    edits_noted: u64,
    /// The value of [`Shared::edits_noted`] the server has confirmed it received.
    edits_acked: u64,
    /// The **server's** version vector as of the last reply, which is what the next delta is
    /// exported since.
    ///
    /// ⚠ Not the version we sent. A reply's updates are imported into this document, so they
    /// become local operations; exporting since what we had *before* importing them would
    /// send the server its own work back, every round trip. Exporting since the server's own
    /// version asks the question actually being asked — what does it not have —  and it
    /// self-heals, because it also covers anything edited while the request was in flight.
    since: Option<Vec<u8>>,
    /// Consecutive failures, for the backoff. Reset by any reply that parses.
    failures: u32,
    /// Round trips the server accepted. The evidence behind [`PushStatus::Clean`].
    pushes: u64,
    /// The earliest the next attempt may go out because a previous one failed.
    retry_at: Option<Instant>,
    /// The last failure's sentence, for [`PushStatus`]. Never carries a token — see
    /// [`crate::without_token`].
    reason: Option<String>,
    /// The size of the body that was refused for being past [`MAX_PUSH_BYTES`], if one was.
    too_large: Option<usize>,
}

/// The pushing client for one board.
///
/// Lives on the [`Viewer`] and dies with it. There is no close path in this application — a
/// tab holds one board for its life — so there is nothing here to shut down, and the unload
/// flush is a browser event rather than a teardown call somebody has to remember to make.
pub struct Pusher {
    /// The full sync URL, **carrying whatever auth the page attached**.
    ///
    /// Handed in already built rather than derived here, and that is the point: the page
    /// built the snapshot URL with its own `encodeURIComponent` and its own `?token=`, that
    /// string is *known* to work because the board on screen came back through it, and
    /// `live::from_snapshot_url` has already turned it into a sync endpoint. A second
    /// derivation here could disagree with the first on exactly the boards whose names are
    /// not identifiers, and the failure would present as *"my edits are not saving"* on those
    /// boards alone.
    endpoint: String,
    shared: Rc<RefCell<Shared>>,
}

impl Pusher {
    /// A pusher for one board, against an endpoint that is already a working sync URL.
    ///
    /// See [`Pusher::endpoint`] for why this takes a built string rather than the pieces.
    pub fn new(endpoint: String) -> Self {
        // Named once, at the top of the log, **with the token stripped**. The last thing that
        // should be pasted into an issue is the credential guarding somebody's boards.
        log::info!("velm push: editing is on; pushing to {}", crate::without_token(&endpoint));
        Self { endpoint, shared: Rc::new(RefCell::new(Shared::default())) }
    }

    /// Called after every **local** edit, once the change is in the document.
    ///
    /// Non-blocking and cheap — one increment. The answer arrives later, through
    /// [`Pusher::drain`] and [`Pusher::status`].
    ///
    /// ⚠ **Never call this after `Board::apply` of a remote update.** This counter is the only
    /// thing standing between the tab and pushing back what it merely received: an update that
    /// arrived from the server is already the server's, and marking it as local work would put
    /// this client in a loop of echoing its own downloads. A spurious call is not fatal — the
    /// export comes back empty and [`Pusher::tick`] acknowledges it without sending — but it
    /// costs a full `export_since` on a board that had nothing to say.
    ///
    /// ⚠ **Call it *after* the mutation, not before.** [`Pusher::tick`] exports from the board
    /// as it stands, so a `note_edit` that ran first and a `tick` that ran before the write
    /// would send an empty delta and mark the real edit acknowledged. Ordering the two calls
    /// around the mutation is what makes "pushes immediately" literal:
    ///
    /// ```ignore
    /// viewer.board.…;                 // the edit
    /// viewer.projection.rebuild(…)?;  // whatever the edit path already does
    /// if let Some(push) = viewer.push.as_mut() {
    ///     push.note_edit();
    ///     push.tick(&viewer.board);   // same frame, not the next timer tick
    /// }
    /// ```
    pub fn note_edit(&mut self) {
        let mut shared = self.shared.borrow_mut();
        shared.edits_noted = shared.edits_noted.saturating_add(1);
    }

    /// Send, if there is anything to send and nothing is out. Never blocks.
    ///
    /// ⚠ **Drain and apply *before* calling this.** [`Pusher::drain`] hands back updates that
    /// came down on the last push; this exports the board's current version as the `vv` half
    /// of the frame, so a tick that asked before applying would tell the server it had less
    /// than it does and the server would dutifully send the same updates again — every push,
    /// for as long as the queue was non-empty. [`tick`] does it in that order and says so.
    ///
    /// **Refusing rather than queueing is the whole concurrency design.** A second request
    /// built while the first is out carries a version vector the first is about to supersede,
    /// and its delta is a superset the first already covers — so it is work whose answer is
    /// stale before it is asked for. Without this bound a slow link stacks requests for ever.
    /// Nothing is lost by refusing: the counters have not moved and the document still holds
    /// the edits, so the next tick builds a current request from scratch.
    pub fn tick(&mut self, board: &Board) {
        let now = Instant::now();
        let (attempt, body) = {
            let mut shared = self.shared.borrow_mut();

            // The watchdog, before anything else reads `in_flight`. See [`WATCHDOG`].
            //
            // ⚠ Read into a `bool` first rather than tested in the `if` itself. `shared` is a
            // `RefMut`, so a scrutinee spelled `shared.in_flight` borrows it through `Deref`
            // for the whole of the `if` — and the body needs it mutably.
            let stalled = shared
                .in_flight
                .is_some_and(|out| now.saturating_duration_since(out.began) > WATCHDOG);
            if stalled {
                shared.in_flight = None;
                note_failure(&mut shared, "the push never came back".to_owned(), now);
            }

            if shared.in_flight.is_some() {
                return;
            }
            if shared.queue.len() >= MAX_QUEUED {
                return;
            }
            if shared.retry_at.is_some_and(|at| now < at) {
                return;
            }
            // Nothing local is waiting. This is the common case on a board being read rather
            // than written, and it costs two loads and a compare — which is what makes it safe
            // to call this from a frame path as well as from the timer.
            if shared.edits_noted <= shared.edits_acked {
                return;
            }

            // `decode_or_empty` rather than `decode`: bytes that will not parse mean "the
            // server has nothing of ours", which costs one full upload and self-heals, where
            // a hard error would stop this tab saving for good. `velmd` degrades the same way
            // on the same field, in the same direction, for the same reason.
            let since = Version::decode_or_empty(shared.since.as_deref().unwrap_or_default());
            let delta = match board.export_since(&since) {
                Ok(delta) => delta,
                Err(error) => {
                    // Counted as a failure rather than returned quietly. The alternative is a
                    // tick that fails identically four times a second for ever, with nothing
                    // on the page to say the tab has stopped saving.
                    note_failure(
                        &mut shared,
                        format!("this tab's changes could not be packed up: {error}"),
                        now,
                    );
                    return;
                }
            };

            // ⚠ **An empty delta with edits outstanding is acknowledged, not sent.** It means
            // the server already has everything this document holds — a `note_edit` for a
            // change that produced no operations, or one raised against an update that had
            // arrived from the server. Sending it would be a request with nothing in it;
            // leaving the counters alone would leave the page reading *saving 1 edit* for
            // ever, over work that is genuinely saved.
            if delta.is_empty() {
                shared.edits_acked = shared.edits_noted;
                return;
            }

            let body = frame(&board.version().encode(), &delta);
            if body.len() > MAX_PUSH_BYTES {
                // Recorded as its own state rather than as a failure sentence, because it is
                // not a failure that retrying fixes. See [`PushStatus::TooLarge`]. The backoff
                // is set all the same, so this re-derives once a minute rather than four times
                // a second — an 8MB export on the main thread is a visible stall.
                let bytes = body.len();
                shared.too_large = Some(bytes);
                note_failure(
                    &mut shared,
                    format!(
                        "these edits came to {bytes} bytes, past the {MAX_PUSH_BYTES}-byte \
                         limit this server will read"
                    ),
                    now,
                );
                return;
            }
            shared.too_large = None;

            // Snapshotted here, so an edit made while this is in flight is not acknowledged by
            // its success. See [`InFlight::edits`].
            let edits = shared.edits_noted;
            shared.attempts = shared.attempts.wrapping_add(1);
            let attempt = shared.attempts;
            shared.in_flight = Some(InFlight { attempt, began: now, edits });
            // ⚠ The attempt id travels out of this block with the body rather than being read
            // back from `shared` after it. Re-reading `attempts` outside the borrow would be a
            // second derivation of "which request is this", and the two would agree until the
            // day something else incremented it in between — at which point a reply would
            // acknowledge a request that was never sent.
            (attempt, body)
        };

        // ⚠ The borrow above ends here, before the spawn. `spawn_local` schedules rather than
        // polls, so today the future could not run inside this call anyway — which is exactly
        // the kind of "safe because of what happens to be true elsewhere" this file does not
        // rely on.
        let endpoint = self.endpoint.clone();
        let shared = Rc::clone(&self.shared);
        wasm_bindgen_futures::spawn_local(async move {
            let outcome = round_trip(endpoint, body).await;
            let now = Instant::now();
            let mut shared = shared.borrow_mut();
            // Superseded: the watchdog gave up on this attempt and has already counted it.
            // Returning here rather than clearing the flag is what stops a late answer from
            // acknowledging edits a *newer*, still-outstanding request is carrying.
            let Some(out) = shared.in_flight.filter(|out| out.attempt == attempt) else {
                return;
            };
            shared.in_flight = None;
            match outcome {
                Ok(Answer { version, updates }) => {
                    // The acknowledgement, and the only place `edits_acked` ever moves
                    // forward on a real send. Everything noted after this request went out
                    // stays pending and goes in the next one.
                    shared.edits_acked = out.edits;
                    // ⚠ Kept, where `live.rs` throws the same field away. See the module
                    // header: this is what stops the next push carrying the whole document.
                    shared.since = Some(version);
                    shared.failures = 0;
                    shared.retry_at = None;
                    shared.reason = None;
                    shared.too_large = None;
                    shared.pushes = shared.pushes.wrapping_add(1);
                    // An empty update list is the steady state of a healthy conversation, not
                    // an error and not something to queue: queueing it would make `tick`
                    // reproject the whole board for a reply that had nothing to say.
                    if !updates.is_empty() {
                        shared.queue.push(updates);
                    }
                }
                // ⚠ Neither counter moves. `since` has not advanced either, so the next tick
                // re-derives *the same delta* from the board — which is why there is no queue
                // of pending edits in this file for a failure path to drop or re-order. The
                // document is the queue.
                Err(detail) => note_failure(&mut shared, detail, now),
            }
        });
    }

    /// Updates that came back on a push, in the order they arrived.
    ///
    /// Never blocks. Drained by [`tick`] and applied on the same task, before the next send.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut shared = self.shared.borrow_mut();
        std::mem::take(&mut shared.queue)
    }

    /// What the page should say about this tab's unsent work.
    pub fn status(&self) -> PushStatus {
        let shared = self.shared.borrow();
        let pending = shared.edits_noted.saturating_sub(shared.edits_acked);
        if let Some(bytes) = shared.too_large {
            return PushStatus::TooLarge { bytes, cap: MAX_PUSH_BYTES, pending };
        }
        match &shared.reason {
            Some(reason) if shared.failures >= OFFLINE_AFTER => {
                PushStatus::Offline { pending, reason: reason.clone() }
            }
            Some(reason) => PushStatus::Retrying {
                failures: shared.failures,
                pending,
                reason: reason.clone(),
            },
            None if pending > 0 => PushStatus::Sending { pending },
            None => PushStatus::Clean { pushes: shared.pushes },
        }
    }

    /// Everything not yet acknowledged, framed and ready for `sendBeacon`.
    ///
    /// `None` when there is nothing outstanding, when the board will not export, or when the
    /// borrow is unavailable — all three mean *do not send a beacon*, and none of them loses
    /// anything a live [`Pusher::tick`] will not pick up if the tab survives.
    ///
    /// ⚠ **It deliberately includes a request that is in flight.** "Not yet acknowledged" is
    /// measured from [`Shared::since`], which only advances on a reply that arrived — so a
    /// push that is still travelling when the page goes away is re-sent in the beacon. That is
    /// free: `Board::apply` is idempotent, and a request whose answer nobody will ever read is
    /// a request nobody can confirm was delivered.
    ///
    /// ⚠ **`&self`, and that is load-bearing rather than tidy.** The unload path needs only an
    /// immutable borrow of the viewer, so it cannot contend with the frame loop's mutable one
    /// in any way that could be resolved by dropping the flush. It also means a beacon does
    /// **not** acknowledge anything: there is no result to acknowledge on, so a tab that comes
    /// back from `visibilitychange: hidden` simply pushes the same bytes again.
    pub fn tail(&self, board: &Board) -> Option<Vec<u8>> {
        // `try_borrow` rather than `borrow`. Nothing here awaits under this borrow, so a
        // conflict is not reachable today; on the unload path the cost of being wrong is the
        // tab aborting as it closes, which would look exactly like a browser crash.
        let shared = self.shared.try_borrow().ok()?;
        if shared.edits_noted <= shared.edits_acked {
            return None;
        }
        let since = Version::decode_or_empty(shared.since.as_deref().unwrap_or_default());
        let delta = match board.export_since(&since) {
            Ok(delta) => delta,
            Err(error) => {
                log::warn!("velm push: the unload flush could not pack this tab's changes: {error}");
                return None;
            }
        };
        if delta.is_empty() {
            return None;
        }
        // The current version vector, not an empty one. The reply is thrown away either way,
        // and an empty `vv` would make `velmd` serialise the whole board for nobody.
        let body = frame(&board.version().encode(), &delta);
        if body.len() > MAX_PUSH_BYTES {
            log::warn!(
                "velm push: the unload flush is {} bytes, past the {MAX_PUSH_BYTES}-byte limit; \
                 these edits cannot be saved from this tab",
                body.len()
            );
            return None;
        }
        Some(body)
    }

    /// Hand a body to `navigator.sendBeacon`. Answers whether the browser **queued** it.
    ///
    /// ⚠ **Queued is not delivered, and there is no API that tells you which happened.** That
    /// is the whole nature of the unload path: the page is going away, so nothing can wait for
    /// a reply. `true` here means the browser accepted the bytes into its beacon quota.
    ///
    /// Public alongside [`Pusher::tail`] so that a caller flushing at some other moment does
    /// not have to rebuild the URL — a second copy of the endpoint is precisely what
    /// [`Pusher::endpoint`]'s doc argues against.
    pub fn beacon(&self, body: &[u8]) -> bool {
        let Some(window) = web_sys::window() else { return false };
        if body.len() > BEACON_LIMIT {
            log::warn!(
                "velm push: a {}-byte flush is past the ~{BEACON_LIMIT}-byte beacon quota most \
                 browsers impose; trying anyway",
                body.len()
            );
        }
        // `Uint8Array::from` copies, so this does not hand the browser a view into wasm memory
        // that a later allocation could move out from under it — which matters more here than
        // on the fetch path, because the page is being torn down around this call.
        //
        // A `BufferSource` body carries no `Content-Type`, which keeps this a CORS-simple
        // POST — no preflight, and a preflight is a second round trip a page being unloaded
        // does not have time for. `velmd` needs `Content-Length`, which the browser sets, and
        // never looks at the type.
        let payload = js_sys::Uint8Array::from(body);
        match window.navigator().send_beacon_with_opt_js_u8_array(&self.endpoint, Some(&payload)) {
            Ok(true) => {
                log::info!("velm push: flushed {} bytes on the way out", body.len());
                true
            }
            Ok(false) => {
                log::warn!(
                    "velm push: the browser refused a {}-byte flush; those edits are lost",
                    body.len()
                );
                false
            }
            Err(error) => {
                log::warn!("velm push: the unload flush would not send ({error:?})");
                false
            }
        }
    }

    /// An update came back on a push and could not be merged.
    ///
    /// Counted as a failure so the backoff covers it, because the alternative is a loop: the
    /// local version cannot advance past bytes that will not apply, so the next push asks for
    /// the same range and gets the same answer. Backing off turns that into a slow, visible,
    /// reported failure instead of a fast invisible one.
    pub fn note_apply_failure(&mut self, detail: String) {
        let now = Instant::now();
        let mut shared = self.shared.borrow_mut();
        note_failure(&mut shared, detail, now);
    }

    /// The tab is visible again: drop the backoff so the next tick sends at once.
    ///
    /// Only the wait is cleared, never `in_flight` — a request that is genuinely out stays
    /// out, or coming back to a tab would fire a second push against every reply still
    /// travelling.
    fn wake(&mut self) {
        let mut shared = self.shared.borrow_mut();
        shared.retry_at = None;
    }
}

/// Start the push loop and the unload flush. Called once, from `boot`, after `VIEWER` holds
/// the viewer.
///
/// ⚠ **This one call is the entire reachability of this module.** Without it the file
/// compiles, its logic is right, and no edit ever leaves the tab — which is this repository's
/// signature defect, found nine times by its own count. It is commented rather than merely
/// present for that reason.
///
/// ⚠ **One interval and three closures for the life of the tab**, deliberately not the
/// `Closure::once` + `forget` per call that `crate::schedule_frame` uses for
/// `requestAnimationFrame`. That idiom is right for a one-shot callback and wrong for a
/// recurring one: `forget` leaks the JS wrapper, and a loop that re-armed itself four times a
/// second would leak all day on a device that has no memory to spare.
pub fn drive() {
    let Some(window) = web_sys::window() else { return };

    let ticker = Closure::<dyn FnMut()>::new(|| tick(false));
    if let Err(error) = window.set_interval_with_callback_and_timeout_and_arguments_0(
        ticker.as_ref().unchecked_ref(),
        TICK_MS,
    ) {
        // Recorded rather than ignored: with no timer nothing here retries, so a single failed
        // push would strand the tab's work with nothing to pick it up again.
        log::warn!("velm push: the push timer would not start ({error:?})");
    }
    ticker.forget();

    // ⚠ **`pagehide` and not `unload`.** A page with an `unload` handler is ineligible for the
    // back/forward cache on every current browser, and — worse for this — `unload` does not
    // fire at all on iOS Safari, which is the device this whole client exists for. `pagehide`
    // fires on both a real navigation and a bfcache suspension.
    let leaving = Closure::<dyn FnMut()>::new(|| flush("pagehide"));
    if let Err(error) =
        window.add_event_listener_with_callback("pagehide", leaving.as_ref().unchecked_ref())
    {
        log::warn!("velm push: no pagehide listener ({error:?})");
    }
    leaving.forget();

    // The other half, and on a phone it is the one that actually fires: switching apps hides
    // the tab, and the operating system may kill it later without ever raising `pagehide`
    // again. So a hide is treated as a possible ending — flush — and a show as a return —
    // clear the backoff and send whatever accumulated while nobody was looking.
    if let Some(document) = window.document() {
        let visibility = Closure::<dyn FnMut()>::new(|| {
            let hidden = web_sys::window()
                .and_then(|window| window.document())
                .is_some_and(|document| document.hidden());
            if hidden {
                flush("visibilitychange");
            } else {
                tick(true);
            }
        });
        let listening = document.add_event_listener_with_callback(
            "visibilitychange",
            visibility.as_ref().unchecked_ref(),
        );
        if let Err(error) = listening {
            log::warn!("velm push: no visibility listener ({error:?})");
        }
        visibility.forget();
    }
}

/// One turn of the loop: apply what came back, then send what is waiting.
///
/// ⚠ **Drain, apply, reproject, *then* export and send.** The order is the behaviour, not a
/// preference: sending first would carry the version the board had *before* the updates
/// sitting in the queue were applied, so the server would send them again on the next push,
/// and every push after that. It is the same law `live::tick` states one file over, and it is
/// one statement's difference between a sync that converges and one that does not.
///
/// Enters with nothing borrowed and never awaits, which is what makes the borrow below safe
/// against the frame loop rather than lucky.
///
/// `waking` is true only on the turn that follows a tab becoming visible again, where the
/// backoff is dropped so work that piled up while nobody was looking goes at once rather than
/// up to a minute later.
fn tick(waking: bool) {
    let Some(held) = crate::VIEWER.with(|slot| slot.borrow().clone()) else { return };
    // A failed borrow means a frame is mid-flight. Nothing is lost: the queue keeps what
    // arrived, the counters have not moved, and the next tick is 250ms away.
    let Ok(mut guard) = held.try_borrow_mut() else { return };
    // One deref, so the field accesses below are disjoint borrows of `Viewer` rather than
    // repeated borrows of the whole `RefMut`.
    let viewer: &mut Viewer = &mut guard;

    let updates = match viewer.push.as_mut() {
        Some(push) => {
            if waking {
                push.wake();
            }
            push.drain()
        }
        // A static board, or a page with no server. There is nothing to push to.
        None => return,
    };

    // ⚠ `live::merge` rather than a second apply-and-reproject here. It is the one place that
    // knows a merge is `Board::apply` **plus** `Projection::rebuild` **plus** re-reading the
    // board's background, and a second copy that forgot the third would leave a board whose
    // colour had been changed elsewhere drawing the old one until the tab was reloaded. Two
    // copies of a layout is a click that lands where the paint is not; two copies of a merge
    // is the same mistake one layer down.
    let merged = if updates.is_empty() { Ok(0) } else { crate::live::merge(viewer, &updates) };

    match merged {
        Ok(applied) => {
            if applied > 0 {
                log::info!("velm push: merged {applied} update(s) that came back on a push");
            }
            // Disjoint field borrows: `push` mutably, `board` immutably. Spelled as two
            // statements so that stays obvious to a reader as well as to the compiler.
            let board = &viewer.board;
            if let Some(push) = viewer.push.as_mut() {
                push.tick(board);
            }
        }
        // Deliberately no send on this turn. `note_apply_failure` has set a backoff, and the
        // turn after it re-derives the same delta — see that method.
        Err(detail) => {
            if let Some(push) = viewer.push.as_mut() {
                push.note_apply_failure(detail);
            }
        }
    }
}

/// The tail, on the way out. Best effort, and there is no way to make it more than that.
///
/// ⚠ **Only `try_borrow`.** [`Pusher::tail`] is `&self` and the board is `&Board`, so this
/// path never needs the mutable borrow the frame loop holds — which is what makes it safe to
/// run from an event that can fire at any moment during teardown. A failed borrow is reported
/// rather than swallowed, because on this path a skipped flush is somebody's work.
fn flush(from: &str) {
    let Some(held) = crate::VIEWER.with(|slot| slot.borrow().clone()) else { return };
    let Ok(viewer) = held.try_borrow() else {
        log::warn!("velm push: {from} arrived mid-frame; the flush could not read the board");
        return;
    };
    let Some(push) = viewer.push.as_ref() else { return };
    let Some(body) = push.tail(&viewer.board) else { return };
    log::info!("velm push: {from} — flushing {} bytes", body.len());
    push.beacon(&body);
}

/// What this tab is holding, as a string, for the page.
///
/// A getter rather than a write into `#velm-status`: that element carries the boot line — the
/// item count, the pixels painted and the camera — which is the diagnostic every screenshot of
/// this page depends on, and overwriting it would destroy the one thing that makes a
/// photograph of this client self-describing.
///
/// `chrome.js` gates every control on `typeof mod.fn === 'function'`, so a build without this
/// export simply draws nothing where the indicator would be.
///
/// ⚠ `"starting"` and `"off"` are different answers and must not share a word — the mistake
/// `sync_status` already made and fixed. `"off"` is final and means this board cannot be
/// edited from here; `"starting"` means `boot` has not finished, which is true for a moment on
/// every board including the ones that can.
#[wasm_bindgen]
pub fn push_status() -> String {
    crate::VIEWER.with(|slot| {
        let Some(held) = slot.borrow().clone() else { return "starting".to_owned() };
        let Ok(viewer) = held.try_borrow() else { return "busy".to_owned() };
        match viewer.push.as_ref() {
            Some(push) => push.status().line(),
            None => "off".to_owned(),
        }
    })
}

/// Whether this tab is holding work the server has not confirmed.
///
/// Exported so the page can put a `beforeunload` confirmation in front of a close that would
/// lose something — which is the browser's own last line of defence and the only one that
/// involves the person. Deliberately **not** wired to a confirmation from here: a
/// `beforeunload` prompt is a page-level policy, it is ignored by browsers unless the user has
/// interacted with the page, and a wasm module that raised one unasked would be making a
/// product decision in a module that pushes bytes.
#[wasm_bindgen]
pub fn push_unsaved() -> bool {
    crate::VIEWER.with(|slot| {
        let Some(held) = slot.borrow().clone() else { return false };
        let Ok(viewer) = held.try_borrow() else { return false };
        viewer.push.as_ref().is_some_and(|push| push.status().unsaved())
    })
}

/// Records a failure and sets the earliest time the next attempt may go out.
///
/// A free function rather than a method so both the main task ([`Pusher::note_apply_failure`],
/// the watchdog, the size refusal) and the spawned future reach one definition of what a
/// failure costs.
fn note_failure(shared: &mut Shared, detail: String, now: Instant) {
    shared.failures = shared.failures.saturating_add(1);
    let wait = backoff_after(shared.failures);
    // `checked_add` because `Instant` arithmetic can overflow at the end of the monotonic
    // clock's range. `None` there means "hold nothing back", which is the safe direction on
    // this path: the worst case is one extra push against a server that is down.
    shared.retry_at = now.checked_add(wait);
    log::warn!("velm push: {detail}; retrying in {wait:?}");
    shared.reason = Some(detail);
}

/// How long to wait after `failures` consecutive failures.
///
/// Pure, and — with `backoff_after` in `live.rs` and `vellum_app::sync` — the third copy of
/// one schedule. It is the piece of this file most worth moving somewhere it can be tested;
/// see the module note in the report. `vellum-app`'s copy is the one that has a test, because
/// this crate is `#![cfg(target_arch = "wasm32")]` and nothing in it can have a runnable one.
fn backoff_after(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // Clamped *before* the shift. The failure count is unbounded — a tablet left on a table
    // over a weekend against a server that never answers comes back with a five-figure count
    // — and `1u32 << 32` is a debug panic and a wrapped shift in release.
    let steps = (failures - 1).min(16);
    let factor = 1u32 << steps;
    BACKOFF_BASE.checked_mul(factor).unwrap_or(BACKOFF_CAP).min(BACKOFF_CAP)
}

/// The request body: a version vector, and this tab's changes behind it.
///
/// ⚠ This is the parameter `live::request_body` deliberately does not have. That function's
/// doc says a general `frame(version, payload)` there *"would leave a send path one argument
/// away from existing"* — this file **is** that send path, made explicit and put behind a
/// safety argument, rather than grown quietly inside the reader.
///
/// ⚠ **A length that does not fit the header becomes `u32::MAX`, not a truncated value.** A
/// version vector cannot reach 4GB — it is a few bytes per peer — but `as u32` on one that did
/// would keep the low 32 bits and produce a frame whose prefix disagrees with its contents,
/// which `velmd`'s `split_frame` would split at the wrong offset and **accept**, merging a
/// slice of a version vector into a board as though it were operations. `u32::MAX` is
/// unparseable there by construction, so the impossible case fails loudly at the first hop
/// rather than quietly corrupting a board.
fn frame(version: &[u8], delta: &[u8]) -> Vec<u8> {
    let length = u32::try_from(version.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + version.len() + delta.len());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(version);
    out.extend_from_slice(delta);
    out
}

/// What came back from one push.
///
/// A struct rather than a tuple because the two halves are easy to swap and the compiler
/// cannot tell two `Vec<u8>`s apart — the same reason `vellum_app::sync::SyncReply` names its
/// fields. `Debug` is deliberately **not** derived: `updates` is board content.
struct Answer {
    /// The server's version vector, which becomes [`Shared::since`].
    version: Vec<u8>,
    /// Everything this tab was missing, for `Board::apply`. May be empty.
    updates: Vec<u8>,
}

/// Splits a reply into its version vector and the updates behind it.
///
/// Panic-free **by construction** rather than by argument: every read goes through `get`, so a
/// truncated body, a body shorter than the header, and a header claiming more than the body
/// holds all return an error instead of slicing. That distinction matters more here than it
/// usually would — `[profile.release]` sets `panic = "abort"`, so an out-of-range slice would
/// not raise an error a caller could report, it would take the tab down and with it every
/// unpushed edit in it. The same class of bug reached the user twice through
/// `strip_site_affix`.
///
/// ⚠ **This is the fifth copy of this function in the repository** — `velmd::sync`'s
/// `split_frame`, `vellum_app::sync`'s `unframe`, `live.rs`'s `unframe`, and this one, against
/// one `frame` in each of three. `vellum-doc` is the only crate all of them depend on, and
/// `version.rs` already says a `Version` is encoded *"for the body of a sync request or
/// response"*. The framing belongs there; the report that came with this file names the split.
fn unframe(bytes: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let Some(header) = bytes.get(..4).and_then(|head| <[u8; 4]>::try_from(head).ok()) else {
        return Err(format!("a sync frame needs 4 header bytes and this one has {}", bytes.len()));
    };
    // `usize` is at least 32 bits on wasm32, so this widening cannot lose a byte of the length.
    let claimed = u32::from_le_bytes(header) as usize;
    let rest = bytes.get(4..).unwrap_or_default();
    let Some(version) = rest.get(..claimed) else {
        return Err(format!(
            "the reply claims a {claimed}-byte version vector and carries {} bytes",
            rest.len()
        ));
    };
    let updates = rest.get(claimed..).unwrap_or_default();
    Ok((version, updates))
}

/// One round trip. Runs on a spawned task and touches nothing but its arguments.
///
/// ⚠ **It cannot see a `Viewer`, a `Board` or a `Shared`.** It takes owned bytes and answers
/// owned bytes. That is the mechanical form of "the document is only ever changed on the main
/// task with nothing awaited", and it is the same shape `vellum_app::sync`'s worker takes for
/// the same reason: a signature with no document in it cannot apply one, even by accident.
///
/// Never returns a typed error: every failure is a sentence, because the caller's response to
/// all of them is identical — count it, back off, say so — and a `Result` with variants would
/// invite an early `?` on a path whose whole job is to always answer.
///
/// ⚠ **Every sentence it produces goes through [`crate::without_token`].** These strings
/// become `Shared::reason`, which becomes `PushStatus::line()`, which `push_status()` hands to
/// the page — and `boards.js` states the posture: the passphrase is "never in a URL, never in
/// history and never in an access log". A status line on hover is none of the three and is not
/// the exception.
async fn round_trip(endpoint: String, body: Vec<u8>) -> Result<Answer, String> {
    let window = web_sys::window().ok_or("no window")?;

    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    // `Uint8Array::from` copies, so this does not hand JavaScript a view into wasm memory that
    // a later allocation could move out from under it — which on this path would be a board
    // edit sent as whatever happened to be at that address.
    let payload = js_sys::Uint8Array::from(body.as_slice());
    init.set_body_opt_u8_array(Some(&payload));
    // See [`REQUEST_TIMEOUT_MS`]: one signal covers the fetch, the body read and the error
    // read, because aborting a request aborts its body stream with it.
    init.set_signal(Some(&web_sys::AbortSignal::timeout_with_u32(REQUEST_TIMEOUT_MS)));
    // ⚠ **No `Content-Type`, deliberately.** `velmd` needs a `Content-Length` — which `fetch`
    // sets itself for a buffer body — and never looks at the type. Leaving it off also keeps
    // this a CORS-simple request, so a cross-origin deployment does not pay a preflight on
    // every keystroke's push.

    let response =
        wasm_bindgen_futures::JsFuture::from(window.fetch_with_str_and_init(&endpoint, &init))
            .await
            .map_err(|_| format!("could not reach {}", crate::without_token(&endpoint)))?;
    let response: web_sys::Response =
        response.dyn_into().map_err(|_| "that is not a response".to_owned())?;

    let status = response.status();
    if !response.ok() {
        // ⚠ The server's own words, not the number. *"push failed"* against a wrong token is a
        // report nobody can act on; *"a bearer token is required"* is one they can, and it is
        // the sentence `velmd` already writes into the body of its 401. The two that matter
        // most here have their own sentences too: **413** is the size cap this client is
        // supposed to have caught first, and **400** carries either *"that delta did not apply
        // to this board"* or *"that delta would leave this board unreadable"* — the second is
        // `velmd` refusing to save a document it could not read back, which is a RULE ZERO
        // guard doing its job and must reach the page verbatim rather than as "sync failed".
        return Err(match read_text(&response).await {
            Some(detail) if !detail.is_empty() => format!("HTTP {status}: {detail}"),
            _ => format!("HTTP {status}"),
        });
    }

    let buffer = wasm_bindgen_futures::JsFuture::from(
        response.array_buffer().map_err(|_| "the reply has no body".to_owned())?,
    )
    .await
    .map_err(|_| "could not read the reply".to_owned())?;
    let buffer: js_sys::ArrayBuffer =
        buffer.dyn_into().map_err(|_| "the reply is not bytes".to_owned())?;

    // Before the copy, never after. See [`MAX_REPLY_BYTES`]. The JS-side allocation has already
    // happened by then and cannot be helped without streaming the body; the wasm-side copy is
    // the one this can refuse, and on a tablet it is the one that matters.
    if buffer.byte_length() > MAX_REPLY_BYTES {
        return Err(format!(
            "the reply is {} bytes, past the {MAX_REPLY_BYTES}-byte ceiling",
            buffer.byte_length()
        ));
    }
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();

    let (version, updates) = unframe(&bytes)?;
    Ok(Answer { version: version.to_vec(), updates: updates.to_vec() })
}

/// The text of an error response, bounded.
///
/// ⚠ **Bounded by characters, never by bytes.** `velmd`'s own error bodies are ASCII, but a
/// proxy or a load balancer in front of it can answer with anything, and slicing a `String` at
/// a byte offset aborts the process on any multi-byte character straddling the boundary.
/// Feedback 30, twice, and feedback 34 again.
async fn read_text(response: &web_sys::Response) -> Option<String> {
    let promise = response.text().ok()?;
    let value = wasm_bindgen_futures::JsFuture::from(promise).await.ok()?;
    let text = value.as_string()?;
    Some(text.trim().chars().take(200).collect())
}
