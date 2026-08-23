//! Two-way sync with `velmd`, off the frame loop.
//!
//! `docs/08-web.md` records the state this closes: *"The client is a reader. No editing, no
//! sync, so a board changed on the Mac has to be re-migrated."* This is the Mac half of the
//! round trip — one POST per board, on a worker thread, answered through a channel that the
//! frame loop drains.
//!
//! # The wire
//!
//! ```text
//! POST /api/v1/boards/{id}/sync
//!   request   [u32 le: length of the version vector][vv bytes][delta bytes]
//!   response  [u32 le: length of the version vector][vv bytes][updates bytes]
//! ```
//!
//! One round trip, no request ids, no retry bookkeeping and no conflict resolution — because
//! Loro is a CRDT and [`vellum_doc::Board::apply`] is commutative and idempotent. An update
//! applied twice is the same board as an update applied once, so the only thing a dropped
//! reply costs is a round trip. That is what makes this module small enough to be honest
//! about: there is no state machine here, only a request in flight or not.
//!
//! # Why this module never names `Board`
//!
//! ⚠ **The document must be applied on the main thread, and the mechanical guarantee that it
//! is happens to be the shortest one available: nothing in this file can see a board.** It
//! takes bytes and answers bytes. There is no import of `vellum_doc` and no board type in any
//! signature, so a worker thread here *cannot* apply an update even by accident, in this
//! version or in a later one written by somebody who has not read this paragraph.
//!
//! Two reasons the apply belongs on the main thread, and the second is the one a reader
//! usually misses:
//!
//! - **The document is never shared across threads in this application.** `vellum-store`'s
//!   autosave writer already establishes the pattern and it is the pattern worth copying:
//!   the *bytes* cross the thread boundary, never the document. [`crate::editor::Editor`]
//!   owns the board outright.
//! - **A document change is not finished until the projection is rebuilt.** `Editor::edit`'s
//!   own doc comment exists to make that unforgettable — a board mutated without a reproject
//!   draws stale *and* hit-tests stale, so clicks land where items used to be. A worker that
//!   applied an update would have changed the document and left the R-tree, the layout cache
//!   and the painter's epoch all describing the board as it was.
//!
//! # ⚠ Trap 11: a remote update must **wait** for an open undo group, never close one
//!
//! Two gestures hold a Loro undo group open across many calls on purpose — the on-canvas
//! caret, so a typed word is one `⌘Z`, and the eraser's sweep, so one gesture is one undo.
//! Loro has no depth count, so any *other* grouped operation attempted while one is live
//! fails with `UndoGroupAlreadyStarted`, and because nothing else ever closes a group it then
//! fails for the rest of the session.
//!
//! This is exactly the shape of `apply_link_fetches`, found by an adversarial review after
//! the first round of undo-group fixes was declared done (feedback 30): a background answer
//! landing mid-edit re-raised the very toast the session had just removed, from a path no
//! keystroke and no command goes through. The fix there is the fix here, and the distinction
//! is worth restating because it is the whole reason there are two correct answers to
//! `ActiveState::busy_with_a_group`:
//!
//! - A **command** is something the user just asked for, so closing their edit to serve it is
//!   reasonable — `ActiveState::run` calls `settle`.
//! - A **remote update** is not. Ending somebody's half-typed sticky because a collaborator
//!   moved a frame is a worse bug than the one being avoided. So the *applier* returns early
//!   and the update is held on the `Editor` until the gesture ends. Nothing is lost by
//!   waiting, which is the property that makes waiting legal.
//!
//! ⚠ **It is held on the editor rather than left in this channel, and that was a correction.**
//! Draining is what clears `outstanding`, so a caller that skipped the drain while a gesture
//! was open stopped asking altogether — sync stalled in both directions for as long as
//! somebody was typing. Drain every frame; hold what cannot be applied yet.
//!
//! # Remote edits are not undoable, and that is correct
//!
//! `Board::apply`'s own doc comment says it: *"Applied changes are not undoable: undo covers
//! this peer's own edits, so replaying a log or receiving a collaborator's change never lets
//! the local user undo work that was not theirs."* A `⌘Z` that reverted somebody else's move
//! would be a local edit deleting a remote one, which is the one way a CRDT can still lose
//! work. So the applier opens **no undo group at all** — not a nested one, not a fresh one.
//!
//! # RULE ZERO
//!
//! Nothing here deletes or truncates anything. This module opens no file, holds no path and
//! has no board; the only durable consequence of a sync is that `Editor::edit` records the
//! applied bytes through the autosave writer that every other edit already goes through. A
//! request that is refused, times out or comes back malformed loses nothing either: the delta
//! is re-derived next time from the board plus [`Sync::since`], both of which are still here.
//!
//! # The worker
//!
//! **One thread, not two.** `crate::links` runs a pool of two because it is fetching from
//! many hosts at once and one slow server must not block the rest. This talks to exactly one
//! server about exactly one board, so a second worker would only let two round trips overlap
//! — which buys nothing (the second would carry a version vector already superseded by the
//! first) and costs the guarantee that replies arrive in the order they were asked for. So
//! the `Receiver` is moved straight into the thread rather than shared behind an
//! `Arc<Mutex<…>>` the way the fetch pool's is.
//!
//! It does not spin: it blocks on `recv`, and it returns when the `Sender` drops, which is
//! what happens when the app drops its [`Sync`]. **The backoff after a failure is on the main
//! thread**, in [`Sync::request`], rather than as a `sleep` in the worker. Two reasons, and
//! both were paid for elsewhere in this codebase: a schedule that is pure arithmetic on the
//! caller's side is testable without a clock and without a socket (the `glass_budget` lesson —
//! an assertion against wall time fails on a loaded machine and teaches nobody anything), and
//! a worker that is never asleep cannot delay a quit.

