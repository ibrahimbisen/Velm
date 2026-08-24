//! Storing a picture, and asking which pictures are already here.
//!
//! The desktop app holds ~1.3 GB of assets in a content-addressed store and the server holds
//! its own. Until this module there was no way to move one to the other: `POST /api/v1/import`
//! writes blobs, but only as a side effect of decoding a Miro paste, so a board that was
//! already a board could reach the server with its document and never with its pixels. The
//! browser then drew grey rectangles and `GET /api/v1/blobs/{hash}` answered 404 for ever.
//!
//! Two routes:
//!
//!     POST /api/v1/blobs/{hash}      the raw bytes, no wrapper and no multipart
//!     POST /api/v1/blobs/missing     a JSON array of hex hashes, answering the subset
//!                                    this store does not have
//!
//! # Why the client names the hash and the server checks it
//!
//! The name is not trusted. It is *checked*: the bytes are hashed as they arrive and the
//! upload is refused when the two disagree, so a claimed hash can only ever cause a refusal.
//!
//! The reason to have the client name it at all is that **velmd can never remove a blob.**
//! `tests/rule_zero.rs` forbids the calls, and there is no garbage collector that can see
//! every board. So a body corrupted or truncated in transit, stored under whatever it
//! happened to hash to, is permanent disk that nothing will ever reference and nothing can
//! ever reclaim. Naming the content in advance turns that from a silent orphan into a 400.
//!
//! It also makes the probe below sound: both routes speak about a blob by the same name, so
//! "the server does not have this one" and "here is that one" cannot drift apart.
//!
//! # Why the upload must be `application/octet-stream`
//!
//! ⚠ **This is a cross-site request forgery defence, not a politeness.** The argument is
//! `accounts::sent_as_json`'s, applied to a route where the consequence is worse. A plain
//! HTML form on any page the owner visits can POST to this server with the owner's cookie
//! attached, and a form can only send three content types — `application/octet-stream` is not
//! one of them, so requiring it means a forged upload cannot be built out of markup at all.
//!
//! The tempting counter-argument is that naming the hash makes forgery pointless, since an
//! attacker can only store content whose hash they already know. That is wrong: the attacker
//! writes the body, so they choose the content and therefore choose the hash. Nothing about
//! content addressing stops a page from filling this disk. `SameSite=Strict` on the session
//! cookie (`accounts.rs`) is the first layer; this is the second, and it is the one that also
//! covers the bearer token.
//!
//! The probe is deliberately **not** given the same rule. It writes nothing, its answer is
//! unreadable from a foreign origin, and the client half of this feature is being written
//! against a specification that requires the type on the upload alone. A rule that only one
//! of two halves knows about is an outage, not a defence.
//!
//! # 🛑 RULE ZERO
//!
//! Nothing here can overwrite, truncate or displace a file, and the proof is structural
//! rather than a promise:
//!
//! 1. A blob's filename is a function of its content — `BlobStore::path_for` names it from
//!    the hash the store itself computed. Two different contents cannot want the same name
//!    unless BLAKE3-256 collides.
//! 2. So the only file an upload can land on is one whose bytes are already identical, and
//!    every path that could touch it declines: `put` returns early on `contains`, `put_reader`
//!    drops its staged file on `contains`, and `publish` swallows a lost race when the
//!    destination exists — same bytes by construction.
//! 3. The write itself is a staged file plus a rename inside `vellum-store`, with the file
//!    fsynced before it becomes visible. A crash can leave a stray file in `.staging`; it can
//!    never leave a partial blob under a valid name.
//! 4. This module opens no file of its own. Every byte reaches the disk through `BlobStore`,
//!    so none of the truncating calls `tests/rule_zero.rs` names appears here at all.
//!
//! # What an authorised but careless client can do to the disk
//!
//! ⚠ **There is no quota, and this states that rather than implying otherwise.** A caller who
//! has passed the bearer or session gate can store distinct content until the filesystem is
//! full, one request at a time, 64 MiB at a time. Four things bound it and a byte ceiling is
//! not among them:
//!
//! - **Who can reach the route at all.** Everything under `/api/v1/` is behind the gate, and
//!   on the deployment this was written for the account list is invite-only.
//! - **64 MiB per request**, refused from the `Content-Length` before a byte of body is read.
//! - **Sixteen connections for the whole server**, and the client half of this feature sends
//!   one request at a time so that the owner's own browser keeps its slots.
//! - **Duplicate content is free.** Content addressing means re-sending the same gigabyte a
//!   hundred times stores it once, so an abusive client has to bring genuinely new bytes to
//!   cost anything.
//!
//! The exposure this widens already existed: `POST /api/v1/import` stores blobs today, 8 MiB
//! at a time, for any authenticated caller. This makes the pipe wider, not new. A real quota
//! needs either a directory walk on every upload — 3,458 files in the store this was measured
//! against — or a cached total that has to survive a restart and stay honest against
//! `velmd import` and files copied in by hand. Both are more machinery than a single-operator
//! server justifies, so the honest answer is this paragraph and a line in the public docs.
//!
//! One disclosure to state rather than leave to be discovered: the probe is a **dedup
//! oracle**. Any account can ask whether an exact file is already on this server. That is the
//! write-side twin of `GET /api/v1/blobs/{hash}`, which has no per-board check either because
//! a deduplicated blob has no single owning board.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use vellum_store::{BlobStore, Hash, StoreError};

