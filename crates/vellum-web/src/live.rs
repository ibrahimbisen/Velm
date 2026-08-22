//! Live sync: the tab asks `velmd` what has changed, and merges it.
//!
//! Before this, the browser fetched a board's snapshot once at boot and never looked again —
//! so a board open in a tab could not see an edit made on the Mac, or in another tab, or by
//! anything else. `velmd` answers `POST /api/v1/boards/{id}/sync`; this is the client half.
//!
//! # The wire, and the half of it this speaks
//!
//! ```text
//! POST /api/v1/boards/{id}/sync
//!   request   [u32 le: length of the version vector][vv bytes][delta bytes]
//!   response  [u32 le: length of the version vector][vv bytes][update bytes]
//! ```
//!
//! ⚠ **This client sends no delta, ever, and that is structural rather than a convention.**
//! There is no `delta` parameter anywhere in this file: [`request_body`] takes a version
//! vector and nothing else, so the request it builds is four bytes plus a version and there
//! is no shape of a send path here for a later change to "finish". `docs/08-web.md` and
//! RULE ZERO record why the browser is a reader: a tab can be killed by the OS with no
//! warning and no chance to flush, and a client that could edit would at that moment be
//! holding the only recent copy of a board that cannot be re-imported.
//!
//! The reply's own version vector is parsed and **discarded**. `vellum-app`'s `Sync` keeps
//! the server's version because it has edits of its own to export *since* something; a reader
//! has nothing to export, so the only marker it needs is its own `board.version()`, taken
//! fresh on every ask. That is what makes the whole thing self-healing with no bookkeeping:
//! if a reply is lost, dropped, or fails to apply, the local version simply has not moved and
//! the next request asks for the same range again.
//!
//! # No request ids, no acknowledgements, no retry bookkeeping
//!
//! ⚠ Loro is a CRDT and [`vellum_doc::Board::apply`] is **commutative and idempotent**. An
//! update applied twice is the same board as an update applied once, so a repeated request
//! costs a round trip and nothing else. That is the entire reason this module has no state
//! machine in it — only "a request is out" or "it is not". Anyone tempted to add sequence
//! numbers, at-least-once delivery or an acknowledgement should read that sentence again:
//! they would be paying for a guarantee the document layer already gives for free.
//!
//! # Nothing here ever holds the viewer across an `await`
//!
//! ⚠ The board lives in a `thread_local` `RefCell<Viewer>` that the frame loop borrows
//! mutably every frame. So the split in this file is not tidiness — it is the whole safety
//! argument, and it is the same one [`crate::Viewer::begin_self_check`] states from the other
//! end:
//!
//! - **[`Live`] owns the queue**, in its own `Rc<RefCell<Shared>>`. The spawned future
//!   touches `Shared` and nothing else. It cannot see a `Viewer`, a `Board` or a
//!   `Projection`, in this version or in a later one written by somebody who has not read
//!   this paragraph.
//! - **[`tick`] is the only thing that touches the viewer**, it is entered from a timer with
//!   nothing borrowed, and it never awaits. It borrows, applies, reprojects, asks, and
//!   returns.
//!
//! # ⚠ The tab is not always visible, so this is not driven by `requestAnimationFrame`
//!
//! `requestAnimationFrame` stops firing when a tab is hidden. A poll loop hung off the frame
//! loop would therefore stop the moment somebody switched tabs, and — worse — replies already
//! in the queue would sit there unapplied, so the board would be stale on return until a
//! whole round trip had completed. That is exactly the shape of a *"sync doesn't work"*
//! report. A `setInterval` keeps running while hidden (clamped by the browser to a second or
//! so, and after a long absence Chrome throttles it further, which is the right direction:
//! a tab nobody is looking at should cost less).
//!
//! **Two rates, and they are not the same number.** The timer wakes at [`TICK_MS`] (250ms) —
//! that is local arithmetic, a couple of compares and an empty-vector check, four times a
//! second, against a frame loop already running at sixty. A *request* goes out at most once
//! per [`Live::period`] (2s by default), which [`Live::poll`] decides for itself. The fast
//! tick is what makes an arriving reply appear within a quarter second instead of waiting up
//! to a whole poll period; it is not four requests a second.
//!
//! The one thing a clamped timer cannot do is notice promptly that the tab came *back*, so
//! `visibilitychange` clears the schedule and ticks at once. Without it, a tab returned to
//! after five minutes hidden can be up to a minute behind for no reason a user could guess.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_time::Instant;

