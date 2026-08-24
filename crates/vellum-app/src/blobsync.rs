//! A board's pictures, sent to the server while the board is live-synced.
//!
//! # The hole this closes
//!
//! `crate::sync` sends the **document**. A picture pasted on the Mac reaches `velmd` as an
//! item naming a BLAKE3 hash, and the bytes behind that hash never move: the browser draws
//! nothing where the picture is and `GET /api/v1/blobs/{hash}` answers 404 for ever. That is
//! word for word the failure `crates/velmd/src/blobs.rs` was written to end — and it was
//! closed for `crate::push` ("Send my boards"), which is a whole-library batch the user has
//! to ask for, and left open for the sync that runs by itself.
//!
//! So a board already on the server got new pictures on every edit and none of the bytes.
//!
//! # Why this is a second worker and not a branch inside `crate::sync`
//!
//! The two jobs have nothing in common but an address:
//!
//! - **Different sizes.** A document delta is kilobytes; one picture is up to 64 MiB. Putting
//!   a 23 MB upload on the sync worker would stall the document round trip behind it, and the
//!   document is what the other machine is waiting for.
//! - **Different cadence.** The document has to go every few seconds. A blob has to go
//!   **once, ever** — content addressing means the server keeps it and `velmd` cannot delete
//!   it, so a hash this process has seen settled never has to be asked about again.
//! - **`crate::sync` cannot see a board and must not learn to.** Its own header states the
//!   rule: nothing in that file imports `vellum_doc`, which is the mechanical guarantee that a
//!   worker thread there cannot apply a document. Deciding which pictures a board references
//!   means reading its items, so it belongs here.
//!
//! # The cadence
//!
//! [`BlobSync::request`] takes every hash the hot board references and drops the ones already
//! settled. When nothing is left it does not touch the network at all, which is the steady
//! state: a board whose pictures are all up costs one set difference per tick and no request.
//!
//! ⚠ **`settled` only ever grows, and that is sound rather than sloppy.** `velmd` has no
//! route that removes a blob and `crates/velmd/tests/rule_zero.rs` forbids the calls that
//! would, so "the server has this hash" is a fact that cannot become false while the process
//! runs. A fresh `BlobSync` — a new session, a new server, a restart — starts empty and
//! re-probes, which is what makes the cache safe to keep with no expiry.

use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use vellum_store::{BlobStore, Hash};

use crate::sync::{Credential, auth_header};

/// The most of a probe's answer that is read. A wrong address answers with anything at all,
/// and this is the bound on that — `crate::push`'s number and its reason.
const MAX_ANSWER_BYTES: u64 = 4 * 1024 * 1024;

/// How many hashes go in one `missing` probe. `crate::push`'s number and its reasoning: 4,096
/// hashes as a JSON array of 64-character strings is about 274 KB, comfortably inside the
/// route's own ceiling.
const HASHES_PER_PROBE: usize = 4_096;

/// The server's per-request body ceiling, from `crates/velmd/src/blobs.rs`. A picture past it
/// is skipped and counted, never retried and never fatal — the same rule `crate::push` applies.
const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

/// Uploads in one batch, so a board that has just been imported does not hold the worker for
/// an hour before the first picture appears in the browser.
///
/// ⚠ **This is a *bound*, not a cap on the feature.** Whatever is left is unsettled, so the
/// next tick asks for it — the loop converges, it just converges in visible steps. A number
/// here rather than "all of them" is what makes the browser show pictures arriving instead of
/// showing none for ten minutes and then all of them.
const UPLOADS_PER_BATCH: usize = 16;

/// The least time between two reads of the board's item list.
///
/// ⚠ **This is the whole per-frame cost of the feature and it is why the number exists.**
/// Deciding what to ask about means walking every item on the board — 1,300 on the reference
/// board — and `is_ready()` is true on almost every frame once nothing is outstanding, so
/// without a period this would be an O(items) walk sixty times a second for a set that
/// changes when somebody pastes a picture. Three seconds is under the delay at which "I
/// pasted it and it is not on the web yet" becomes a complaint, and is 1 walk per 180 frames.
const SCAN_PERIOD: Duration = Duration::from_secs(3);