use std::io::Read;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

use crate::time::Instant;

/// Named so a server log can tell a Velm desktop apart from the wasm client, which fetches
/// as the browser.
const USER_AGENT: &str = concat!("Velm/", env!("CARGO_PKG_VERSION"), " (sync)");

/// Long enough for a home router waking a sleeping server, short enough that a wrong address
/// is reported rather than hung on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A deadline for the whole exchange.
///
/// A global deadline is the right shape **here specifically**, and the reason is worth stating
/// because it does not generalise to every HTTP call in an application: a sync is one bounded
/// request and one bounded response, so there is nothing that legitimately takes longer and a
/// clock can only ever be catching a fault. Put the same deadline over a long-lived *streamed*
/// exchange and it does the opposite — it cuts off an answer that was arriving correctly, and
/// reports the timeout as though the far end had gone quiet.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The most a reply may be, in bytes.
///
/// The point is not to bound a board, it is to bound a *wrong address*: a URL that answers
/// with a media stream must not be read into memory on a machine that has kernel-panicked
/// twice. 32MB is roughly thirty times the largest payload measured anywhere in this
/// repository — the reference Miro board's entire clipboard export is 1.1MB — so a real
/// board never approaches it, and a reply that does is named rather than truncated, because
/// a silently short update is a board that syncs and is wrong.
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// The first wait after a failure. Doubles per consecutive failure up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_secs(2);

/// The longest this will ever wait between attempts.
///
/// A minute rather than an hour: the common failure is a laptop that has moved between
/// networks, and the user's evidence that sync is working again should arrive within the
/// time it takes them to notice they are back.
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// What came back from one round trip.
///
/// `Debug` is written by hand rather than derived, because these fields are *board content*.
/// A derived `Debug` would put a user's document into any log line that ever formatted a
/// reply, and the useful thing to log is the size, which is what this prints. It is a shape
/// an adversarial review of this repository has caught before, on a struct whose own doc
/// comment promised it was never printed: the promise is not the mechanism, the hand-written
/// `Debug` is.
pub enum SyncReply {
    /// The server accepted the delta and answered.
    Synced {
        /// The server's own version vector, exactly as it arrived. This becomes
        /// [`Sync::since`] — see that method for why it is the server's and not ours.
        version: Vec<u8>,
        /// Everything the client was missing, for `Board::apply`. **May be empty**, and that
        /// is the steady state rather than an error: once the two sides agree, every sync
        /// answers with nothing to do. See [`SyncReply::changes_the_board`].
        updates: Vec<u8>,
    },
    /// The round trip did not complete, or completed with something that is not a frame.
    /// Carries a sentence fit to put in a toast.
    Failed { detail: String },
}

impl SyncReply {
    /// Whether this reply has anything for `Board::apply`.
    ///
    /// ⚠ The applier must ask this rather than calling `apply` unconditionally. Handing Loro
    /// a zero-byte update is asking the document layer a question about nothing, and the
    /// interesting cost is not the call — it is that the caller wraps it in `Editor::edit`,
    /// which reprojects the whole document and asks the autosave writer for a delta. On a
    /// 1,300-item board, once a second, for a sync that had nothing to say.
    pub fn changes_the_board(&self) -> bool {
        matches!(self, Self::Synced { updates, .. } if !updates.is_empty())
    }
}

impl std::fmt::Debug for SyncReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Synced { version, updates } => f
                .debug_struct("Synced")
                .field("version_bytes", &version.len())
                .field("update_bytes", &updates.len())
                .finish(),
            Self::Failed { detail } => f.debug_struct("Failed").field("detail", detail).finish(),
        }
    }
}

