//! Two-way sync — the one route in this server that can change a board.
//!
//! Every other route in `velmd` is a `GET` that reads. This one takes what a browser has
//! done and merges it into the board on disk, so it is worth being precise about what that
//! can and cannot cost, before any of it.
//!
//! # The protocol, in one round trip
//!
//! ```text
//! POST /api/v1/boards/{id}/sync
//!   request   [4-byte little-endian u32: length of the version vector][vv bytes][delta bytes]
//!   response  [4-byte little-endian u32: length of the version vector][vv bytes][update bytes]
//! ```
//!
//! The client sends the version it has plus whatever it has done since; the server applies
//! that, saves, and answers with its own version and everything the client is missing.
//!
//! There are no request ids, no acknowledgements and no retry bookkeeping, and that is not
//! an omission — Loro is a CRDT, so [`vellum_doc::Board::apply`] is commutative and
//! idempotent. A request that is lost costs one retry; a request that is *repeated* costs
//! nothing at all, because the second apply merges bytes the document already contains and
//! [`vellum_store::BoardDb::save`] compares versions and declines to write. That is why
//! every failure path below is safe to retry, including a 500 raised after a successful
//! save: the client re-sends the same two blobs and gets the same answer.
//!
//! # 🛑 RULE ZERO — what a client can and cannot do to a board
//!
//! The boards this server holds are a migration of real Miro boards that cannot be
//! re-imported. So the question is not *"is the client trusted"* — it holds the bearer
//! token, so it is — but *"what is the worst an authorised client can leave behind, and is
//! it reversible?"*
//!
//! **What is structurally impossible.** Applying a delta is a *merge* of operation
//! histories, not a replacement of the document. There is no byte sequence in Loro's update
//! format that removes history: an import can only union what the peer knows with what this
//! document already knows. So a client cannot truncate the board, cannot overwrite it with a
//! different document, and cannot discard the operations that were already there. This file
//! also removes no file and moves none — the only writes it makes are SQLite transactions
//! through [`vellum_store::BoardDb`], and `tests/rule_zero.rs` greps this crate's source to
//! keep that true rather than trusting this paragraph.
//!
//! **What is possible, and accepted.** An authorised client can author ordinary edits, and
//! that includes deletions and a rename — which is exactly what an editing client is *for*,
//! and the token is the gate on it. A hostile or simply broken one can also merge in items
//! that belong to no board anybody recognises. All of that is visible on the canvas, and all
//! of it is reversible: [`before_the_web`] takes a **labelled restore point before the first
//! change this server ever merges into a given board**, and a labelled point is never
//! pruned — not here, which prunes nothing, and not by the desktop app's
//! `prune_restore_points`, which only takes the automatic ones.
//!
//! **What was possible and is not, which is the one that mattered.** The board's `meta` map
//! carries the schema marker, and [`vellum_doc::Board::from_bytes`] refuses a board whose
//! marker is newer than the build reading it. A merged write of that key would therefore
//! make the board **refuse to open** — and it would not fail here or on the next load, since
//! `BoardDb::load` replays update chunks with no check at all. It would surface days later,
//! the first time the chunk chain collapsed into a snapshot and `from_bytes` was asked to
//! read it back. Sync is the only place with both halves in hand, so it is the only place
//! this can be caught: [`readable_afterwards`] serialises the merged document and parses it
//! again, and a document that cannot be read back is never saved. It costs a full
//! serialise-and-parse on every applied delta — **unmeasured**, because this was written on
//! a machine that could not run the build — and the cheaper form is named in the report: a
//! `schema()` getter on `Board` would make this a field compare instead.
//!
//! # The board lock
//!
//! Held across open + restore point + apply + save, and **dropped before the response is
//! written**. The lock exists so only one `BoardDb` is open per board at a time, which is a
//! correctness argument about SQLite in WAL mode; holding it across `respond` would turn it
//! into a throughput lock and let one slow reader stall every board request for the length
//! of the write timeout. That is why the work below answers an [`Outcome`] rather than
//! calling `respond` where it stands — every exit from the locked section, including the two
//! 404s, leaves the lock behind before a byte goes on the wire.