use crate::Viewer;

/// How often the driver wakes. **Not** how often a request goes out — see the module header.
///
/// A tick with nothing to do is one `Instant::now()`, three compares and a look at an empty
/// `Vec`. What it buys is latency: a reply that landed 10ms ago is on the board 250ms later
/// rather than up to [`DEFAULT_PERIOD_MS`] later.
const TICK_MS: i32 = 250;

/// How often a request actually goes out, when the page does not say.
///
/// Two seconds is chosen against what the reply costs rather than against how live it feels:
/// a poll with nothing to report is a version vector out and a version vector back — a few
/// hundred bytes — and `velmd`'s sync handler explicitly does no restore point, no save and
/// no write at all for an empty delta, so an idle client costs the server a board open.
pub const DEFAULT_PERIOD_MS: i32 = 2_000;

/// A floor on the period, so a page cannot ask for a request per tick.
const MIN_PERIOD_MS: i32 = 250;

/// How long one round trip has, enforced by `AbortSignal.timeout`.
///
/// ⚠ **Every `await` on a browser callback needs one of these.** `docs/08-web.md` §6 records
/// the probe page's three hangs — `mapAsync`, `toBlob` and `createImageBitmap` — and the
/// lesson that bounding only the two that were *observed* to hang left the third to hang
/// next. A `fetch` against a server that accepts the connection and then says nothing never
/// settles, and a promise that never settles here would leave `in_flight` set for the life of
/// the tab: sync would stop, silently, with no error to display.
///
/// One signal covers all three awaits in [`round_trip`] — the fetch, the body read, and the
/// error-text read — because aborting a request aborts its body stream too.
const REQUEST_TIMEOUT_MS: u32 = 30_000;

/// The belt behind [`REQUEST_TIMEOUT_MS`]'s braces.
///
/// If the abort signal ever fails to fire — a browser that does not implement it, a promise
/// resolved by something outside the fetch — this is what puts `in_flight` back down. It is
/// deliberately longer than the request timeout so that in normal operation it never runs;
/// a request it does catch is counted as a failure, and the answer that eventually arrives is
/// discarded by its attempt id (see [`Shared::attempts`]).
const WATCHDOG: Duration = Duration::from_millis(REQUEST_TIMEOUT_MS as u64 + 10_000);

/// The most a reply may be, in bytes, before it is copied into wasm memory.
///
/// The point is not to bound a board, it is to bound a *wrong address*: a URL that answers
/// with a media stream must not be read into a tab's linear memory, which — unlike a native
/// heap — never gives a page back to the operating system. The same 32MB `vellum-app`'s
/// `Sync` uses, which is roughly thirty times the largest payload measured anywhere in this
/// repository.
///
/// ⚠ Checked on the `ArrayBuffer`'s own `byteLength`, **before** `to_vec()`. The JS-side
/// allocation has already happened by then and cannot be helped without streaming the body;
/// the wasm-side copy is the one this can refuse, and on a tablet it is the one that matters.
const MAX_REPLY_BYTES: u32 = 32 * 1024 * 1024;

/// How many replies may wait to be applied.
///
/// Reached only when [`tick`] cannot get the viewer for several ticks running, which nothing
/// today can cause. It is here so that if something ever does, the queue stops growing rather
/// than growing without bound: `poll` refuses while it is full, and the version vector has
/// not moved, so nothing is lost — the server sends the same range again when asking resumes.
const MAX_QUEUED: usize = 8;

/// The first wait after a failure. Doubles per consecutive failure up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_secs(2);

/// The longest this will ever wait between attempts.
///
/// A minute rather than an hour, for `vellum-app`'s reason: the common failure is a device
/// that has moved between networks, and the evidence that sync is working again should arrive
/// within the time it takes somebody to notice they are back.
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Consecutive failures after which the page should say *disconnected* rather than *retrying*.
///
/// ⚠ [`Status::Offline`] does **not** mean this has given up — nothing here ever gives up.
/// It means the backoff has walked out far enough that a person watching deserves to be told
/// the board they are looking at is not live, rather than being shown a hopeful word while it
/// silently goes stale.
const OFFLINE_AFTER: u32 = 5;