/// The worker, its channels, and everything the main thread needs to decide whether to ask.
///
/// ⚠ **This type shadows `std::marker::Sync` inside this module.** It compiles — no trait
/// bound in this file names `Sync`, and `thread::Builder::spawn` asks only for `Send` — but a
/// `where T: Sync` written here later would silently resolve to this struct and fail to
/// compile with a message about the wrong thing entirely. If that ever becomes tempting,
/// spell it `std::marker::Sync`.
pub struct Sync {
    /// The full URL, built once. Kept for the HUD and for the error text, which is useless
    /// without it — *"sync failed"* against three possible servers is not a report.
    endpoint: String,
    outbound: Sender<Vec<u8>>,
    inbound: Receiver<SyncReply>,
    /// Whether a request is out. One at a time, deliberately — see [`Sync::request`].
    outstanding: bool,
    /// The server's version vector as of the last reply, which is what the *next* delta is
    /// exported since. See [`Sync::since`].
    since: Option<Vec<u8>>,
    /// Consecutive failures, for the backoff schedule. Reset by any reply that parses.
    failures: u32,
    /// The earliest a new request may go out. `None` means now.
    retry_after: Option<Instant>,
    /// The last failure's sentence, for the HUD and for a toast.
    last_error: Option<String>,
}

impl Sync {
    /// Starts the worker. It lives for the process and idles on an empty channel.
    ///
    /// `server` is the origin, with or without a trailing slash — `http://127.0.0.1:8787` or
    /// `https://boards.example.com`. `token` is velmd's bearer token and **may be empty**: a
    /// server bound to loopback needs none, and velmd refuses to bind a public address
    /// without one, so "no token" is a legitimate configuration rather than a mistake.
    ///
    /// `board_id` is the board's file stem, which is what `velmd`'s `boards_json` names a
    /// board by. Real stems have spaces in them, so it is percent-encoded into the path —
    /// see [`escape_segment`].
    pub fn new(server: String, token: String, board_id: String) -> Self {
        let endpoint = endpoint_for(&server, &board_id);
        let (outbound, requests) = channel::<Vec<u8>>();
        let (answers, inbound) = channel::<SyncReply>();

        // The URL and the token are moved in rather than travelling with each request: they
        // do not change for the life of this `Sync`, and a request that carried its own
        // address is a request that could be pointed somewhere else by a bug upstream.
        let worker_endpoint = endpoint.clone();
        let spawned = std::thread::Builder::new().name("velm-sync".to_owned()).spawn(move || {
            // Built once so connections are pooled across syncs. A round trip every few
            // seconds over a fresh TLS handshake each time is most of the cost of syncing at
            // all on a remote server.
            let http = agent();
            // Exits when the `Sender` drops, which is what dropping the `Sync` does. No
            // shutdown flag, no poison value, nothing to forget to send.
            while let Ok(body) = requests.recv() {
                let reply = post(&http, &worker_endpoint, &token, body);
                // The receiver is gone: the app is shutting down.
                if answers.send(reply).is_err() {
                    return;
                }
            }
        });
        if let Err(error) = spawned {
            log::warn!("sync: the worker would not start ({error})");
        }

        Self {
            endpoint,
            outbound,
            inbound,
            outstanding: false,
            since: None,
            failures: 0,
            retry_after: None,
            last_error: None,
        }
    }

    // `pub fn endpoint()` was written here and removed: it claimed to be *"for the HUD and
    // for error text"*, and it had no caller anywhere in the workspace — the HUD carries no
    // sync line, and the error text reads the field directly two functions below. An accessor
    // whose doc names two consumers that do not exist is worse than no accessor: it reads as
    // evidence that a feature is wired.

    /// The version vector the next delta should be exported since, or `None` on a fresh
    /// [`Sync`] that has never had a reply.
    ///
    /// ⚠ **This is the *server's* version, not the last one we sent, and the difference is a
    /// bug worth naming rather than a preference.** The obvious marker — "the version vector
    /// I sent last time" — echoes the server's own work back at it one round later: a reply's
    /// updates are imported into the local document, so they become local oplog entries, and
    /// `export_since(what_I_had_before_importing_them)` includes every one of them. A fresh
    /// client against a populated board would re-upload the entire board the round after it
    /// downloaded it.
    ///
    /// Exporting since the *server's* version answers the question actually being asked —
    /// what does the server not have — and it self-heals: it also picks up anything edited
    /// locally while the request was in flight, and if a reply's updates are ever dropped
    /// before they are applied, the next request still carries the client's true current
    /// version in the header, so the server sends them again.
    ///
    /// `None` means the server has nothing of ours, so the caller should export since the
    /// empty version — the whole document.
    pub fn since(&self) -> Option<&[u8]> {
        self.since.as_deref()
    }

    /// Whether [`Sync::request`] would accept one right now: nothing in flight, and past any
    /// backoff a previous failure imposed.
    pub fn is_ready(&self) -> bool {
        !self.outstanding && self.retry_after.is_none_or(|at| Instant::now() >= at)
    }