use std::collections::BTreeMap;
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use vellum_doc::{Board, Version};
use vellum_store::BoardDb;

use crate::serve::{Server, board_by_id, respond};

/// The largest request body this will read.
///
/// The number is chosen from what a delta can actually contain rather than from what a board
/// file weighs. **The document never carries pixels** — an image is an `asset_id` in the
/// document and its bytes live in the blob store — so a delta is geometry, text and style,
/// and even a client that has been offline for a week sends kilobytes. The largest board in
/// the migration this was written for is a 28 MB file, and most of that is the full snapshots
/// its restore points hold.
///
/// 8 MiB is therefore far more than any incremental delta and roughly a whole large board's
/// document, so nothing legitimate is refused. It is also small enough that the server's own
/// connection bound is the real limit: `MAX_CONNECTIONS` is 16, so sixteen clients all at the
/// cap are 128 MiB of buffers rather than a machine.
pub const MAX_BODY: usize = 8 * 1024 * 1024;

/// How long a client has to finish sending its body.
///
/// ⚠ A **wall-clock deadline**, for the reason `serve.rs`'s `HEAD_DEADLINE` already records:
/// the socket's read timeout is reset by every byte that arrives, so a client dripping one
/// byte every fourteen seconds holds a connection for as long as it likes and sixteen of them
/// take every slot this server has. The per-read timeout catches a client that stops; only a
/// deadline catches one that never stops slowly.
const BODY_DEADLINE: Duration = Duration::from_secs(30);

/// How much is asked of the socket at a time while reading a body.
const READ_CHUNK: usize = 64 * 1024;

/// ⚠ How much is reserved *before* any of the body has arrived.
///
/// Never `Vec::with_capacity(length)`: the length is a number the client chose, so trusting
/// it lets a ten-byte request reserve 8 MiB, sixteen times over, for as long as it cares to
/// hold the socket. The vector grows into what actually turns up.
const INITIAL_CAPACITY: usize = 64 * 1024;

/// What a restore point taken by this server is called.
///
/// A label rather than an automatic point, because an automatic one is eligible for pruning
/// and this is the thing being kept: the state of the board before anything reached it from a
/// browser. It is also how [`before_the_web`] knows it has already been taken, which is what
/// keeps this to one snapshot per board **ever** rather than one per server restart.
const BEFORE_THE_WEB: &str = "before the first change from the web";

const PATH_PREFIX: &str = "/api/v1/boards/";
const PATH_SUFFIX: &str = "/sync";

/// The board id in a sync path, or `None` if this is not one.
///
/// The routing rule lives here rather than in `serve.rs`'s `route` so it can be tested
/// without a socket, and so the one fact `serve.rs` has to get right — that this path is
/// under `/api/v1/`, and therefore behind `needs_token` — is asserted in the same file that
/// defines the path.
///
/// An id is compared against file stems that came out of a directory listing, never joined
/// onto a directory, so an id like `../../etc` or `a/b` matches nothing and answers 404. The
/// traversal defence is [`crate::serve::board_by_id`]'s shape, not a check here.
pub fn board_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix(PATH_PREFIX)?.strip_suffix(PATH_SUFFIX)?;
    (!id.is_empty()).then_some(id)
}

/// Why a request body was refused before a byte of it was read.
///
/// Returned rather than written, so the decision is testable without a socket and so the one
/// caller decides when the connection is spoken to.
///
/// `Debug` is safe here and is not on the other types in this file by reflex: a `Refusal`
/// holds a status and a fixed sentence, never a token and never a byte a client sent.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub message: &'static str,
}