/// What the page can say about the connection.
///
/// A value rather than a log line, because *"the board you are reading is not live"* is
/// something a person has to be able to see. [`sync_status`] is how it reaches JavaScript.
///
/// `Debug` is safe here and is not derived by reflex: every field is a status word, a count,
/// or a bounded sentence taken from the server's own error body. **No board content reaches
/// this type** — the updates go from the queue into `Board::apply` and are never formatted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Nothing has come back yet. The board on screen is the boot snapshot.
    Connecting,
    /// The last round trip completed. `replies` counts the ones that carried a frame.
    Connected { replies: u64 },
    /// Failing, and still asking. `failures` is the consecutive count.
    Retrying { failures: u32, reason: String },
    /// Failing for long enough that the backoff has reached its ceiling. Still asking.
    Offline { reason: String },
}

impl Status {
    /// One line, for the page.
    pub fn line(&self) -> String {
        match self {
            Self::Connecting => "connecting".to_owned(),
            Self::Connected { replies } => format!("live · {replies} sync(s)"),
            Self::Retrying { failures, reason } => {
                format!("retrying ({failures}) · {reason}")
            }
            Self::Offline { reason } => format!("offline · {reason}"),
        }
    }
}

/// The request that is out, if one is.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    attempt: u64,
    began: Instant,
}

/// Everything the spawned future and the main task both touch.
///
/// ⚠ This is the *whole* boundary. A future spawned by [`Live::poll`] captures an
/// `Rc<RefCell<Shared>>` and nothing else, so there is no path from a network answer to the
/// document except through this struct and then through [`tick`], on the main task, with
/// nothing awaited.
#[derive(Default)]
struct Shared {
    /// Updates that have arrived and not yet been applied. Drained by [`Live::drain`].
    queue: Vec<Vec<u8>>,
    /// The request that is out. `None` means the next tick may ask.
    in_flight: Option<InFlight>,
    /// Requests issued, ever.
    ///
    /// Names the one that is out, so an answer from an attempt the watchdog already gave up
    /// on is discarded rather than clearing a *newer* request's flag or reporting its result.
    attempts: u64,
    /// Consecutive failures, for the backoff. Reset by any reply that parses.
    failures: u32,
    /// Replies that carried a frame. The evidence behind [`Status::Connected`].
    replies: u64,
    /// The earliest the next request may go out because of the *period*. `None` means now.
    next_at: Option<Instant>,
    /// The earliest the next request may go out because a previous one *failed*.
    ///
    /// Separate from `next_at` on purpose: one is a rhythm and the other is a penalty, and
    /// folding them into one field means a success cannot clear the penalty without also
    /// resetting the rhythm.
    retry_at: Option<Instant>,
    /// The last failure's sentence, for [`Status`].
    reason: Option<String>,
}

/// The polling client for one board.
///
/// Lives on the [`Viewer`] and dies with it. There is no close path in this application — a
/// tab holds one board for its life — so there is nothing here to shut down.
pub struct Live {
    /// The full sync URL, built once, **carrying whatever auth the page attached**.
    endpoint: String,
    /// How often a request goes out. Not the tick rate; see the module header.
    period: Duration,
    shared: Rc<RefCell<Shared>>,
}

impl Live {
    /// A client for one board on one server.
    ///
    /// `server` is an origin with or without a trailing slash, and **may be empty**, which is
    /// the normal case: the page is served from the same origin as its boards, so a relative
    /// `/api/v1/…` is what `fetch` should resolve against the document. `docs/08-web.md` §5
    /// records why one origin is the whole hosting decision.
    ///
    /// ⚠ **`board_id` and `token` are already percent-encoded** — they come out of the URL
    /// the page built, and the page is the one authority on that encoding. Re-encoding a
    /// decoded id here would be a second derivation able to disagree with the snapshot fetch
    /// that already worked, and the failure would present as *"the board loads but never
    /// updates"* on exactly the boards whose names are not identifiers.
    ///
    /// The token rides in the query string rather than in an `Authorization` header. That is
    /// not laziness: it is the one idiom this client already has — `crate::images`' `base` +
    /// `suffix` exists precisely because a blob's token has to land after its hash, and the
    /// snapshot fetch and the board list do the same. `velmd`'s `authorised` accepts either.
    /// A header would mean a percent-decoder here, a second auth path, and the `Headers`
    /// web-sys feature, in exchange for hiding a secret that is already in `location.search`.
    pub fn new(server: String, token: Option<String>, board_id: String, period_ms: i32) -> Self {
        let endpoint = endpoint_for(&server, &board_id, token.as_deref());
        let period = Duration::from_millis(period_ms.max(MIN_PERIOD_MS) as u64);
        // Named once, at the top of the log, **with the token stripped**. *"sync failed"*
        // against three possible servers is not a report; the address it is actually asking
        // is the first thing anybody diagnosing this needs, and the last thing that should be
        // pasted into an issue is the credential riding on the end of it.
        log::info!(
            "velm sync: polling {} every {period:?}",
            endpoint.split_once("?token=").map_or(endpoint.as_str(), |(url, _)| url)
        );
        Self { endpoint, period, shared: Rc::new(RefCell::new(Shared::default())) }
    }