    /// Whether a round trip is out.
    pub const fn outstanding(&self) -> bool {
        self.outstanding
    }

    /// The last failure's sentence, if the last round trip failed.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Sends one round trip. Never blocks, never touches the network on this thread.
    ///
    /// `vv` is the client's **current** version — what it has — and decides what comes back.
    /// `delta` is what the client has done that the server lacks, which the caller derives
    /// from [`Sync::since`]. The two are different questions and it is worth keeping them
    /// visibly different: `vv` governs the download, `since` governs the upload.
    ///
    /// **An empty `delta` is a legitimate request**, and it is the one that makes remote
    /// changes arrive at all: a client with nothing to say still has to ask, or a board
    /// edited on another machine never appears. The framing carries an empty payload without
    /// a special case, which is why there is no `Option` here.
    ///
    /// Returns whether the request went, so the caller can report "syncing" rather than
    /// guessing. It is refused, without loss, when one is already out or when a previous
    /// failure's backoff has not elapsed. Refusing rather than queueing is the whole
    /// concurrency design: a second request built from a version vector the first is about to
    /// supersede is work whose answer is already stale, and the next frame will build a
    /// current one for free.
    pub fn request(&mut self, vv: Vec<u8>, delta: Vec<u8>) -> bool {
        if !self.is_ready() {
            return false;
        }
        let body = frame(&vv, &delta);
        if self.outbound.send(body).is_err() {
            // The worker is gone — it failed to spawn, or it panicked. Recorded rather than
            // ignored, and `outstanding` deliberately left false: `Fetcher::request`'s own
            // rescue, for the same reason. Marking a request as out when nothing is coming
            // back would refuse every later attempt for the life of the process, so the one
            // failure would present as sync having silently stopped.
            self.note_failure("the sync worker is gone");
            return false;
        }
        self.outstanding = true;
        true
    }

    /// Everything that has come back since the last call. Never blocks.
    ///
    /// ⚠ **Call this every frame, and guard the *apply* rather than the drain.** This method
    /// is what clears `outstanding`, so a caller that skips it while a gesture is open stops
    /// asking altogether: one reply arriving mid-word refused every later request, and a
    /// caret left open stopped sync for good. What must not happen is a reply drained and
    /// **dropped** — the version vector has not moved, so the server would resend eventually,
    /// but the board draws the old state until it does. `ActiveState::apply_sync` holds what
    /// it cannot apply yet, on the `Editor`, and applies it when the gesture ends.
    pub fn drain(&mut self) -> Vec<SyncReply> {
        let mut out = Vec::new();
        loop {
            match self.inbound.try_recv() {
                Ok(reply) => {
                    self.outstanding = false;
                    match &reply {
                        SyncReply::Synced { version, .. } => {
                            self.since = Some(version.clone());
                            self.failures = 0;
                            self.retry_after = None;
                            self.last_error = None;
                        }
                        SyncReply::Failed { detail } => self.note_failure(detail),
                    }
                    out.push(reply);
                }
                Err(TryRecvError::Empty) => return out,
                Err(TryRecvError::Disconnected) => {
                    // The worker died mid-request. Without this, `outstanding` would stay
                    // true for ever and `request` would refuse every attempt afterwards —
                    // the same stuck state the send-failure path above avoids, reached from
                    // the other side. Recorded once, because `outstanding` is only true
                    // after a send that succeeded.
                    if self.outstanding {
                        self.outstanding = false;
                        self.note_failure("the sync worker stopped");
                    }
                    return out;
                }
            }
        }
    }

    /// Records a failure and sets the next attempt's earliest time.
    fn note_failure(&mut self, detail: &str) {
        self.failures = self.failures.saturating_add(1);
        let wait = backoff_after(self.failures);
        // `checked_add` because `Instant` arithmetic can overflow at the end of the monotonic
        // clock's range. `None` there means "do not hold anything back", which is the safe
        // direction: the worst case is one extra request against a server that is down.
        self.retry_after = Instant::now().checked_add(wait);
        log::warn!("sync: {} ({detail}); retrying in {wait:?}", self.endpoint);
        self.last_error = Some(detail.to_owned());
    }
}

/// How long to wait after `failures` consecutive failures.
///
/// Pure, so the schedule can be pinned by a test that neither sleeps nor opens a socket. A
/// budget asserted against wall time is a test that fails on a loaded machine and teaches
/// nobody anything — `vellum-render`'s `glass_budget.rs` is the standing example.
fn backoff_after(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // Clamped *before* the shift rather than after. The failure count is unbounded — a laptop
    // whose lid closes on a Friday against a server that never answers comes back with a
    // five-figure count — and `1u32 << 32` is undefined behaviour's well-defined cousin: a
    // debug panic and a wrapped shift in release. Sixteen steps is already far past the cap.
    let steps = (failures - 1).min(16);
    let factor = 1u32 << steps;
    BACKOFF_BASE.checked_mul(factor).unwrap_or(BACKOFF_CAP).min(BACKOFF_CAP)
}