use crate::serve::{Server, json_string, respond};
use crate::sync::Refusal;

/// Everything under here is a blob upload; the segment after it is the hash.
const PATH_PREFIX: &str = "/api/v1/blobs/";

/// The probe. ⚠ It sits **under** [`PATH_PREFIX`], so the dispatch in `serve.rs` has to test
/// this first. If that order is ever inverted the probe answers *"that is not a blob hash"*,
/// because `missing` is seven characters and can never parse as 64 hex — which is a loud
/// failure that stores nothing, not a dangerous one. A test asserts both halves.
const PATH_MISSING: &str = "/api/v1/blobs/missing";

/// The largest blob this route accepts, refused before a byte of body is read.
///
/// **64 MiB, and the number is measured rather than chosen.** The store this was written for
/// holds 3,458 blobs; three of them are over 8 MiB and the largest is 22,957,039 B. So the
/// cap has to clear 22 MiB, and `crate::sync::MAX_BODY` — 8 MiB — does not.
///
/// ⚠ **This is why the route cannot go through `sync::read_body`.** That function re-checks
/// `MAX_BODY` internally, deliberately, so *any* caller of it is capped at 8 MiB whatever its
/// own `Content-Length` check allowed. A blob route with a 64 MiB cap that called it would
/// refuse this user's three largest pictures as *"that request body did not arrive"* — a
/// truthful-sounding 400 about entirely the wrong thing.
///
/// Streaming is what makes the larger number cheaper rather than more expensive. `MAX_BODY`
/// is 8 MiB because sixteen buffered bodies are 128 MiB of resident memory; this route holds
/// [`DRAIN_CHUNK`] per connection regardless of the blob's size, so sixteen concurrent
/// uploads at the cap are 4 MiB.
pub const MAX_BLOB: usize = 64 * 1024 * 1024;

/// The largest probe body, in bytes.
///
/// A hash is 64 hex characters and a JSON array entry costs 67 bytes with its quotes and
/// comma, so this is about 7,800 hashes — comfortably past the 3,458 the measured store
/// holds. Small enough that the probe goes through `sync::read_body` unchanged, which is the
/// point: only the upload needs to read its own body.
const MAX_MANIFEST: usize = 512 * 1024;

/// The most hashes one probe may ask about.
///
/// Stated as well as implied. [`MAX_MANIFEST`] already bounds the count, but a client that
/// batches needs a number it can divide by, and a cap that has to be derived from a byte
/// budget is a cap nobody derives.
const MAX_HASHES: usize = 8_000;

/// The wall clock a slow upload gets before it is given up on.
///
/// ⚠ A **deadline**, not the socket's per-read timeout, for the reason `serve.rs`'s
/// `HEAD_DEADLINE` and `sync.rs`'s `BODY_DEADLINE` both record: `SO_RCVTIMEO` is reset by
/// every byte that arrives, so a client dripping one byte per timeout holds a connection for
/// as long as it likes, and sixteen of them take every slot this server has.
///
/// **It grows with the declared length and stops at a ceiling**, rather than being flat, and
/// that is the whole of the difference from `BODY_DEADLINE`. A flat 180 s would hand a
/// one-byte body the same three minutes a 64 MiB one needs. So: [`BLOB_GRACE`] to get
/// started, plus one second per [`BLOB_FLOOR`] bytes declared, capped at [`BLOB_CEILING`].
///
/// The arithmetic, which belongs here so that changing [`MAX_BLOB`] forces a look at it: at
/// the ceiling, 64 MiB in 180 s is a 3 Mbit/s sustained floor. The largest real blob,
/// 22,957,039 B, would earn 30 + 175 s and so is held at the ceiling too, which asks 1.0
/// Mbit/s of it. An 8 MiB body gets 94 s. Sixteen slots held for 180 s is six times the 30 s
/// window every other body has, reachable only by a caller who has already passed the gate —
/// so on this deployment it is bounded by who holds an account.
const BLOB_GRACE: Duration = Duration::from_secs(30);
const BLOB_CEILING: Duration = Duration::from_secs(180);
const BLOB_FLOOR: u64 = 128 * 1024;

/// How much is asked of the socket at a time. `vellum-store` stages in chunks of the same
/// size, so this is the peak this route adds per connection.
const DRAIN_CHUNK: usize = 256 * 1024;

/// The two refusals a body can earn once it has started arriving.
///
/// Kept as constants because they travel inside an [`io::Error`] and come back out at the
/// response, and a sentence that is written twice drifts.
const MISNAMED: &str = "those bytes are not the blob you named\n";
const INCOMPLETE: &str = "that blob did not arrive\n";