/// How many bytes of body the request head promises, or why it will not be read.
///
/// Refusing **before** reading is the whole point: a client that declares 4 GB is turned away
/// having cost one comparison, and one that declares nothing is turned away rather than
/// having its socket read until a deadline decides.
///
/// `Transfer-Encoding` is refused outright rather than implemented. Chunked framing is a
/// second body parser reachable from the network, on a server whose entire audience is one
/// wasm client that sends `fetch` with an `ArrayBuffer` — which always sets a
/// `Content-Length`.
pub fn content_length(headers: &BTreeMap<String, String>) -> Result<usize, Refusal> {
    if headers.contains_key("transfer-encoding") {
        return Err(Refusal {
            status: 400,
            message: "send a Content-Length; this server does not read chunked bodies\n",
        });
    }
    let Some(raw) = headers.get("content-length") else {
        return Err(Refusal { status: 400, message: "a sync request needs a Content-Length\n" });
    };
    let Ok(length) = raw.trim().parse::<u64>() else {
        return Err(Refusal { status: 400, message: "that Content-Length is not a number\n" });
    };
    if length > MAX_BODY as u64 {
        return Err(Refusal { status: 413, message: "that sync request is too large\n" });
    }
    // Infallible once the cap above has passed on every target this builds for, and written
    // as a conversion anyway: `usize` is not guaranteed to be 32 bits, and a silent truncation
    // here would read a short body and call it complete.
    let Ok(length) = usize::try_from(length) else {
        return Err(Refusal { status: 413, message: "that sync request is too large\n" });
    };
    Ok(length)
}