/// Builds the sync URL for one board.
///
/// Separate from [`Sync::new`] so the two things that are easy to get wrong — a doubled
/// slash, and a board name with a space in it — are testable without starting a thread.
fn endpoint_for(server: &str, board_id: &str) -> String {
    let base = server.trim_end_matches('/');
    format!("{base}/api/v1/boards/{}/sync", escape_segment(board_id))
}

/// Percent-encodes one path segment.
///
/// ⚠ **A board id is a file stem and real ones have spaces in them** — `velmd`'s
/// `boards_json` names a board by `path.file_stem()`, and this repository's own reference
/// board is *"BMW 2020 530i g30"*. A raw space in a request line is a malformed HTTP request,
/// which fails as something else entirely.
///
/// Encodes **bytes**, not characters, which is what makes a non-ASCII board name correct
/// rather than merely tolerated: a `%` escape describes one byte of UTF-8, and iterating
/// `chars()` here would produce escapes no server can decode. The unreserved set is RFC 3986's.
fn escape_segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for &byte in id.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Lays a version vector and a payload out as the wire format, both directions.
///
/// One function for the request and the response because they are the same frame; two would
/// be two places for the endianness to disagree.
///
/// ⚠ **A length that does not fit in the header becomes `u32::MAX`, not a truncated value.**
/// A version vector cannot reach 4GB — it is a few bytes per peer — but `as u32` on a length
/// that did would keep the low 32 bits and produce a frame whose prefix disagrees with its
/// contents, which the far side would split at the wrong offset and accept. `u32::MAX` is
/// unparseable by [`unframe`] by construction, so the impossible case fails loudly at the
/// first hop instead of quietly corrupting a document.
pub fn frame(version: &[u8], payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(version.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + version.len() + payload.len());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(version);
    out.extend_from_slice(payload);
    out
}

/// Splits a frame back into its version vector and its payload.
///
/// Panic-free **by construction** rather than by argument: every read goes through `get`, so
/// a truncated body, a body shorter than the header, and a header claiming more than the body
/// holds all return an error instead of slicing. That distinction matters here more than it
/// usually would — `[profile.release]` sets `panic = "abort"`, so an out-of-range slice in
/// this function would not raise an error the caller could report, it would kill the
/// application. The same class of bug reached the user twice through `strip_site_affix`.
///
/// The bytes are borrowed rather than copied: an update can be megabytes, and the caller
/// copies exactly the half it keeps.
pub fn unframe(bytes: &[u8]) -> Result<(&[u8], &[u8]), FrameError> {
    let prefix: [u8; 4] = bytes
        .get(..4)
        .and_then(|head| head.try_into().ok())
        .ok_or(FrameError::TooShort { got: bytes.len() })?;
    // `usize` is at least 32 bits on every target Velm builds for, wasm32 included, so this
    // widening cannot lose a byte of the length.
    let claimed = u32::from_le_bytes(prefix) as usize;
    let rest = bytes.get(4..).unwrap_or_default();
    let version = rest
        .get(..claimed)
        .ok_or(FrameError::LengthBeyondBody { claimed, available: rest.len() })?;
    let payload = rest.get(claimed..).unwrap_or_default();
    Ok((version, payload))
}

/// Why a frame could not be read.
///
/// A typed error rather than a `String` so the applier can say *"the server answered
/// something that is not a sync frame"* and put the numbers in the log, which is the
/// difference between diagnosing a proxy that returned an HTML error page and guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Fewer than the four header bytes. An empty body lands here, which is the shape of a
    /// 204 or of a proxy that closed the connection.
    TooShort { got: usize },
    /// The header claims a version vector longer than what followed it.
    LengthBeyondBody { claimed: usize, available: usize },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { got } => {
                write!(f, "a sync frame needs 4 header bytes and this one has {got}")
            }
            Self::LengthBeyondBody { claimed, available } => write!(
                f,
                "the frame claims a {claimed}-byte version vector and carries {available} bytes"
            ),
        }
    }
}

impl std::error::Error for FrameError {}

/// The HTTP agent. One per worker, so connections are pooled across syncs.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        // A non-2xx is a *response*, not a transport failure: velmd's body carries the reason
        // — *"a bearer token is required"* — and that sentence is the only useful thing to put
        // in front of the user. `http_status_as_error` would throw it away and leave a toast
        // reading "401".
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into()
}