    /// Ask, if it is time and nothing is in flight. Never blocks; the answer arrives later.
    ///
    /// `version` is the board's **current** version, taken fresh by the caller after applying
    /// everything that had arrived — see [`tick`], which drains before it asks for exactly
    /// this reason. It governs what comes back and nothing else.
    ///
    /// **One request at a time, and refusing rather than queueing is the whole concurrency
    /// design.** A second request built while the first is out carries a version vector the
    /// first is about to supersede, so its answer is stale before it is asked for — and the
    /// next tick builds a current one for nothing. Without this bound a slow link stacks
    /// requests for ever, which is the failure `crate::images`' in-flight budget already
    /// exists to prevent on the picture path.
    pub fn poll(&mut self, version: Vec<u8>) {
        let now = Instant::now();
        let attempt = {
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
                note_failure(&mut shared, "the request never came back".to_owned(), now);
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
            if shared.next_at.is_some_and(|at| now < at) {
                return;
            }

            shared.attempts = shared.attempts.wrapping_add(1);
            let attempt = shared.attempts;
            shared.in_flight = Some(InFlight { attempt, began: now });
            // Scheduled from the moment this one goes out, not from when it comes back, so a
            // round trip that takes longer than the period is followed immediately rather
            // than by another full period of waiting.
            shared.next_at = now.checked_add(self.period);
            attempt
        };

        // ⚠ The borrow above ends here, before the spawn. `spawn_local` schedules rather than
        // polls, so today the future could not run inside this call anyway — which is exactly
        // the kind of "safe because of what happens to be true elsewhere" this file does not
        // rely on.
        let body = request_body(&version);
        let endpoint = self.endpoint.clone();
        let shared = Rc::clone(&self.shared);
        wasm_bindgen_futures::spawn_local(async move {
            let outcome = round_trip(endpoint, body).await;
            let now = Instant::now();
            let mut shared = shared.borrow_mut();
            // Superseded: the watchdog gave up on this attempt and has already counted it.
            // Returning here rather than clearing the flag is what stops a late answer from
            // reporting on — or cancelling — a request that is genuinely still out.
            if shared.in_flight.map(|out| out.attempt) != Some(attempt) {
                return;
            }
            shared.in_flight = None;
            match outcome {
                Ok(updates) => {
                    shared.failures = 0;
                    shared.retry_at = None;
                    shared.reason = None;
                    shared.replies = shared.replies.wrapping_add(1);
                    // ⚠ An empty update list is the **steady state** of a working sync, not
                    // an error and not something to queue: once the two sides agree, every
                    // round trip answers with a version vector and nothing else. Queueing it
                    // would make `tick` reproject the whole board once a period for a sync
                    // that had nothing to say.
                    if !updates.is_empty() {
                        shared.queue.push(updates);
                    }
                }
                Err(detail) => note_failure(&mut shared, detail, now),
            }
        });
    }

    /// Updates that have arrived since the last call, in the order they came.
    ///
    /// Never blocks. Drained by [`tick`], applied on the same task, before the next ask.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut shared = self.shared.borrow_mut();
        std::mem::take(&mut shared.queue)
    }

    /// What the page should say about the connection.
    pub fn status(&self) -> Status {
        let shared = self.shared.borrow();
        match &shared.reason {
            Some(reason) if shared.failures >= OFFLINE_AFTER => {
                Status::Offline { reason: reason.clone() }
            }
            Some(reason) => {
                Status::Retrying { failures: shared.failures, reason: reason.clone() }
            }
            None if shared.replies > 0 => Status::Connected { replies: shared.replies },
            None => Status::Connecting,
        }
    }

    /// An update came back and could not be merged.
    ///
    /// Counted as a failure so the backoff covers it, because the alternative is a loop: the
    /// local version cannot advance past bytes that will not apply, so the next request asks
    /// for the same range and gets the same answer. Backing off turns that into a slow,
    /// visible, reported failure instead of a fast invisible one.
    pub fn note_apply_failure(&mut self, detail: String) {
        let now = Instant::now();
        let mut shared = self.shared.borrow_mut();
        note_failure(&mut shared, detail, now);
    }

    /// The tab is visible again: drop the schedule so the next tick asks at once.
    ///
    /// Only the *waits* are cleared, never `in_flight` — a request that is genuinely out
    /// stays out, or coming back to a tab would fire a second request against every reply
    /// still travelling.
    fn wake(&mut self) {
        let mut shared = self.shared.borrow_mut();
        shared.next_at = None;
        shared.retry_at = None;
    }
}