/// Read exactly `length` bytes of body, continuing from what the head parser already has.
///
/// ⚠ **`already` is not optional and getting it wrong is the whole bug.** `serve.rs` reads
/// the request head through a `BufReader`, and a `BufReader` fills its internal buffer from
/// the socket however small the read asked for — so by the time the head ends at
/// `\r\n\r\n`, the first several kilobytes of the body are usually sitting in that buffer.
/// A sync POST is small enough to arrive in one TCP segment, so on loopback that is *every*
/// request. Those bytes must be handed here, or the body read waits for data that has already
/// been delivered and the request dies at a timeout.
///
/// `None` means the body did not arrive: end of stream before `length`, a socket error, or
/// the deadline. Every one of those is the same answer to the caller — 400, the request was
/// not complete — and none of them is worth telling the client apart.
pub fn read_body(stream: &TcpStream, already: &[u8], length: usize) -> Option<Vec<u8>> {
    // Checked here as well as in `content_length`, so the function is safe on its own rather
    // than safe because of where it happens to be called from.
    if length > MAX_BODY {
        return None;
    }
    let mut body = Vec::with_capacity(length.min(INITIAL_CAPACITY));
    // `already` can be longer than the body when a client pipelines a second request behind
    // this one. The extra is dropped, which is correct: every response this server sends
    // carries `Connection: close`, so there is no second request on this socket to serve.
    body.extend_from_slice(&already[..already.len().min(length)]);

    // The common case, by the argument above: the head parser's buffer already held the whole
    // body, so the request is answered without a scratch buffer, a socket read or a clock.
    if body.len() >= length {
        return Some(body);
    }

    let mut stream = stream;
    // Heap rather than a 64 KB array on the stack: this runs on a connection thread that also
    // opens SQLite and replays a CRDT, and a scratch buffer is not what that stack is for.
    let mut chunk = vec![0u8; READ_CHUNK];
    let began = Instant::now();
    while body.len() < length {
        if began.elapsed() > BODY_DEADLINE {
            return None;
        }
        let want = (length - body.len()).min(READ_CHUNK);
        match stream.read(&mut chunk[..want]) {
            // End of stream with the declared length unmet: the body is truncated. Answering
            // `Some` here would hand a short buffer to the frame parser, which would refuse
            // it anyway — but as "malformed frame" rather than "you did not finish sending".
            Ok(0) => return None,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    }
    Some(body)
}

/// What the locked section decided, so that the response is written after the lock is gone.
enum Outcome {
    /// The framed reply body: the server's version, then everything the client is missing.
    Synced(Vec<u8>),
    /// No board on this server answers to that id.
    NoSuchBoard,
    /// The file is there and holds no document yet.
    Unsaved,
    /// The client's delta was refused, with the reason to send back.
    Rejected(&'static str),
}

/// Answer one sync request.
///
/// The signature takes the whole [`Server`] because it needs three things from it — the data
/// directory, the board lock and the CORS origin — and threading three arguments through
/// would let a future caller pass a lock that guards something else.
pub fn handle(server: &Server, id: &str, body: &[u8], stream: &TcpStream) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();

    let Some((encoded, delta)) = split_frame(body) else {
        return respond(stream, 400, "text/plain", b"that is not a sync frame\n", origin);
    };
    // ⚠ A version vector that will not parse is **not** a 400. It degrades to "this client has
    // seen nothing", which costs the whole document in the reply and loses nothing — `apply`
    // merges what the client already had. Refusing instead would leave a client whose stored
    // vector is corrupt permanently unable to sync, with no gesture that repairs it.
    //
    // Logged, though, and that is why this is `decode` rather than `Version::decode_or_empty`
    // — whose own doc names this call site and says to reach for the reporting form where the
    // failure is worth counting. It is: the symptom of a client stuck here is the entire board
    // on the wire every poll, and without this line there is nothing that says why.
    let since = Version::decode(encoded).unwrap_or_else(|error| {
        eprintln!(
            "velmd: sync {}: a {} byte version vector did not decode ({error}); \
             answering as though this client had seen nothing",
            short(id),
            encoded.len()
        );
        Version::empty()
    });

    let outcome = {
        let _guard = server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        sync_one(&server.config.data, id, &since, delta)
    };
    // ⚠ The guard is gone by this line, and every path below is a write to a socket. The `?`
    // is deliberately out here too: an error becomes a 500 in `serve.rs`, and raising it
    // inside the block would hold the board lock across that write for the length of the
    // write timeout — which is the shape of the defect a review just found on the read path.
    match outcome? {
        Outcome::Synced(reply) => respond(stream, 200, "application/octet-stream", &reply, origin),
        Outcome::NoSuchBoard => respond(stream, 404, "text/plain", b"no such board\n", origin),
        Outcome::Unsaved => {
            // Deliberately not "create it from the delta". A `.vellum` with no document in it
            // is a board the desktop app has made and not yet written, so the client cannot
            // have taken a snapshot from it — which means the version vector it sent
            // describes some *other* document, and merging into an empty board would be this
            // server inventing board content out of a stranger's history.
            respond(stream, 404, "text/plain", b"that board holds no snapshot\n", origin)
        }
        Outcome::Rejected(reason) => respond(stream, 400, "text/plain", reason.as_bytes(), origin),
    }
}

/// Open the board, merge, save, and work out what the client is missing.
///
/// Runs with the board lock held and touches no socket, which is what keeps the lock off the
/// response write. Every early exit is a value rather than a written reply for the same
/// reason.
fn sync_one(data: &Path, id: &str, since: &Version, delta: &[u8]) -> anyhow::Result<Outcome> {
    let Some(path) = board_by_id(data, id) else {
        return Ok(Outcome::NoSuchBoard);
    };
    let mut db = BoardDb::open(&path)?;
    let Some(mut board) = db.load()? else {
        return Ok(Outcome::Unsaved);
    };

    // An empty delta is a legal request and the common one: it is a client asking *"what is
    // new?"*. It must cost no restore point, no save and no write of any kind — a poll that
    // touched the file would put this server's clients in the board's history sixty times an
    // hour and grow the file for saying nothing.
    if !delta.is_empty() {
        before_the_web(&mut db, &board)?;

        if let Err(error) = board.apply(delta) {
            // Logged with the error and answered without it. `serve.rs` already prints the
            // error behind a 500 the same way, and this request is authenticated — but the
            // client is told only that its bytes were refused, because the shape of a parser's
            // complaint is a description of the parser.
            eprintln!(
                "velmd: sync {}: a {} byte delta did not apply: {error}",
                short(id),
                delta.len()
            );
            return Ok(Outcome::Rejected("that delta did not apply to this board\n"));
        }

        if let Err(error) = readable_afterwards(&board) {
            eprintln!("velmd: sync {}: refusing to save an unreadable board: {error}", short(id));
            return Ok(Outcome::Rejected("that delta would leave this board unreadable\n"));
        }

        db.save(&board)?;
        // Explicit rather than left to `Drop`, which discards the result: this is the one
        // place in the server that has just written to an irreplaceable board, and a failure
        // to record the session as closed means the next open reports a crash. Reporting it
        // costs a 500 the client answers by retrying, which is free.
        db.close()?;
    }

    // Taken **after** the merge, so the reply carries the server's version including whatever
    // the client just sent. Asking for everything since the client's own version is what keeps
    // the answer small — the client's own operations are already covered by the vector it
    // sent, so they are not echoed back to it.
    let updates = board.export_since(since)?;
    let version = board.version().encode();
    Ok(Outcome::Synced(frame(&version, &updates)?))
}

/// Take the one restore point that makes everything this route does reversible.
///
/// Once per board, **ever** — not once per server run. The check is a scan of the board's own
/// restore points for [`BEFORE_THE_WEB`], which is cheap: `BoardDb::restore_points` selects
/// `LENGTH(snapshot)` and never reads a snapshot's bytes.
///
/// ⚠ The stateless form is deliberate and it replaced a process-global set that had been
/// written first. A labelled point is never pruned, by anything, so "once per server run"
/// meant a machine that restarts nightly adding a permanent multi-megabyte snapshot to a
/// user's board file every day with nothing that ever cleans it up — a growth defect in
/// exactly the file RULE ZERO exists to protect.
fn before_the_web(db: &mut BoardDb, board: &Board) -> anyhow::Result<()> {
    let taken = db
        .restore_points()?
        .iter()
        .any(|point| point.label.as_deref() == Some(BEFORE_THE_WEB));
    if !taken {
        db.create_restore_point(board, BEFORE_THE_WEB)?;
    }
    Ok(())
}

/// Refuse to save a document that cannot be read back.
///
/// The failure this exists for is described in the module header: a merged write of the
/// schema marker makes [`Board::from_bytes`] refuse the board, and nothing between here and
/// a snapshot collapse days later would notice, because `BoardDb::load` replays update chunks
/// without checking. Serialising and parsing is the only total check available from outside
/// `vellum-doc`, and "never write bytes we cannot read" is worth its cost on the one route in
/// this program that writes at all.
///
/// There is one **legitimate** way to trip it, and refusing is still the right answer: a
/// client built against a newer `SCHEMA_VERSION` writes the newer marker, and this server
/// cannot read the result. That reads as "sync stopped working after I upgraded the client",
/// and the honest answer is to upgrade the server — the alternative is a server that writes a
/// board it can no longer open.
fn readable_afterwards(board: &Board) -> anyhow::Result<()> {
    let bytes = board.to_bytes()?;
    Board::from_bytes(&bytes)?;
    Ok(())
}

/// Split a framed body into the version vector and the delta behind it.
///
/// ⚠ **Every read is a `get`.** The length is four bytes a stranger chose, so a body claiming
/// `u32::MAX` bytes of version vector must answer `None` rather than panic on a slice — and it
/// must do so without reserving anything, which is why nothing here allocates at all.
fn split_frame(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let header: [u8; 4] = body.get(..4)?.try_into().ok()?;
    let declared = usize::try_from(u32::from_le_bytes(header)).ok()?;
    let rest = body.get(4..)?;
    Some((rest.get(..declared)?, rest.get(declared..)?))
}

/// The wire form: the version vector's length, the vector, then everything after it.
fn frame(version: &[u8], updates: &[u8]) -> anyhow::Result<Vec<u8>> {
    let length = u32::try_from(version.len()).map_err(|_| {
        anyhow::anyhow!("a version vector of {} bytes does not fit the frame", version.len())
    })?;
    let mut out = Vec::with_capacity(4 + version.len() + updates.len());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(version);
    out.extend_from_slice(updates);
    Ok(out)
}

/// A board id, bounded **and stripped**, for a log line.
///
/// ⚠ **This doc used to say a control character could not get here, and that stopped being
/// true in the commit that made ids percent-decoded.** The reasoning was sound and was about
/// somebody else's code: `httparse` refuses a raw control byte in a request target, so nothing
/// could reach this. But `%0A` is three perfectly legal URI bytes, and the decode that makes
/// *"BMW 2020 530i g30"* work turns them into a real newline **after** httparse has finished
/// with the request.
///
/// So `POST /api/v1/boards/x%0Avelmd:%20token%20accepted/sync` wrote a second, forged line
/// into the operator's log, indistinguishable from velmd's own — and `%1b` put ANSI escapes
/// into their terminal. No board needed to exist: this is logged before the id is looked up.
///
/// This is the `locked: false` trap in its purest form: a comment that was a **true statement
/// about an upstream fact**, invalidated by a change in a different function, breaking no
/// test. The fix is not the truncation, which was always fine — it is that a string from the
/// wire now goes through `printable` like every other one.
fn short(id: &str) -> String {
    crate::serve::printable(&id.chars().take(64).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn body_of(version: &[u8], delta: &[u8]) -> Vec<u8> {
        frame(version, delta).expect("a short version vector frames")
    }

    #[test]
    fn a_frame_round_trips() {
        let framed = body_of(b"a version vector", b"some delta bytes");
        let (version, delta) = split_frame(&framed).expect("the frame it just wrote parses");
        assert_eq!(version, b"a version vector");
        assert_eq!(delta, b"some delta bytes");
        assert_eq!(framed.len(), 4 + 16 + 16);
        // Little-endian, stated as bytes rather than trusted to the platform: the other end of
        // this is a browser reading a `DataView`, and a big-endian server would hand it a
        // 268-million-byte length for a 16-byte vector.
        assert_eq!(&framed[..4], &[16u8, 0, 0, 0]);
    }

    /// A zero-length delta is the *common* request, not an edge case: it is a client asking
    /// what is new. It has to parse, and it has to parse as an empty delta rather than as a
    /// malformed frame, or every poll is a 400.
    #[test]
    fn a_zero_length_delta_is_a_legal_request() {
        let framed = body_of(b"vv", b"");
        let (version, delta) = split_frame(&framed).expect("a delta-free frame parses");
        assert_eq!(version, b"vv");
        assert!(delta.is_empty(), "an empty delta came back as {} bytes", delta.len());
    }

    /// A client that has never seen this board sends an empty version vector, which is four
    /// zero bytes and nothing else.
    #[test]
    fn an_empty_version_vector_is_a_legal_request() {
        let framed = body_of(b"", b"delta");
        let (version, delta) = split_frame(&framed).expect("an empty vector frames and parses");
        assert!(version.is_empty());
        assert_eq!(delta, b"delta");
        assert_eq!(framed, [0, 0, 0, 0, b'd', b'e', b'l', b't', b'a']);
    }

    /// ⚠ The no-slicing requirement, made executable.
    ///
    /// A length prefix is four bytes a stranger chose. `u32::MAX` against a four-byte body has
    /// to answer `None` — a slice would panic, and `panic = "abort"` in `[profile.release]`
    /// means a panic here is the server going away, not an error. It also must not reserve the
    /// length it was told, which is why `split_frame` allocates nothing whatsoever.
    #[test]
    fn a_length_prefix_larger_than_the_body_is_refused() {
        assert!(split_frame(&[0xFF, 0xFF, 0xFF, 0xFF]).is_none(), "u32::MAX was accepted");
        assert!(split_frame(&[0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3]).is_none());
        // One byte short of what it promises, which is the realistic version: a truncated
        // upload rather than a hostile one.
        assert!(split_frame(&[3, 0, 0, 0, b'a', b'b']).is_none(), "a short vector was accepted");
    }

    #[test]
    fn a_body_too_short_to_hold_a_header_is_refused() {
        for length in 0..4usize {
            let truncated = vec![0u8; length];
            assert!(
                split_frame(&truncated).is_none(),
                "{length} byte(s) parsed as a frame"
            );
        }
        // Exactly the header, and nothing after it, is a legal empty request.
        assert!(split_frame(&[0, 0, 0, 0]).is_some(), "a bare header was refused");
    }

    #[test]
    fn only_a_sync_path_is_a_sync_path() {
        assert_eq!(board_id("/api/v1/boards/products/sync"), Some("products"));
        assert_eq!(board_id("/api/v1/boards/a-board-2/sync"), Some("a-board-2"));
        for other in [
            "/api/v1/boards/products/snapshot",
            "/api/v1/boards",
            "/api/v1/boards//sync",
            "/sync",
            "/index.html",
        ] {
            assert!(board_id(other).is_none(), "{other} was routed to sync");
        }
    }

    /// ⚠ The one thing `serve.rs` has to get right about this route, asserted where the path
    /// is defined: it is under `/api/v1/`, so `needs_token` gates it. A write route reachable
    /// without the token would put ~58 irreplaceable boards behind a guessable URL, editable.
    #[test]
    fn the_sync_route_is_inside_the_token_gate() {
        assert!(
            PATH_PREFIX.starts_with("/api/v1/"),
            "the sync path moved out from under needs_token"
        );
    }

    #[test]
    fn a_body_is_measured_before_it_is_read() {
        let mut headers = BTreeMap::new();
        headers.insert("content-length".to_owned(), "128".to_owned());
        assert_eq!(content_length(&headers).map_err(|r| r.status), Ok(128));

        let at_the_cap = BTreeMap::from([("content-length".to_owned(), MAX_BODY.to_string())]);
        assert_eq!(content_length(&at_the_cap).map_err(|r| r.status), Ok(MAX_BODY));

        let over = BTreeMap::from([("content-length".to_owned(), (MAX_BODY + 1).to_string())]);
        assert_eq!(content_length(&over).map_err(|r| r.status), Err(413));

        // A declared body far past anything `usize` could hold still refuses rather than
        // wrapping into a small, plausible number.
        let absurd = BTreeMap::from([("content-length".to_owned(), "99999999999999".to_owned())]);
        assert_eq!(content_length(&absurd).map_err(|r| r.status), Err(413));

        for bad in ["", "twelve", "-1", "12 34"] {
            let headers = BTreeMap::from([("content-length".to_owned(), bad.to_owned())]);
            assert_eq!(
                content_length(&headers).map_err(|r| r.status),
                Err(400),
                "{bad:?} was accepted as a length"
            );
        }
        assert_eq!(content_length(&BTreeMap::new()).map_err(|r| r.status), Err(400));

        let chunked = BTreeMap::from([("transfer-encoding".to_owned(), "chunked".to_owned())]);
        assert_eq!(content_length(&chunked).map_err(|r| r.status), Err(400));
    }

    /// Over a real loopback socket, because the failure being tested is about what arrives
    /// and when. A test that handed `read_body` a `Vec` would be testing `min`.
    fn over_a_socket(
        already: &[u8],
        length: usize,
        writes: &[&[u8]],
        finish: bool,
    ) -> Option<Vec<u8>> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback binds");
        let addr = listener.local_addr().expect("a bound listener has an address");
        let writes: Vec<Vec<u8>> = writes.iter().map(|w| w.to_vec()).collect();
        let sender = std::thread::spawn(move || {
            let mut client = std::net::TcpStream::connect(addr).expect("loopback connects");
            for piece in &writes {
                client.write_all(piece).expect("loopback accepts a write");
                client.flush().ok();
            }
            if finish {
                // Half-close, so the reader sees end of stream rather than waiting.
                client.shutdown(std::net::Shutdown::Write).ok();
            }
            // Held open until the reader is done, or the socket dies under it.
            std::thread::sleep(Duration::from_millis(50));
        });
        let (server, _) = listener.accept().expect("the connection arrives");
        let body = read_body(&server, already, length);
        drop(server);
        sender.join().ok();
        body
    }

    /// ⚠ The leftover from the head parser is part of the body and is not optional.
    ///
    /// `serve.rs` reads the head through a `BufReader`, which fills its internal buffer from
    /// the socket however small the read asked for — so a sync POST small enough to arrive in
    /// one segment lands its whole body in that buffer before the head loop even ends. If it
    /// is not carried across, this read waits for bytes that have already been delivered.
    #[test]
    fn a_body_already_buffered_by_the_head_parser_is_used() {
        let framed = body_of(b"vv", b"delta");
        let body = over_a_socket(&framed, framed.len(), &[], false)
            .expect("a body that had already arrived was not read");
        assert_eq!(body, framed);
    }

    #[test]
    fn a_body_that_arrives_in_pieces_is_still_read() {
        let framed = body_of(b"vv", b"a longer delta");
        let (head, tail) = framed.split_at(5);
        let body = over_a_socket(&framed[..2], framed.len(), &[&head[2..], tail], false)
            .expect("a body split across three deliveries was not reassembled");
        assert_eq!(body, framed);
    }

    /// A client that promises more than it sends and then goes away is refused, rather than
    /// its short buffer being handed on as though it were whole.
    #[test]
    fn a_truncated_body_is_refused() {
        assert!(
            over_a_socket(b"", 32, &[b"a partial body"], true).is_none(),
            "a body that stopped early was accepted"
        );
        assert!(
            over_a_socket(b"1234", 32, &[], true).is_none(),
            "a body that never continued was accepted"
        );
    }

    /// A second request pipelined behind the first is dropped rather than treated as body.
    /// Every response carries `Connection: close`, so there is nothing on this socket to
    /// serve after this one.
    #[test]
    fn bytes_past_the_declared_length_are_not_part_of_the_body() {
        let framed = body_of(b"vv", b"delta");
        let mut with_extra = framed.clone();
        with_extra.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n");
        let body = over_a_socket(&with_extra, framed.len(), &[], false)
            .expect("the body itself was refused");
        assert_eq!(body, framed);
    }

    /// The cap is enforced by the reader too, so it holds however the length got here.
    #[test]
    fn a_body_over_the_cap_is_refused_by_the_reader_as_well() {
        assert!(over_a_socket(b"", MAX_BODY + 1, &[], true).is_none());
    }

    #[test]
    fn a_log_line_from_a_board_id_is_bounded() {
        assert_eq!(short("products"), "products");
        // ⚠ The decode that makes a board named "BMW 2020 530i g30" reachable is also what
        // lets `%0A` become a real newline here, after httparse has stopped looking.
        assert_eq!(short("x\nvelmd: token accepted"), "x.velmd: token accepted");
        assert_eq!(short("x\u{1b}[2J"), "x.[2J");
        // Category Cf, which `char::is_control` does not cover: a bidi override renders the
        // rest of the line backwards.
        assert_eq!(short("a\u{202e}b"), "a.b");
        assert_eq!(short(&"x".repeat(9999)).chars().count(), 64);
    }
}