/// The waits after consecutive failures. The last is repeated for as long as failures last.
///
/// On the main thread, like `crate::sync`'s: the worker must stay free to answer, and a sleep
/// on the worker is a sleep a quit has to wait out.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(300),
];

// ----- what crosses back --------------------------------------------------------------

/// What one batch came to.
///
/// ⚠ **No `Debug`** — it names hashes, which are board content. The rule
/// `crate::sync::Credential` keeps, applied to the other kind of thing that should not land
/// in a log by accident.
pub struct BlobReport {
    /// Hashes the server is now known to have: already present, or uploaded by this batch.
    pub settled: Vec<Hash>,
    /// How many this batch actually uploaded, for the log line.
    pub sent: u32,
    /// Pictures past the server's ceiling, and pictures this Mac has not got. Counted,
    /// never retried, and deliberately **not** a batch failure — see [`Sent::Absent`].
    pub too_big: u32,
    /// Why the batch stopped early, if it did.
    pub failure: Option<String>,
    /// A 401 or a 403 arrived. Every later request would fail the same way, so the caller
    /// drops this `BlobSync` rather than letting it retry for the life of the process.
    pub session_lost: bool,
}

// ----- the front half -------------------------------------------------------------------

/// One server's picture channel, for as long as a credential lasts.
///
/// ⚠ **Not `Debug` and not `Clone`.** It holds a [`Credential`] indirectly — the worker owns a
/// clone — and it owns channel ends. The immutability is deliberate and is the same contract
/// `crate::sync::Sync` states: the worker never changes its mind about who it is, so signing
/// in or out **drops this and builds a new one** rather than assigning a field.
pub struct BlobSync {
    outbound: Sender<Vec<Hash>>,
    inbound: Receiver<BlobReport>,
    /// Whether a batch is out. One at a time — a second batch built while the first is
    /// uploading would ask about hashes the first is in the middle of settling.
    outstanding: bool,
    /// What the server is known to have. See the header: this never shrinks.
    settled: HashSet<Hash>,
    /// Consecutive failures, for [`BACKOFF`]. Reset by any batch that finishes.
    failures: u32,
    /// The earliest a new batch may go out. `None` means now.
    retry_after: Option<Instant>,
    /// When the board's items were last walked. See [`SCAN_PERIOD`].
    scanned_at: Option<Instant>,
    /// The last failure's sentence, for the log and for a toast.
    last_error: Option<String>,
}

impl BlobSync {
    /// Starts the worker. It lives for the process and idles on an empty channel.
    ///
    /// `server` is the origin, with or without a trailing slash. `blobs` is the shared,
    /// content-addressed asset directory — `crate::editor::blob_directory()`.
    ///
    /// ⚠ **The store is opened on the worker, once, and not here.** `BlobStore::open` touches
    /// the filesystem, and this is called from a frame.
    pub fn new(server: String, credential: Credential, blobs: PathBuf) -> Self {
        let (outbound, requests) = channel::<Vec<Hash>>();
        let (answers, inbound) = channel::<BlobReport>();

        let base = server.trim_end_matches('/').to_owned();
        let spawned = std::thread::Builder::new().name("velm-blobs".to_owned()).spawn(move || {
            // Built once so the connection is pooled across batches. A picture at a time over
            // a fresh TLS handshake is most of the cost of sending small ones at all.
            let http = crate::sync::agent();
            // Opened once. A failure here is reported on the first batch rather than
            // swallowed at startup, so the reason reaches the same place every other reason
            // does instead of a log line nobody is reading.
            let store = BlobStore::open(&blobs);
            // Exits when the `Sender` drops, which is what dropping the `BlobSync` does.
            while let Ok(wanted) = requests.recv() {
                let report = match &store {
                    Ok(store) => run_batch(&http, &base, &credential, store, wanted),
                    Err(error) => BlobReport {
                        settled: Vec::new(),
                        sent: 0,
                        too_big: 0,
                        failure: Some(format!("the picture store would not open ({error})")),
                        session_lost: false,
                    },
                };
                // The receiver is gone: the app is shutting down.
                if answers.send(report).is_err() {
                    return;
                }
            }
        });
        if let Err(error) = spawned {
            log::warn!("blobs: the worker would not start ({error})");
        }

        Self {
            outbound,
            inbound,
            outstanding: false,
            settled: HashSet::new(),
            failures: 0,
            retry_after: None,
            scanned_at: None,
            last_error: None,
        }
    }