/// The hash named in an upload path, or `None` if this is not one.
///
/// The routing rule lives here rather than in `serve.rs`, following `sync::board_id` and
/// `manage::is_create`: it can then be tested without a socket, and the one fact `serve.rs`
/// has to get right — that this path is under `/api/v1/` and therefore behind `needs_token`
/// — is asserted in the same file that spells it.
///
/// No validation here beyond "not empty". Parsing the segment as a [`Hash`] is the traversal
/// defence and it is total, exactly as on the GET route: 64 hex characters decode or they do
/// not, and no spelling of `..` or `/` survives that.
pub(crate) fn upload_target(path: &str) -> Option<&str> {
    let hash = path.strip_prefix(PATH_PREFIX)?;
    (!hash.is_empty()).then_some(hash)
}

/// Whether this path is the probe. Exact equality, so nothing near it is routed here.
pub(crate) fn is_missing(path: &str) -> bool {
    path == PATH_MISSING
}

/// Whether the caller said the body is raw bytes.
///
/// The media type only: a parameter is legal, and refusing `application/octet-stream;
/// something` would refuse a client that got the part that matters right. Case-insensitive,
/// because a header value is not case-sensitive and a client that shouts is not an attacker.
fn sent_as_bytes(headers: &BTreeMap<String, String>) -> bool {
    headers.get("content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/octet-stream"))
    })
}

/// How many bytes of body an upload promises, or why it will not be read.
///
/// ⚠ **The content-type check is here, ahead of the length**, and that placement is the
/// point: both are refusals decided from the request head alone, before a byte of body is
/// read, so putting them in one function gives the route one pre-body gate that can be
/// tested without a socket. 415 rather than 400, following `accounts::sign_in`: the request
/// was well formed and the *type* is what was refused, so the client has been told exactly
/// what to change.
pub(crate) fn upload_length(headers: &BTreeMap<String, String>) -> Result<usize, Refusal> {
    if !sent_as_bytes(headers) {
        return Err(Refusal { status: 415, message: "send application/octet-stream\n" });
    }
    length_within(
        headers,
        MAX_BLOB,
        "a blob upload needs a Content-Length\n",
        "that blob is too large\n",
    )
}

/// How many bytes of body a probe promises, or why it will not be read.
pub(crate) fn manifest_length(headers: &BTreeMap<String, String>) -> Result<usize, Refusal> {
    length_within(
        headers,
        MAX_MANIFEST,
        "a blob list needs a Content-Length\n",
        "that blob list is too large\n",
    )
}

/// One length parser for both routes, with the route's own two sentences.
///
/// ⚠ **The sentences are parameterised, not just the 413 one.** An upload that answered *"a
/// sync request needs a Content-Length"* is exactly the wrong-file trap `manage.rs` gave
/// itself a separate cap to avoid: an operator reads the sentence and goes looking in the
/// file that did not refuse them. The two neutral refusals below say nothing about which
/// route asked, so they are shared.
///
/// `Transfer-Encoding` is refused outright rather than implemented, which is `sync.rs`'s
/// decision and its reasons: chunked framing would be a second body parser reachable from
/// the network, and every client this server has sets a length.
fn length_within(
    headers: &BTreeMap<String, String>,
    cap: usize,
    needs: &'static str,
    too_large: &'static str,
) -> Result<usize, Refusal> {
    if headers.contains_key("transfer-encoding") {
        return Err(Refusal {
            status: 400,
            message: "send a Content-Length; this server does not read chunked bodies\n",
        });
    }
    let Some(raw) = headers.get("content-length") else {
        return Err(Refusal { status: 400, message: needs });
    };
    let Ok(length) = raw.trim().parse::<u64>() else {
        return Err(Refusal { status: 400, message: "that Content-Length is not a number\n" });
    };
    if length > cap as u64 {
        return Err(Refusal { status: 413, message: too_large });
    }
    // Infallible once the cap has passed on every target this builds for, and written as a
    // conversion anyway: `usize` is not guaranteed to be 64 bits, and a silent truncation
    // here would read a short body and call it whole.
    let Ok(length) = usize::try_from(length) else {
        return Err(Refusal { status: 413, message: too_large });
    };
    Ok(length)
}

/// How long this body has, from its declared length. See [`BLOB_GRACE`].
fn deadline_for(length: usize) -> Duration {
    let declared = u64::try_from(length).unwrap_or(u64::MAX);
    let allowance = BLOB_GRACE + Duration::from_secs(declared / BLOB_FLOOR);
    allowance.min(BLOB_CEILING)
}

/// The request body, as a `Read` that hashes what it yields.
///
/// ⚠ **The hash check lives in the reader rather than after the store call, and that is what
/// makes a mismatch store nothing.** `BlobStore::put_reader` learns a blob's name only when
/// its last byte has been read, so by the time it could compare, it has already staged the
/// file and is one `persist` away from publishing it. Refusing from inside the reader instead
/// makes the store's own `drain` fail, which drops the staged temporary file — inside
/// `vellum-store`, where a temporary file this program created is allowed to be removed —
/// and returns before anything is published under any name.
///
/// The published name still comes from the store's own pass over the bytes. This reader's
/// hash decides only whether the store gets to finish, so a bug here can refuse a good blob
/// and can never file one under a name that is not its content.
struct Body<'a> {
    stream: &'a TcpStream,
    /// What the head parser already read past the end of the head. Not optional and getting
    /// it wrong is the whole bug: a `BufReader` fills from the socket however small the read
    /// asked for, so the first kilobytes of the body are usually sitting in it already.
    already: &'a [u8],
    taken: usize,
    length: usize,
    began: Instant,
    deadline: Duration,
    hasher: blake3::Hasher,
    expected: Hash,
}