/// A client for the board the page is already showing, or `None` if there is nothing to sync
/// with.
///
/// It reads the **snapshot URL `boot` was handed** rather than taking new parameters, and
/// that is the load-bearing choice: the page built that string, with its own
/// `encodeURIComponent` and its own `?token=`, and it is known to work because the board on
/// screen came back through it. Deriving the sync URL from it means sync reaches exactly the
/// boards the snapshot fetch reaches, and adding no parameter to `start` means an updated
/// wasm bundle cannot arrive beside a page that has not been updated to feed it.
///
/// `None` for the development route — `./board.bin` beside the page is a static file with no
/// server behind it, and a static file has nothing to sync with. That falls out of the
/// parse rather than needing a flag.
pub fn from_snapshot_url(snapshot_url: &str, period_ms: i32) -> Option<Live> {
    let (path, query) = match snapshot_url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (snapshot_url, ""),
    };
    let (server, rest) = path.split_once("/api/v1/boards/")?;
    let board_id = rest.strip_suffix("/snapshot")?;
    // A `/` left in the id means this was not the shape it looked like — refuse rather than
    // build a URL with a path segment nobody meant.
    if board_id.is_empty() || board_id.contains('/') {
        return None;
    }
    let token = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    Some(Live::new(server.to_owned(), token, board_id.to_owned(), period_ms))
}

/// Start the poll loop. Called once, from `boot`, after `VIEWER` holds the viewer.
///
/// ⚠ **One interval and one closure for the life of the tab**, deliberately not the
/// `Closure::once` + `forget` per call that [`crate::schedule_frame`] uses for
/// `requestAnimationFrame`. That idiom is right for a one-shot callback and wrong for a
/// recurring one: `forget` leaks the JS wrapper, and a loop that re-armed itself four times a
/// second would leak all day on a device that has no memory to spare. `setInterval` is armed
/// once and never cancelled, because there is no path in this application that closes a
/// board without closing the tab.
pub fn drive() {
    let Some(window) = web_sys::window() else { return };

    let ticker = Closure::<dyn FnMut()>::new(|| tick(false));
    if let Err(error) = window.set_interval_with_callback_and_timeout_and_arguments_0(
        ticker.as_ref().unchecked_ref(),
        TICK_MS,
    ) {
        // Recorded rather than ignored: with no timer nothing here ever runs again, and a
        // board that silently stops updating is the report this module exists to prevent.
        log::warn!("velm sync: the poll timer would not start ({error:?})");
        return;
    }
    ticker.forget();

    // Coming back to a hidden tab. See the module header: a background timer can be throttled
    // to once a minute, so without this a tab returned to after a while is behind for a
    // reason nobody watching could guess.
    if let Some(document) = window.document() {
        let wake = Closure::<dyn FnMut()>::new(|| {
            // Fires on the way out as well as the way back, and there is nothing to do on the
            // way out — asking as a tab is hidden is one request nobody will read the answer
            // to.
            let hidden = web_sys::window()
                .and_then(|window| window.document())
                .is_some_and(|document| document.hidden());
            if !hidden {
                tick(true);
            }
        });
        let listening = document
            .add_event_listener_with_callback("visibilitychange", wake.as_ref().unchecked_ref());
        if let Err(error) = listening {
            log::warn!("velm sync: no visibility listener ({error:?})");
        }
        wake.forget();
    }
}