    /// Whether [`BlobSync::request`] would accept one right now.
    pub fn is_ready(&self) -> bool {
        !self.outstanding && self.retry_after.is_none_or(|at| Instant::now() >= at)
    }

    /// Whether it is worth walking the board's items to build a request.
    ///
    /// ⚠ **Ask this before reading the board, never after.** It is the guard that keeps an
    /// O(items) walk off most frames — see [`SCAN_PERIOD`]. Both this and [`Self::is_ready`]
    /// have to be true, and they answer different questions: this one is about the cost of
    /// *building* a request, that one about whether the channel will take it.
    pub fn wants_scan(&self) -> bool {
        self.is_ready() && self.scanned_at.is_none_or(|at| at.elapsed() >= SCAN_PERIOD)
    }

    /// The last failure's sentence, if the last batch failed.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Ask about every hash a board references. Never blocks, never touches the network here.
    ///
    /// Returns whether a batch went out. `false` covers the ordinary case as well as the
    /// refusals: **a board whose pictures are all settled sends nothing**, which is the steady
    /// state and costs one set difference.
    pub fn request(&mut self, wanted: impl IntoIterator<Item = Hash>) -> bool {
        if !self.is_ready() {
            return false;
        }
        // ⚠ **Moved whether or not a batch goes out**, and that is not bookkeeping — it is
        // what stops the walk happening on every frame. `crate::sync`'s `asked_at` carries
        // the same warning for the same reason: the common case here is "nothing to send",
        // and without this that case would re-walk 1,300 items sixty times a second.
        self.scanned_at = Some(Instant::now());
        let batch: Vec<Hash> =
            wanted.into_iter().filter(|hash| !self.settled.contains(hash)).collect();
        if batch.is_empty() {
            return false;
        }
        if self.outbound.send(batch).is_err() {
            // The worker is gone — it failed to spawn, or it panicked. `outstanding` is
            // deliberately left false, `crate::sync::Sync::request`'s rescue for its reason:
            // marking a batch as out when nothing is coming back refuses every later attempt
            // for the life of the process.
            self.note_failure("the picture worker is gone".to_owned());
            return false;
        }
        self.outstanding = true;
        true
    }

    /// Everything that has come back since the last call, folded into `settled`. Never blocks.
    ///
    /// ⚠ **Call this every frame.** It is what clears `outstanding`, so a caller that skips it
    /// stops asking altogether — `crate::sync::Sync::drain` records what that costs.
    ///
    /// Answers whether the credential was refused, which is the one outcome the caller has to
    /// act on rather than log.
    pub fn drain(&mut self) -> bool {
        let mut session_lost = false;
        loop {
            match self.inbound.try_recv() {
                Ok(report) => {
                    self.outstanding = false;
                    let settled = report.settled.len();
                    self.settled.extend(report.settled);
                    if report.sent > 0 || report.too_big > 0 {
                        log::info!(
                            "blobs: {} sent, {} too large, {settled} now on the server",
                            report.sent,
                            report.too_big
                        );
                    }
                    session_lost |= report.session_lost;
                    match report.failure {
                        Some(why) => self.note_failure(why),
                        None => {
                            self.failures = 0;
                            self.retry_after = None;
                            self.last_error = None;
                        }
                    }
                }
                Err(TryRecvError::Empty) => return session_lost,
                Err(TryRecvError::Disconnected) => {
                    self.outstanding = false;
                    return session_lost;
                }
            }
        }
    }