impl<'a> Body<'a> {
    fn new(stream: &'a TcpStream, already: &'a [u8], length: usize, expected: Hash) -> Self {
        Self {
            stream,
            already,
            taken: 0,
            length,
            began: Instant::now(),
            deadline: deadline_for(length),
            hasher: blake3::Hasher::new(),
            expected,
        }
    }

    /// The end of the body, which is where the name is checked.
    fn ended(&self) -> io::Result<usize> {
        let actual = Hash::from_bytes(*self.hasher.finalize().as_bytes());
        if actual != self.expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData, MISNAMED));
        }
        Ok(0)
    }
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // The declared length is the only end-of-body signal there is. The socket stays open
        // until the response is written, so waiting for an end of stream would wait for ever.
        if self.taken >= self.length {
            return self.ended();
        }
        if self.began.elapsed() > self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, INCOMPLETE));
        }
        let want = (self.length - self.taken).min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        // ⚠ `already` can be longer than the body when a client pipelines a second request
        // behind this one, so it is clamped to the declared length. The extra is dropped,
        // which is correct: every response this server sends carries `Connection: close`, so
        // there is no second request on this socket to serve.
        let head = self.already.len().min(self.length);
        let read = if self.taken < head {
            let n = (head - self.taken).min(want);
            buf[..n].copy_from_slice(&self.already[self.taken..self.taken + n]);
            n
        } else {
            let mut source = self.stream;
            match source.read(&mut buf[..want]) {
                // ⚠ **An end of stream short of the declared length is an error, never
                // `Ok(0)`.** Returning `Ok(0)` would let the store hash a truncated body and
                // report it as a hash mismatch — a 400 with the wrong sentence, telling a
                // client its bytes were wrong when its connection was.
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, INCOMPLETE)),
                Ok(n) => n,
                Err(error) => return Err(error),
            }
        };
        self.hasher.update(&buf[..read]);
        self.taken += read;
        Ok(read)
    }
}

/// Read the body to its end and verify it, storing nothing.
///
/// For the case where the store already holds the named blob. It costs one hash pass instead
/// of a staged write and two fsyncs, and the answer stays honest: 200 means *"the bytes you
/// sent are the blob you named, and it is here"*, not merely *"something with that name is
/// here"*.
///
/// Hand-rolled rather than `std::io::sink`, because the obvious way to feed one is a call
/// `tests/rule_zero.rs` forbids by name.
fn already_here(mut body: Body<'_>) -> io::Result<()> {
    let mut scratch = vec![0u8; DRAIN_CHUNK];
    loop {
        match body.read(&mut scratch) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Which refusal a store error is, or `None` for one the client cannot fix.
///
/// The kinds are the channel because `Read` has no other one. `InvalidData` is set by
/// [`Body::ended`] alone on this path: everything else the store does with the reader is a
/// write to a staged temporary file, which fails with a filesystem error rather than that.
/// A wrong guess in either direction is a wrong sentence, never a wrong store.
fn refusal_for(error: &StoreError) -> Option<&'static str> {
    // Named `cause` rather than `io`, which would read as the module this file imports.
    let StoreError::Io(cause) = error else { return None };
    match cause.kind() {
        io::ErrorKind::InvalidData => Some(MISNAMED),
        // `WouldBlock` is what `SO_RCVTIMEO` raises on this platform, so it belongs with the
        // deadline rather than with the errors an operator should see in a log.
        io::ErrorKind::UnexpectedEof | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
            Some(INCOMPLETE)
        }
        _ => None,
    }
}

/// `POST /api/v1/blobs/{hash}` — store the body under the name it was given, or refuse it.
///
/// Idempotent: storing content that is already here answers 200 and writes nothing, which is
/// what makes a second run of a whole-library push cheap even when the client skipped the
/// probe.
///
/// The caller has already passed the bearer or session gate; see the module header for what
/// that authorises and what bounds it. There is no per-blob owner and none is invented here:
/// a blob is deduplicated across boards by design, so *"who owns this one"* has no single
/// answer, and `accounts::may_see` takes a board id that this route does not have.
pub(crate) fn upload(
    server: &Server,
    hash: &str,
    already: &[u8],
    length: usize,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    // The same sentence and the same parse the GET route uses. Nothing has been read from the
    // socket at this point, so a bad name costs one refusal and no body.
    let Ok(expected) = hash.parse::<Hash>() else {
        return respond(stream, 400, "text/plain", b"that is not a blob hash\n", origin);
    };
    let store = BlobStore::open(&server.config.blobs)?;
    let body = Body::new(stream, already, length, expected);
    let stored = if store.contains(&expected) {
        already_here(body).map_err(StoreError::from)
    } else {
        store.put_reader(body).map(|_| ())
    };
    match stored {
        Ok(()) => {
            let answer = format!("{{\"hash\":{}}}", json_string(&expected.to_hex()));
            respond(stream, 200, "application/json", answer.as_bytes(), origin)
        }
        Err(error) => match refusal_for(&error) {
            Some(sentence) => respond(stream, 400, "text/plain", sentence.as_bytes(), origin),
            // Not a refusal: a full disk, a permissions fault, something an operator has to
            // see. `serve.rs` owns the one 500 path and the one log line.
            None => Err(error.into()),
        },
    }
}