/// One turn of the loop: apply what arrived, then ask for what is next.
///
/// ⚠ **Drain, apply, reproject, *then* read the version and ask.** The order is the
/// behaviour, not a preference: asking first would send the version the board had *before*
/// the updates sitting in the queue were applied, so the server would dutifully send them
/// again — every poll, for as long as the queue was non-empty. One statement's difference
/// between a sync that converges and one that re-downloads what it already has.
///
/// Enters with nothing borrowed and never awaits, which is what makes the borrow below safe
/// against the frame loop rather than lucky.
fn tick(wake: bool) {
    let Some(held) = crate::VIEWER.with(|slot| slot.borrow().clone()) else { return };
    // A failed borrow means a frame is mid-flight. Nothing is lost: the queue keeps what
    // arrived, `next_at` has not moved, and the next tick is 250ms away.
    let Ok(mut guard) = held.try_borrow_mut() else { return };
    // One deref, so the field accesses below are disjoint borrows of `Viewer` rather than
    // repeated borrows of the whole `RefMut`.
    let viewer: &mut Viewer = &mut guard;

    let updates = match viewer.live.as_mut() {
        Some(live) => {
            if wake {
                live.wake();
            }
            live.drain()
        }
        // A static board, or a page with no server. There is nothing to ask.
        None => return,
    };

    let merged = if updates.is_empty() { Ok(0) } else { merge(viewer, &updates) };

    let version = viewer.board.version().encode();
    let Some(live) = viewer.live.as_mut() else { return };
    match merged {
        Ok(applied) => {
            if applied > 0 {
                log::info!("velm sync: merged {applied} update(s)");
            }
            live.poll(version);
        }
        // Deliberately no ask on this tick. `note_apply_failure` has set a backoff, and the
        // tick that comes after it will ask for the same range again — see that method.
        Err(detail) => live.note_apply_failure(detail),
    }
}

/// Merge arrived updates into the document and re-derive everything that came from it.
///
/// ⚠ **The reproject is unconditional, including on the failure path.** A closure that fails
/// partway has usually already changed the document, and returning early would leave the
/// scene, the R-tree and every layout cache describing a board that no longer exists — so the
/// painter draws stale. That is trap 11's lesson stated one layer out: `Editor::edit`'s own
/// doc comment exists to make "reproject, *then* record" unforgettable, and the native applier
/// pays for it for the same reason. Here the consequence would be items drawn where they used
/// to be, which on a read-only client is indistinguishable from sync not working at all.
///
/// ⚠ **A remote change is not undoable, and there is no undo group here.** `Board::apply`'s
/// own doc says it: undo covers this peer's own edits, so receiving somebody else's change
/// must never let the local user undo work that was not theirs. This client has no undo at
/// all, which makes the point moot today and worth writing down for the day it does not.
fn merge(viewer: &mut Viewer, updates: &[Vec<u8>]) -> Result<usize, String> {
    let mut applied = 0usize;
    let mut failure: Option<String> = None;
    for delta in updates {
        match viewer.board.apply(delta) {
            Ok(()) => applied += 1,
            // The first failure is the one reported; the rest are still attempted, because
            // `apply` is commutative — a later update is not invalidated by an earlier one
            // that would not parse.
            Err(error) => {
                let detail = format!("a {}-byte update did not apply: {error}", delta.len());
                log::warn!("velm sync: {detail}");
                if failure.is_none() {
                    failure = Some(detail);
                }
            }
        }
    }

    // The scene, the R-tree, the bounds and every item's generation.
    //
    // This is what invalidates the three caches downstream, and it does so precisely: an item
    // that did not change keeps its old generation, so a remote move of one sticky reshapes
    // one sticky rather than every block on the board. `crate::text`, `crate::shapes` and
    // `crate::strokes` all key on `Projected::generation` and prune through
    // `retain_visible`, so a deleted item's cache entry goes on the next frame it is not
    // drawn, and a new item cannot collide with a stale entry because `Projection::intern`
    // never reuses a `SceneId`.
    if let Err(error) = viewer.projection.rebuild(&viewer.board) {
        return Err(format!("the board changed and could not be laid out again: {error}"));
    }

    // ⚠ The board's own colour and pattern are **document** state, so a remote change to
    // either is a change to what this tab clears to and what it draws its grid with. Both are
    // cached on the viewer, read once at boot, and a reproject does not touch them — so a
    // merge that stopped at `rebuild` would leave a board whose background had been changed
    // elsewhere drawing the old colour until the tab was reloaded.
    let background = viewer.board.background();
    viewer.clear =
        crate::board::clear_colour(&background, vellum_project::theme::Theme::LIGHT.canvas);
    viewer.background = background;

    // ⚠ **The camera is deliberately not touched.** `boot` fits the board on open; refitting
    // on every merge would yank the view out from under somebody reading it every time
    // anything anywhere on the board moved. `crate::fit_board` is the gesture that means
    // "show me all of it", and it is theirs to make.

    // Nothing else on the viewer comes from the document. The image layer is keyed by content
    // hash, so a picture that appeared fetches itself on the next frame that asks for it and
    // one that disappeared is released by `enforce_budget`.

    match failure {
        Some(detail) => Err(detail),
        None => Ok(applied),
    }
}