    /// Record a failure and arm the backoff.
    fn note_failure(&mut self, why: String) {
        self.failures = self.failures.saturating_add(1);
        let index = (self.failures as usize - 1).min(BACKOFF.len() - 1);
        self.retry_after = Some(Instant::now() + BACKOFF[index]);
        if self.last_error.as_deref() != Some(why.as_str()) {
            log::warn!("blobs: {why}");
        }
        self.last_error = Some(why);
    }
}

// ----- the worker half ------------------------------------------------------------------

/// Probe, then upload what is missing. Runs on the worker thread.
///
/// Never returns an error type: every failure is a sentence on the report, because the
/// caller's answer to all of them is the same — try again later — and content addressing
/// makes a repeated attempt free.
fn run_batch(
    http: &ureq::Agent,
    base: &str,
    credential: &Credential,
    store: &BlobStore,
    wanted: Vec<Hash>,
) -> BlobReport {
    let mut report =
        BlobReport { settled: Vec::new(), sent: 0, too_big: 0, failure: None, session_lost: false };

    let missing = match probe(http, base, credential, &wanted, &mut report) {
        Some(missing) => missing,
        // The probe could not be trusted. Nothing is settled and nothing is sent: assuming
        // the worst here would upload the whole board on a network blip.
        None => return report,
    };
    let absent: HashSet<Hash> = missing.iter().copied().collect();
    // ⚠ **Settled before a single upload.** Everything the server already had is a fact the
    // probe just established, and recording it is most of the value of this feature: a board
    // whose pictures went up with "Send my boards" settles its whole set in one round trip
    // and never asks again.
    report.settled.extend(wanted.iter().filter(|hash| !absent.contains(hash)).copied());

    for hash in missing.iter().take(UPLOADS_PER_BATCH) {
        match upload(http, base, credential, store, hash) {
            Sent::Ok => {
                report.sent += 1;
                report.settled.push(*hash);
            }
            // Settled on purpose: the server answered "already here" before reading the body,
            // which is the probe having raced another client. Asking again would be a request
            // whose answer is known.
            Sent::AlreadyThere => report.settled.push(*hash),
            Sent::TooBig | Sent::Absent => report.too_big += 1,
            // Not settled and not fatal: the next tick asks about it again.
            Sent::Skipped(why) => report.failure = Some(why),
            Sent::SessionLost => {
                report.session_lost = true;
                report.failure = Some("the server refused this session".to_owned());
                return report;
            }
        }
    }
    report
}

/// `POST /api/v1/blobs/missing` — which of these the server has not got.
///
/// `None` means the answer could not be trusted, which is different from "none are missing"
/// and has to stay different: treating an unreachable server as "the server has everything"
/// would settle the whole board against a probe that never happened.
///
/// ⚠ **There is no `HEAD` fallback here and `crate::push` has one.** That is deliberate: this
/// runs against the server the board is *already live-syncing with*, so it is a server built
/// from this workspace, and one `HEAD` per picture — 3,458 of them on this user's store,
/// every one making the server re-hash a file — is a cost worth paying once when a person
/// presses a button and not on a timer. A server without the route is reported and retried.
fn probe(
    http: &ureq::Agent,
    base: &str,
    credential: &Credential,
    wanted: &[Hash],
    report: &mut BlobReport,
) -> Option<Vec<Hash>> {
    let url = format!("{base}/api/v1/blobs/missing");
    let mut out = Vec::new();
    for batch in wanted.chunks(HASHES_PER_PROBE) {
        let hexes: Vec<String> = batch.iter().map(|hash| hash.to_hex()).collect();
        let body = match serde_json::to_vec(&hexes) {
            Ok(body) => body,
            Err(error) => {
                report.failure = Some(format!("encoding the probe failed ({error})"));
                return None;
            }
        };
        let mut request = http.post(&url).header("content-type", "application/json");
        if let Some((name, value)) = auth_header(credential) {
            request = request.header(name, value);
        }
        let response = match request.send(body) {
            Ok(response) => response,
            Err(error) => {
                report.failure = Some(format!("asking which pictures are missing ({error})"));
                return None;
            }
        };
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            report.session_lost = true;
            report.failure = Some("the server refused this session".to_owned());
            return None;
        }
        if !(200..300).contains(&status) {
            report.failure = Some(format!("asking which pictures are missing (HTTP {status})"));
            return None;
        }
        // ⚠ Bounded, and `crate::push`'s reason: a wrong address or a proxy in front of velmd
        // can answer with anything at all, and this machine has run out of memory twice.
        let mut raw = Vec::new();
        let read = response
            .into_body()
            .into_reader()
            .take(MAX_ANSWER_BYTES)
            .read_to_end(&mut raw);
        if let Err(error) = read {
            report.failure = Some(format!("reading the probe's answer ({error})"));
            return None;
        }
        out.extend(crate::push::hashes_in(&String::from_utf8_lossy(&raw)));
    }
    Some(out)
}