/// One round trip, on the worker thread.
///
/// Never returns an error type: every failure is a [`SyncReply::Failed`] carrying a sentence,
/// because the caller's only response to any of them is the same — report it and try again
/// later — and a `Result` would invite an early `?` on a path whose whole job is to always
/// answer.
fn post(http: &ureq::Agent, endpoint: &str, token: &str, body: Vec<u8>) -> SyncReply {
    let mut request = http.post(endpoint).header("content-type", "application/octet-stream");
    // Only when there is one. A velmd bound to loopback needs no token and rejects nothing,
    // but sending `Authorization: Bearer ` — a header with an empty credential — is a
    // malformed request that some proxies answer with a 400 of their own.
    if !token.is_empty() {
        request = request.header("authorization", format!("Bearer {token}"));
    }

    let response = match request.send(body) {
        Ok(response) => response,
        Err(error) => return SyncReply::Failed { detail: error.to_string() },
    };
    let status = response.status().as_u16();

    // `take(cap + 1)` rather than `take(cap)`: reading one byte past the ceiling is what makes
    // "was this truncated" a fact rather than a guess about a body that happened to be exactly
    // the cap long. A truncated update is the one failure that must never be applied — Loro
    // would reject it, but a decoder that happened not to would write a half-board.
    let mut bytes = Vec::new();
    let read = response
        .into_body()
        .into_reader()
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes);
    if let Err(error) = read {
        return SyncReply::Failed { detail: format!("reading the reply: {error}") };
    }
    if bytes.len() > MAX_RESPONSE_BYTES {
        return SyncReply::Failed {
            detail: format!("the reply is larger than the {MAX_RESPONSE_BYTES}-byte ceiling"),
        };
    }

    parse_reply(status, &bytes)
}