/// The connection, as a string, for the page.
///
/// A getter rather than a write into `#velm-status`: that element carries the boot line —
/// the item count, the pixels painted and the camera — which is the diagnostic every
/// screenshot of this page depends on, and overwriting it with a connection word would
/// destroy the one thing that makes a photograph of this client self-describing.
///
/// `chrome.js` gates every control on `typeof mod.fn === 'function'`, so a build without this
/// export simply draws nothing where the indicator would be.
#[wasm_bindgen]
pub fn sync_status() -> String {
    crate::VIEWER.with(|slot| {
        let Some(held) = slot.borrow().clone() else { return "off".to_owned() };
        let Ok(viewer) = held.try_borrow() else { return "busy".to_owned() };
        match viewer.live.as_ref() {
            Some(live) => live.status().line(),
            None => "off".to_owned(),
        }
    })
}

/// Records a failure and sets the earliest time the next attempt may go out.
///
/// A free function rather than a method so both the main task ([`Live::note_apply_failure`],
/// the watchdog) and the spawned future reach one definition of what a failure costs.
fn note_failure(shared: &mut Shared, detail: String, now: Instant) {
    shared.failures = shared.failures.saturating_add(1);
    let wait = backoff_after(shared.failures);
    // `checked_add` because `Instant` arithmetic can overflow at the end of the monotonic
    // clock's range. `None` there means "hold nothing back", which is the safe direction: the
    // worst case is one extra request against a server that is down.
    shared.retry_at = now.checked_add(wait);
    log::warn!("velm sync: {detail}; retrying in {wait:?}");
    shared.reason = Some(detail);
}

/// How long to wait after `failures` consecutive failures.
///
/// Pure, and it is the piece of this file most worth moving somewhere it can be tested — see
/// the module note in the report. `vellum-app`'s `backoff_after` is the same schedule and has
/// the test this one cannot have.
fn backoff_after(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // Clamped *before* the shift. The failure count is unbounded — a tablet left on a table
    // over a weekend against a server that never answers comes back with a five-figure count
    // — and `1u32 << 32` is a debug panic and a wrapped shift in release. Sixteen steps is
    // already far past the cap.
    let steps = (failures - 1).min(16);
    let factor = 1u32 << steps;
    BACKOFF_BASE.checked_mul(factor).unwrap_or(BACKOFF_CAP).min(BACKOFF_CAP)
}

/// Builds the sync URL for one board.
///
/// Separate from [`Live::new`] so the two things that are easy to get wrong — a doubled
/// slash, and where the token goes — are in one place. An empty `server` yields a relative
/// URL, which is what one-origin hosting wants.
fn endpoint_for(server: &str, board_id: &str, token: Option<&str>) -> String {
    let base = server.trim_end_matches('/');
    let mut url = String::with_capacity(base.len() + board_id.len() + 32);
    url.push_str(base);
    url.push_str("/api/v1/boards/");
    url.push_str(board_id);
    url.push_str("/sync");
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        url.push_str("?token=");
        // Already percent-encoded by the page — see [`Live::new`].
        url.push_str(token);
    }
    url
}

/// The request body: a version vector, and nothing behind it.
///
/// ⚠ **There is no delta parameter, and that is the point.** The wire format has room for one
/// — `velmd`'s handler reads whatever follows the version vector and merges it — and this
/// client must never fill it. Writing a general `frame(version, payload)` here would leave a
/// send path one argument away from existing, in a file whose whole safety argument is that
/// it cannot write to a board. So the payload is absent by construction rather than empty by
/// convention.
///
/// ⚠ **A length that does not fit the header becomes `u32::MAX`, not a truncated value.** A
/// version vector cannot reach 4GB — it is a few bytes per peer — but `as u32` on one that did
/// would keep the low 32 bits and produce a frame whose prefix disagrees with its contents,
/// which the server would split at the wrong offset and *accept*. `u32::MAX` is unparseable
/// by `velmd`'s `split_frame` by construction, so the impossible case fails loudly at the
/// first hop rather than quietly corrupting a request.
fn request_body(version: &[u8]) -> Vec<u8> {
    let length = u32::try_from(version.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + version.len());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(version);
    out
}