/// `POST /api/v1/blobs/missing` — which of these hashes this store does not have.
///
/// The point is a second run that does not re-send a gigabyte. One request answers for the
/// whole store; the alternative was a `HEAD` per blob, which is worse than the upload it is
/// trying to avoid: the GET blob route reads and re-hashes the file to answer at all, so
/// probing 3,458 blobs that way would read 1.3 GB from disk over 3,458 connections, none of
/// which can be reused because every response carries `Connection: close`.
///
/// The answer is a JSON array of the hashes that are **not** here, in the order asked and
/// deduplicated, so it can never be longer than the question.
pub(crate) fn missing(server: &Server, body: &[u8], stream: &TcpStream) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let Ok(asked) = serde_json::from_slice::<Vec<String>>(body) else {
        return respond(stream, 400, "text/plain", b"send a JSON array of blob hashes\n", origin);
    };
    if asked.len() > MAX_HASHES {
        return respond(stream, 413, "text/plain", b"that blob list is too long\n", origin);
    }
    let store = BlobStore::open(&server.config.blobs)?;
    let mut seen = BTreeSet::new();
    let mut absent = Vec::new();
    for (index, raw) in asked.iter().enumerate() {
        let Ok(hash) = raw.parse::<Hash>() else {
            // ⚠ **The index, never the text.** The entry is attacker-controlled bytes and
            // this is a response body; echoing it turns the refusal into a reflection.
            let sentence = format!("entry {index} of that blob list is not a blob hash\n");
            return respond(stream, 400, "text/plain", sentence.as_bytes(), origin);
        };
        if !seen.insert(hash) {
            continue;
        }
        if !store.contains(&hash) {
            absent.push(json_string(&hash.to_hex()));
        }
    }
    let answer = format!("[{}]", absent.join(","));
    respond(stream, 200, "application/json", answer.as_bytes(), origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// A scratch directory that is unique per call and is never cleared.
    ///
    /// Named from an atomic counter rather than the clock, following `paste.rs` — a fixture
    /// whose directory came from the clock in *seconds* was shared by two tests that started
    /// in the same second. Nothing is removed here either, which is what lets
    /// `tests/rule_zero.rs` scan this crate for a removal with no exemption for test code.
    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("velmd-blobs-{name}-{n}-{pid}"));
        std::fs::create_dir_all(dir.join("boards")).unwrap();
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        dir
    }

    fn server_for(dir: &Path) -> Server {
        Server {
            accounts: Mutex::new(crate::accounts::Accounts::open(dir)),
            config: crate::serve::Config {
                data: dir.join("boards"),
                blobs: dir.join("blobs"),
                web: None,
                addr: "127.0.0.1:0".parse().unwrap(),
                token: None,
                app_origin: None,
                behind_https: false,
            },
            boards: Mutex::new(()),
        }
    }

    /// A real loopback pair, because these routes answer by writing to a socket.
    fn a_socket_pair(label: &str) -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{label}: {e}"));
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{label}: {e}"));
        let (server, _) = listener.accept().unwrap_or_else(|e| panic!("{label}: {e}"));
        (server, client)
    }

    /// Everything the client half received, as text.
    fn answer_on(mut client: TcpStream) -> String {
        client.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(read) = client.read(&mut chunk) {
            if read == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Request headers, spelt the way `read_head` leaves them: lowercase names.
    fn headers_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    /// The headers a correct upload sends, with a length the caller chooses.
    fn upload_headers(length: usize) -> BTreeMap<String, String> {
        headers_of(&[
            ("content-type", "application/octet-stream"),
            ("content-length", &length.to_string()),
        ])
    }

    /// How many files a directory holds, at the top level.
    fn files_in(dir: &Path) -> usize {
        std::fs::read_dir(dir).map(|listing| listing.flatten().count()).unwrap_or(0)
    }

    /// The cap has to clear the largest blob in the store this was written for — 22,957,039 B,
    /// measured — and a `const` block fails the **build** if it is ever lowered past that,
    /// rather than failing a suite somebody may not have run.
    const _: () = assert!(
        MAX_BLOB >= 24 * 1024 * 1024,
        "the blob cap no longer clears the largest measured asset, 22,957,039 bytes"
    );

    /// The probe, unlike the upload, hands its body to `sync::read_body` — which re-checks its
    /// own cap internally whatever the caller allowed. If [`MAX_MANIFEST`] ever passed it, the
    /// probe would start answering *"that request body did not arrive"* for a list that was
    /// perfectly well formed. A `const` block for the same reason as the one above.
    const _: () = assert!(
        MAX_MANIFEST < crate::sync::MAX_BODY,
        "the probe's cap no longer fits inside the shared body reader's own"
    );

    /// ⚠ The one thing `serve.rs` has to get right about these routes, asserted where the
    /// paths are defined. A route that *writes to the disk for ever* reachable without the
    /// gate would let anybody who can reach the socket fill it.
    #[test]
    fn the_blob_routes_are_inside_the_token_gate() {
        assert!(PATH_PREFIX.starts_with("/api/v1/"), "the upload path left the token gate");
        assert!(PATH_MISSING.starts_with("/api/v1/"), "the probe path left the token gate");
    }

    /// The overlap between the two paths, and the proof that getting the order wrong is loud.
    #[test]
    fn the_probe_path_is_checked_before_the_upload_path() {
        assert_eq!(upload_target(PATH_MISSING), Some("missing"), "the two paths no longer overlap");
        assert!(
            "missing".parse::<Hash>().is_err(),
            "if this ever parsed, an inverted dispatch would store something instead of refusing"
        );
        assert!(is_missing(PATH_MISSING));
        for other in ["/api/v1/blobs/missing/", "/api/v1/blobs", "/api/v1/blobsmissing"] {
            assert!(!is_missing(other), "{other} was routed to the probe");
        }
    }

    /// An upload path names something. The hash itself is checked by parsing, not here.
    #[test]
    fn an_upload_path_must_name_something() {
        assert_eq!(upload_target("/api/v1/blobs/abc"), Some("abc"));
        assert_eq!(upload_target("/api/v1/blobs/"), None);
        assert_eq!(upload_target("/api/v1/blobs"), None);
        assert_eq!(upload_target("/api/v1/board/x"), None);
    }

    /// 🛑 The forgery defence, driven: the three types an HTML form can send are refused, and
    /// each of them is a real cross-site POST that a page could build.
    #[test]
    fn an_upload_must_say_its_body_is_raw_bytes() {
        for forgeable in [
            "text/plain",
            "text/plain;charset=UTF-8",
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "application/json",
        ] {
            let headers = headers_of(&[("content-type", forgeable), ("content-length", "3")]);
            let refusal = upload_length(&headers).expect_err("a form type was accepted");
            assert_eq!(refusal.status, 415, "{forgeable} was not refused as a type");
        }
        // Absent entirely, which is what `fetch` sends for a body it was given no type for.
        let headers = headers_of(&[("content-length", "3")]);
        assert_eq!(upload_length(&headers).expect_err("no type was accepted").status, 415);
        // The right type, with and without a parameter, and shouted.
        for good in [
            "application/octet-stream",
            "application/octet-stream; v=1",
            "APPLICATION/OCTET-STREAM",
        ] {
            let headers = headers_of(&[("content-type", good), ("content-length", "3")]);
            assert_eq!(upload_length(&headers).unwrap(), 3, "{good} was refused");
        }
    }

    /// The cap, and the sentence — which must name *this* route, or an operator reading a 413
    /// after a failed upload goes looking in `sync.rs`.
    #[test]
    fn an_upload_length_is_capped_with_its_own_sentence() {
        assert_eq!(upload_length(&upload_headers(MAX_BLOB)).unwrap(), MAX_BLOB);
        let over = upload_length(&upload_headers(MAX_BLOB + 1)).expect_err("over the cap passed");
        assert_eq!(over.status, 413);
        let said = over.message;
        assert!(said.contains("blob"), "the refusal does not name the route: {said}");
        assert!(!said.contains("sync"), "the refusal names the wrong route: {said}");

        let none = headers_of(&[("content-type", "application/octet-stream")]);
        assert_eq!(upload_length(&none).expect_err("no length passed").status, 400);

        let chunked = headers_of(&[
            ("content-type", "application/octet-stream"),
            ("transfer-encoding", "chunked"),
            ("content-length", "3"),
        ]);
        assert_eq!(upload_length(&chunked).expect_err("chunked passed").status, 400);

        let words = headers_of(&[
            ("content-type", "application/octet-stream"),
            ("content-length", "some"),
        ]);
        assert_eq!(upload_length(&words).expect_err("a word passed as a length").status, 400);
    }

    /// The probe's cap is its own, and it does **not** demand a content type — see the module
    /// header for why the two routes differ on that.
    #[test]
    fn a_probe_length_is_capped_and_needs_no_content_type() {
        let plain = headers_of(&[("content-length", "10")]);
        assert_eq!(manifest_length(&plain).unwrap(), 10);
        let over = headers_of(&[("content-length", &(MAX_MANIFEST + 1).to_string())]);
        let refused = manifest_length(&over).expect_err("over the cap passed");
        assert_eq!(refused.status, 413);
        let said = refused.message;
        assert!(said.contains("list"), "the refusal does not name the route: {said}");
        assert!(!said.contains("blob upload"), "the refusal names the wrong route: {said}");
    }

    /// The deadline grows with the body and stops at the ceiling, so a one-byte request cannot
    /// hold a connection slot for the three minutes a 64 MiB one needs.
    #[test]
    fn the_deadline_grows_with_the_body_and_then_stops() {
        assert_eq!(deadline_for(0), BLOB_GRACE);
        assert_eq!(deadline_for(MAX_BLOB), BLOB_CEILING);
        // 8 MiB is the middle of the range: past the grace, short of the ceiling.
        let middling = deadline_for(8 * 1024 * 1024);
        assert!(middling > BLOB_GRACE, "an 8 MiB body got only {middling:?}");
        assert!(middling < BLOB_CEILING, "an 8 MiB body got the whole ceiling, {middling:?}");
        // The largest blob measured in the real store reaches the ceiling, which asks it for
        // about 1 Mbit/s. If this ever stops being true the sentence in `BLOB_GRACE` is wrong.
        assert_eq!(deadline_for(22_957_039), BLOB_CEILING);
    }

    /// The common case, and the one that costs nothing: the head parser's buffer already held
    /// the whole body, so the reader never touches the socket.
    #[test]
    fn a_body_already_in_the_head_buffer_needs_no_socket_read() {
        let (socket, client) = a_socket_pair("buffered");
        let bytes = b"a small picture".to_vec();
        let body = Body::new(&socket, &bytes, bytes.len(), Hash::of(&bytes));
        assert!(already_here(body).is_ok(), "a whole body in the buffer was not accepted");
        drop(client);
    }

    /// Pipelined bytes past the declared length are dropped rather than hashed.
    #[test]
    fn bytes_past_the_declared_length_are_dropped() {
        let (socket, client) = a_socket_pair("pipelined");
        let bytes = b"the body and then some more".to_vec();
        let body = Body::new(&socket, &bytes, 8, Hash::of(&bytes[..8]));
        assert!(already_here(body).is_ok(), "the extra bytes were hashed into the body");
        drop(client);
    }

    /// A body arriving in pieces reassembles, which is the only case a socket read is for.
    #[test]
    fn a_body_split_across_writes_reassembles() {
        let (socket, mut client) = a_socket_pair("split");
        let whole = b"one two three four five six seven".to_vec();
        let expected = Hash::of(&whole);
        let rest = whole.clone();
        let sender = std::thread::spawn(move || {
            for piece in rest[4..].chunks(7) {
                client.write_all(piece).unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            client
        });
        let body = Body::new(&socket, &whole[..4], whole.len(), expected);
        let read = already_here(body);
        let client = sender.join().unwrap();
        assert!(read.is_ok(), "a split body was refused: {read:?}");
        drop(client);
    }

    /// ⚠ A half-close short of the declared length is an end-of-stream error, **not** an end
    /// of body — or a truncated upload would be reported as bytes that hash wrong.
    #[test]
    fn a_short_body_is_an_end_of_stream_not_an_end_of_body() {
        let (socket, mut client) = a_socket_pair("short");
        client.write_all(b"half").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let body = Body::new(&socket, &[], 32, Hash::of(b"half"));
        let error = already_here(body).expect_err("a truncated body was accepted");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(refusal_for(&StoreError::from(error)), Some(INCOMPLETE));
        drop(client);
    }

    /// The whole route: bytes in, a blob on disk, and the hash back.
    #[test]
    fn a_good_upload_is_stored_and_answered() {
        let dir = scratch("good");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("good");
        let bytes = b"the pixels of a small picture".to_vec();
        let hash = Hash::of(&bytes);

        upload(&server, &hash.to_hex(), &bytes, bytes.len(), &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.starts_with("HTTP/1.1 200 "), "{answer}");
        assert!(answer.contains(&hash.to_hex()), "the answer does not name the blob: {answer}");
        let store = BlobStore::open(dir.join("blobs")).unwrap();
        assert!(store.contains(&hash), "the blob was answered for and not stored");
    }

    /// Idempotent, and the second time writes nothing: the file is byte for byte the one the
    /// first upload left, and it was not restaged.
    #[test]
    fn the_same_upload_twice_stores_it_once() {
        let dir = scratch("twice");
        let server = server_for(&dir);
        let bytes = b"a picture on two boards".to_vec();
        let hash = Hash::of(&bytes);

        let (first, client) = a_socket_pair("twice-a");
        upload(&server, &hash.to_hex(), &bytes, bytes.len(), &first).unwrap();
        drop(first);
        assert!(answer_on(client).starts_with("HTTP/1.1 200 "));

        let store = BlobStore::open(dir.join("blobs")).unwrap();
        let before = std::fs::metadata(store.path_for(&hash)).unwrap().modified().unwrap();

        let (second, client) = a_socket_pair("twice-b");
        upload(&server, &hash.to_hex(), &bytes, bytes.len(), &second).unwrap();
        drop(second);
        assert!(answer_on(client).starts_with("HTTP/1.1 200 "), "the second upload was not 200");

        let after = std::fs::metadata(store.path_for(&hash)).unwrap().modified().unwrap();
        assert_eq!(before, after, "the second upload rewrote a blob that was already here");
        let staging = dir.join("blobs").join(".staging");
        assert_eq!(files_in(&staging), 0, "a staged file was left behind");
    }

    /// 🛑 The orphan proof, and the most valuable test here. Bytes that are not what they were
    /// named are refused, and **nothing is stored under either name** — not the claimed one,
    /// not the true one, and nothing is left staged. This program can never remove a blob, so
    /// a stored orphan would be permanent.
    #[test]
    fn misnamed_bytes_are_refused_and_nothing_is_stored() {
        let dir = scratch("misnamed");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("misnamed");
        let sent = b"these are not the bytes you asked for".to_vec();
        let claimed = Hash::of(b"something else entirely");
        let truth = Hash::of(&sent);

        upload(&server, &claimed.to_hex(), &sent, sent.len(), &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.starts_with("HTTP/1.1 400 "), "misnamed bytes were accepted: {answer}");
        assert!(answer.contains("not the blob you named"), "{answer}");
        let store = BlobStore::open(dir.join("blobs")).unwrap();
        assert!(!store.contains(&claimed), "a blob was filed under the name that was claimed");
        assert!(!store.contains(&truth), "a blob was filed under the name it really hashes to");
        let staging = dir.join("blobs").join(".staging");
        assert_eq!(files_in(&staging), 0, "the refused bytes were left staged");
    }

    /// A name that is not 64 hex characters is refused before the body is looked at, with the
    /// same sentence the read route gives.
    #[test]
    fn a_path_that_is_not_a_hash_is_refused() {
        let dir = scratch("badname");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("badname");

        upload(&server, "../../etc/passwd", b"anything", 8, &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
        assert!(answer.contains("not a blob hash"), "{answer}");
        // The store was never opened, so not even its staging directory exists: the refusal
        // happens before anything touches the disk.
        assert_eq!(files_in(&dir.join("blobs")), 0, "a bad name reached the disk");
    }

    /// The probe answers the subset the store lacks, in the order asked.
    #[test]
    fn the_probe_answers_only_what_is_not_here() {
        let dir = scratch("probe");
        let server = server_for(&dir);
        let store = BlobStore::open(dir.join("blobs")).unwrap();
        let here = store.put(b"already on the server").unwrap();
        let one = Hash::of(b"the first missing one");
        let two = Hash::of(b"the second missing one");

        let asked = format!("[\"{}\",\"{}\",\"{}\"]", one.to_hex(), here.to_hex(), two.to_hex());
        let (socket, client) = a_socket_pair("probe");
        missing(&server, asked.as_bytes(), &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.starts_with("HTTP/1.1 200 "), "{answer}");
        let expected = format!("[\"{}\",\"{}\"]", one.to_hex(), two.to_hex());
        assert!(answer.ends_with(&expected), "the probe answered {answer}");
    }

    /// A repeated hash is answered once, so the answer can never be longer than the question.
    #[test]
    fn the_probe_answers_a_repeated_hash_once() {
        let dir = scratch("repeat");
        let server = server_for(&dir);
        let one = Hash::of(b"asked for twice").to_hex();

        let asked = format!("[\"{one}\",\"{one}\"]");
        let (socket, client) = a_socket_pair("repeat");
        missing(&server, asked.as_bytes(), &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.ends_with(&format!("[\"{one}\"]")), "{answer}");
    }

    /// ⚠ A bad entry is refused by **index**, and the offending text is never echoed: it is
    /// attacker-controlled bytes and this is a response body.
    #[test]
    fn a_bad_entry_is_refused_without_echoing_it() {
        let dir = scratch("badentry");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("badentry");

        let asked = format!("[\"{}\",\"<script>alert(1)</script>\"]", Hash::of(b"fine").to_hex());
        missing(&server, asked.as_bytes(), &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
        assert!(answer.contains("entry 1"), "the refusal does not say which entry: {answer}");
        assert!(!answer.contains("script"), "the refusal echoed the entry back: {answer}");
    }

    /// A body that is not a JSON array at all is one refusal, not a parse error in a log.
    #[test]
    fn a_probe_body_that_is_not_a_json_array_is_refused() {
        let dir = scratch("notjson");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("notjson");

        missing(&server, b"deadbeef\ncafebabe\n", &socket).unwrap();
        drop(socket);

        assert!(answer_on(client).starts_with("HTTP/1.1 400 "));
    }

    /// More entries than the stated cap, refused with the count's own sentence.
    #[test]
    fn a_probe_asking_for_too_many_is_refused() {
        let dir = scratch("toomany");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("toomany");

        let one = Hash::of(b"one").to_hex();
        let entries: Vec<String> = (0..=MAX_HASHES).map(|_| format!("\"{one}\"")).collect();
        let asked = format!("[{}]", entries.join(","));
        missing(&server, asked.as_bytes(), &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.starts_with("HTTP/1.1 413 "), "{answer}");
    }
}