/// Turns a status and a body into a reply.
///
/// Split out of [`post`] and pure, because this is where every interesting decision is and
/// none of it needs a socket: what a 401 says, what an empty body means, and — the one that
/// matters most — that a **200 with no updates is a success**, not an empty failure. That
/// last case is the steady state of a working sync rather than an edge: once the two sides
/// agree, every round trip answers with a version vector and nothing else. A build that
/// reported it as an error would put a warning in front of the user for the whole time
/// everything was fine — and the mirror of it, a failure quietly reported as a success, is a
/// defect an adversarial review of this repository has found more than once.
fn parse_reply(status: u16, body: &[u8]) -> SyncReply {
    if !(200..300).contains(&status) {
        // Truncated by **characters**, never by bytes: velmd's own error bodies are ASCII, but
        // a proxy or a load balancer sitting in front of it can answer with anything, and
        // `&text[..300]` aborts the process on any multi-byte character straddling the
        // boundary. Feedback 30, twice.
        let detail: String = String::from_utf8_lossy(body).trim().chars().take(300).collect();
        let detail = if detail.is_empty() { format!("HTTP {status}") } else { detail };
        return SyncReply::Failed { detail: format!("HTTP {status}: {detail}") };
    }
    match unframe(body) {
        Ok((version, updates)) => {
            SyncReply::Synced { version: version.to_vec(), updates: updates.to_vec() }
        }
        Err(error) => SyncReply::Failed { detail: error.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port that nothing is listening on, so the worker's attempt is refused by the kernel
    /// immediately and no packet leaves the machine. Never asserted on — these tests are about
    /// what the *caller* does, and a test whose result depends on a socket is a test that
    /// fails on somebody else's network.
    const DEAD: &str = "http://127.0.0.1:9";

    #[test]
    fn a_frame_survives_a_round_trip() {
        let framed = frame(b"version", b"the delta");
        let (version, payload) = unframe(&framed).expect("a frame this function just built");
        assert_eq!(version, b"version");
        assert_eq!(payload, b"the delta");
    }

    /// The poll. A client with nothing to say still has to ask, or a board edited on another
    /// machine never arrives — so an empty payload is the *common* frame, not an edge one.
    #[test]
    fn an_empty_delta_is_a_frame_like_any_other() {
        let framed = frame(b"version", b"");
        let (version, payload) = unframe(&framed).expect("an empty payload is legal");
        assert_eq!(version, b"version");
        assert!(payload.is_empty());
    }

    /// The first sync, from the other side: a client the server has never heard of sends an
    /// empty version vector and the whole document behind it.
    #[test]
    fn an_empty_version_vector_is_a_frame_like_any_other() {
        let framed = frame(b"", b"everything");
        assert_eq!(&framed[..4], &0u32.to_le_bytes(), "the header says zero, not nothing");
        let (version, payload) = unframe(&framed).expect("an empty version vector is legal");
        assert!(version.is_empty());
        assert_eq!(payload, b"everything");
    }

    /// Every way a body can be too short, through `get` rather than a slice. With
    /// `panic = "abort"` in the release profile, the alternative to an error here is not a
    /// caught panic — it is the application dying on the frame a reply arrives.
    #[test]
    fn a_truncated_body_is_an_error_and_not_a_panic() {
        for short in [&b""[..], &b"a"[..], &b"ab"[..], &b"abc"[..]] {
            assert_eq!(
                unframe(short),
                Err(FrameError::TooShort { got: short.len() }),
                "{short:?} was read as a frame"
            );
        }
    }

    /// A header that claims more than followed it — a reply cut off by a proxy, or a frame
    /// built by something that is not this function.
    #[test]
    fn a_length_beyond_the_body_is_an_error() {
        let mut framed = frame(b"version", b"the delta");
        framed.truncate(4 + 3);
        assert_eq!(
            unframe(&framed),
            Err(FrameError::LengthBeyondBody { claimed: 7, available: 3 })
        );

        // And the impossible-length case `frame` produces deliberately rather than truncating.
        let mut hostile = u32::MAX.to_le_bytes().to_vec();
        hostile.extend_from_slice(b"nothing like that much");
        assert!(matches!(
            unframe(&hostile),
            Err(FrameError::LengthBeyondBody { claimed, .. }) if claimed == u32::MAX as usize
        ));
    }

    /// The steady state of a working sync: the server has our edits and we have its, so it
    /// answers with a version and nothing to do. This must be a success the caller can
    /// recognise as a no-op — not an error, and not something that reaches `Board::apply`.
    #[test]
    fn a_reply_with_no_updates_is_a_success_and_a_no_op() {
        let reply = parse_reply(200, &frame(b"server-version", b""));
        let SyncReply::Synced { version, updates } = &reply else {
            panic!("an empty update list is not a failure: {reply:?}");
        };
        assert_eq!(version, b"server-version");
        assert!(updates.is_empty());
        assert!(!reply.changes_the_board(), "nothing should reach the document");
    }

    #[test]
    fn a_reply_with_updates_reaches_the_document() {
        let reply = parse_reply(200, &frame(b"server-version", b"an update"));
        assert!(reply.changes_the_board());
        let SyncReply::Synced { updates, .. } = &reply else { panic!("expected a sync") };
        assert_eq!(updates, b"an update");
    }

    /// velmd's own words reach the user. *"sync failed"* against a wrong token is a report
    /// nobody can act on; *"a bearer token is required"* is one they can.
    #[test]
    fn a_rejection_quotes_the_server() {
        let reply = parse_reply(401, b"a bearer token is required\n");
        let SyncReply::Failed { detail } = &reply else { panic!("401 is not a sync: {reply:?}") };
        assert!(detail.contains("401"), "{detail}");
        assert!(detail.contains("a bearer token is required"), "{detail}");

        // A status with no body still says something.
        let empty = parse_reply(502, b"");
        let SyncReply::Failed { detail } = &empty else { panic!("502 is not a sync: {empty:?}") };
        assert!(detail.contains("502"), "{detail}");
    }

    /// A gateway that answers 200 with an HTML error page is the case that would abort the
    /// process if the body were sliced by bytes rather than parsed.
    #[test]
    fn a_two_hundred_that_is_not_a_frame_is_a_failure_rather_than_a_document() {
        let reply = parse_reply(200, b"<!doctype html>");
        assert!(matches!(reply, SyncReply::Failed { .. }), "{reply:?}");
        assert!(!reply.changes_the_board());
    }

    /// Bounded, monotonic, and it never shifts past the width of the factor. The clamp is the
    /// assertion: a laptop that spends a weekend off the network comes back with a failure
    /// count no schedule should be allowed to exponentiate.
    #[test]
    fn the_backoff_doubles_and_stops() {
        assert_eq!(backoff_after(0), Duration::ZERO);
        assert_eq!(backoff_after(1), BACKOFF_BASE);
        assert_eq!(backoff_after(2), BACKOFF_BASE * 2);
        assert_eq!(backoff_after(3), BACKOFF_BASE * 4);
        assert_eq!(backoff_after(9), BACKOFF_CAP);
        assert_eq!(backoff_after(u32::MAX), BACKOFF_CAP, "an unbounded count is still bounded");

        let mut previous = Duration::ZERO;
        for failures in 0..64 {
            let wait = backoff_after(failures);
            assert!(wait >= previous, "the schedule went backwards at {failures}");
            assert!(wait <= BACKOFF_CAP, "the schedule passed its cap at {failures}");
            previous = wait;
        }
    }

    /// The reference board is called *"BMW 2020 530i g30"*, and a raw space in a request line
    /// is a malformed HTTP request rather than a 404 — so this is the difference between sync
    /// working on the user's real boards and working only on the ones named like identifiers.
    #[test]
    fn a_board_name_with_spaces_survives_the_url() {
        assert_eq!(
            endpoint_for("http://127.0.0.1:8787", "BMW 2020 530i g30"),
            "http://127.0.0.1:8787/api/v1/boards/BMW%202020%20530i%20g30/sync"
        );
        // A trailing slash on the configured origin must not double.
        assert_eq!(
            endpoint_for("https://boards.example.com/", "board"),
            "https://boards.example.com/api/v1/boards/board/sync"
        );
        // Percent-encoded by byte, so a non-ASCII stem is escaped rather than mangled.
        assert_eq!(escape_segment("é"), "%C3%A9");
        assert_eq!(escape_segment("a-b.c_d~e"), "a-b.c_d~e", "the unreserved set is left alone");
        assert_eq!(escape_segment("../etc"), "..%2Fetc", "a separator cannot survive a segment");
    }

    /// The property that makes this safe on the frame loop: `request` returns before anything
    /// touches a socket. A synchronous implementation would not return here until the connect
    /// timeout elapsed, which is the A/B — this test would take [`CONNECT_TIMEOUT`] rather
    /// than microseconds, and the frame loop would take it too.
    #[test]
    fn requesting_does_not_block_the_caller() {
        let mut sync = Sync::new(DEAD.to_owned(), String::new(), "board".to_owned());
        assert!(sync.is_ready());
        assert!(sync.request(b"version".to_vec(), Vec::new()), "the first ask goes");
        assert!(sync.outstanding(), "and it was handed to the worker, not performed here");
    }

    /// One round trip at a time. A second request built while the first is out carries a
    /// version vector the first is about to supersede, so its answer is stale before it is
    /// asked for — and the next frame builds a current one for nothing.
    #[test]
    fn a_second_request_is_refused_while_one_is_out() {
        let mut sync = Sync::new(DEAD.to_owned(), String::new(), "board".to_owned());
        assert!(sync.request(b"version".to_vec(), Vec::new()));
        assert!(!sync.request(b"version".to_vec(), Vec::new()), "the second does not");
        assert!(!sync.is_ready());
    }

    /// Draining an idle `Sync` is a no-op rather than a block, because it runs every frame.
    #[test]
    fn draining_nothing_returns_nothing_and_does_not_block() {
        let mut sync = Sync::new(DEAD.to_owned(), String::new(), "board".to_owned());
        assert!(sync.drain().is_empty());
        assert!(!sync.outstanding());
        assert!(sync.since().is_none(), "nothing has been agreed yet");
        assert!(sync.last_error().is_none());
    }

    /// A failure must not wedge the caller. Whichever way the worker goes — never started, or
    /// stopped mid-request — `outstanding` has to come back down, or `request` refuses every
    /// attempt for the life of the process and sync presents as having silently stopped.
    #[test]
    fn a_failure_leaves_the_caller_able_to_ask_again() {
        let mut sync = Sync::new(DEAD.to_owned(), String::new(), "board".to_owned());
        sync.note_failure("a made-up failure");
        assert!(!sync.outstanding());
        assert_eq!(sync.last_error(), Some("a made-up failure"));
        assert!(!sync.is_ready(), "but not immediately — the backoff is holding it");
        // The backoff is a deadline, not a latch: clearing it is what a reply does.
        sync.retry_after = None;
        assert!(sync.is_ready());
    }

    /// The marker is the **server's** version, and this is the assertion that catches the
    /// obvious wrong answer: keeping the version we sent would make the next `export_since`
    /// include every update we just imported, and a fresh client would re-upload the whole
    /// board the round after it downloaded it.
    #[test]
    fn a_reply_advances_the_marker_to_the_servers_version() {
        let mut sync = Sync::new(DEAD.to_owned(), String::new(), "board".to_owned());
        assert!(sync.since().is_none());
        // Delivered through the channel the worker writes to, so this exercises `drain`'s own
        // bookkeeping rather than a setter written for the test.
        let (answers, inbound) = channel::<SyncReply>();
        sync.inbound = inbound;
        sync.outstanding = true;
        answers
            .send(SyncReply::Synced {
                version: b"the server's version".to_vec(),
                updates: b"an update".to_vec(),
            })
            .expect("the receiver is right there");
        let drained = sync.drain();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].changes_the_board());
        assert_eq!(sync.since(), Some(&b"the server's version"[..]));
        assert!(!sync.outstanding(), "and the caller may ask again");
        assert!(sync.is_ready());
    }

    /// A `Debug` of a reply prints sizes, never board content. Derived, it would put the
    /// user's document into any log line that formatted one — the shape a review of this
    /// repository has already found once, on a struct that only promised it in prose.
    #[test]
    fn a_reply_does_not_print_the_board() {
        let reply = SyncReply::Synced {
            version: b"vv".to_vec(),
            updates: b"a sticky that says something private".to_vec(),
        };
        let printed = format!("{reply:?}");
        assert!(!printed.contains("private"), "{printed}");
        assert!(printed.contains("36"), "the size is the useful part: {printed}");
    }
}