/// Splits a reply into its version vector and the updates behind it.
///
/// Panic-free **by construction** rather than by argument: every read goes through `get`, so a
/// truncated body, a body shorter than the header, and a header claiming more than the body
/// holds all return an error instead of slicing. That distinction matters more here than it
/// usually would — `[profile.release]` sets `panic = "abort"`, so an out-of-range slice would
/// not raise an error a caller could report, it would take the tab down. The same class of bug
/// reached the user twice through `strip_site_affix`.
///
/// The bytes are borrowed rather than copied: an update can be megabytes and the caller copies
/// exactly the half it keeps.
///
/// ⚠ **This is the fourth copy of this function in the repository** — `velmd::sync`'s
/// `split_frame`, `vellum_app::sync`'s `unframe`, and this one, against one `frame` in each of
/// the first two. `vellum-doc` is the only crate all three depend on, and `version.rs` already
/// says a `Version` is encoded *"for the body of a sync request or response"*. The framing
/// belongs there; the report that came with this file names the exact split.
fn unframe(bytes: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let Some(header) = bytes.get(..4).and_then(|head| <[u8; 4]>::try_from(head).ok()) else {
        return Err(format!(
            "a sync frame needs 4 header bytes and this one has {}",
            bytes.len()
        ));
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
async fn round_trip(endpoint: String, body: Vec<u8>) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;

    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    // `Uint8Array::from` copies, so this does not hand JavaScript a view into wasm memory
    // that a later allocation could move out from under it.
    let payload = js_sys::Uint8Array::from(body.as_slice());
    init.set_body_opt_u8_array(Some(&payload));
    // See [`REQUEST_TIMEOUT_MS`]: one signal covers the fetch, the body read and the error
    // read, because aborting a request aborts its body stream with it.
    init.set_signal(Some(&web_sys::AbortSignal::timeout_with_u32(REQUEST_TIMEOUT_MS)));
    // ⚠ **No `Content-Type`, deliberately.** `velmd` needs a `Content-Length` — which `fetch`
    // sets itself for a buffer body — and never looks at the type. Leaving it off also keeps
    // this a CORS-simple request, so a cross-origin deployment does not pay a preflight for a
    // header nothing reads.

    let response = wasm_bindgen_futures::JsFuture::from(
        window.fetch_with_str_and_init(&endpoint, &init),
    )
    .await
    .map_err(|_| format!("could not reach {endpoint}"))?;
    let response: web_sys::Response =
        response.dyn_into().map_err(|_| "that is not a response".to_owned())?;

    let status = response.status();
    if !response.ok() {
        // ⚠ The server's own words, not the number. *"sync failed"* against a wrong token is
        // a report nobody can act on; *"a bearer token is required"* is one they can, and it
        // is the sentence `velmd` already writes into the body of its 401.
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

    // Before the copy, never after. See [`MAX_REPLY_BYTES`].
    if buffer.byte_length() > MAX_REPLY_BYTES {
        return Err(format!(
            "the reply is {} bytes, past the {MAX_REPLY_BYTES}-byte ceiling",
            buffer.byte_length()
        ));
    }
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();

    // The server's own version vector is parsed and thrown away — see the module header. A
    // reader's only marker is its own `board.version()`, taken fresh on the next ask.
    let (_version, updates) = unframe(&bytes)?;
    Ok(updates.to_vec())
}

/// The text of an error response, bounded.
///
/// ⚠ **Bounded by characters, never by bytes.** `velmd`'s own error bodies are ASCII, but a
/// proxy or a load balancer in front of it can answer with anything, and slicing a `String`
/// at a byte offset aborts the process on any multi-byte character straddling the boundary.
/// Feedback 30, twice, and feedback 34 again.
async fn read_text(response: &web_sys::Response) -> Option<String> {
    let promise = response.text().ok()?;
    let value = wasm_bindgen_futures::JsFuture::from(promise).await.ok()?;
    let text = value.as_string()?;
    Some(text.trim().chars().take(200).collect())
}