/// What one upload came to.
enum Sent {
    Ok,
    /// The server answered before reading the body: it has these bytes.
    AlreadyThere,
    /// Past the server's ceiling. Counted, never retried.
    TooBig,
    /// On the board and not in this store. Counted like [`Sent::TooBig`] and **not** reported
    /// as a failure, which is the difference between a quiet gap and a permanent alarm: the
    /// hash never settles, so it comes back in every batch, and failing on it would climb the
    /// backoff to its 300-second ceiling and warn there for ever about bytes that are not
    /// coming. `crate::push` skips it the same way.
    Absent,
    /// Not sent, and not a reason to stop the batch.
    Skipped(String),
    SessionLost,
}

/// `POST /api/v1/blobs/{hash}`, the raw bytes as the body.
///
/// The bytes come out of the local store with [`BlobStore::get`], which re-hashes on the way
/// out, so a picture that has rotted on this Mac is named here rather than uploaded under a
/// name it no longer answers to. The server checks the hash again against the path, which is
/// what makes a claimed hash a claim and not a fact.
fn upload(
    http: &ureq::Agent,
    base: &str,
    credential: &Credential,
    store: &BlobStore,
    hash: &Hash,
) -> Sent {
    let size = match store.size_of(hash) {
        Ok(Some(size)) => size,
        // On a board and not in this store. Nothing to send and nothing to fix here: the
        // board names a picture whose bytes this Mac has not got, which is what a board
        // written on another machine looks like from this one.
        Ok(None) => return Sent::Absent,
        Err(error) => return Sent::Skipped(format!("measuring {hash} ({error})")),
    };
    if size > MAX_BLOB_BYTES {
        return Sent::TooBig;
    }
    let bytes = match store.get(hash) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Sent::Skipped(format!("{hash} left this store mid-send")),
        Err(error) => return Sent::Skipped(format!("reading {hash} ({error})")),
    };

    let url = format!("{base}/api/v1/blobs/{}", hash.to_hex());
    // ⚠ `application/octet-stream` is required by the route and it is a cross-site request
    // forgery defence rather than a formality — `crates/velmd/src/blobs.rs` argues it at
    // length. A plain HTML form cannot send this type, so a forged upload cannot be built out
    // of markup at all.
    let mut request = http.post(&url).header("content-type", "application/octet-stream");
    if let Some((name, value)) = auth_header(credential) {
        request = request.header(name, value);
    }
    match request.send(bytes) {
        Ok(response) => match response.status().as_u16() {
            200..=299 => Sent::Ok,
            // The store already had it. `BlobStore::put` returns early on `contains`, so this
            // is the server saying the bytes are there — not a refusal.
            409 => Sent::AlreadyThere,
            401 | 403 => Sent::SessionLost,
            413 => Sent::TooBig,
            status => Sent::Skipped(format!("sending {hash} (HTTP {status})")),
        },
        Err(error) => Sent::Skipped(format!("sending {hash} ({error})")),
    }
}
