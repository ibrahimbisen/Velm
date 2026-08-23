//! `POST /api/v1/import` — a Miro clipboard payload arrives, a **new** board leaves.
//!
//! # Why the import lives on the server
//!
//! [`vellum_import`] reads Miro's clipboard format, and it writes what it reads into a
//! [`vellum_doc::Board`] and a [`vellum_store::BlobStore`] — so it needs SQLite, and SQLite is
//! the one thing `docs/08-web.md`'s whole design keeps out of the browser. `velmd` already has
//! both halves natively and unchanged, which makes this the only place the import can run at
//! all. The browser posts the payload, is told the new board's id, and opens it over the
//! snapshot and sync routes it already speaks.
//!
//! # 🛑 RULE ZERO — this route creates, and that is the whole of what it does
//!
//! Every other statement about safety here follows from one property: **the path this route
//! writes to did not exist when it was chosen.** [`free_path`] steps past any name that is
//! taken until it finds one that is not, exactly as the desktop app's `Library::free_path`
//! does — and it is reimplemented here rather than borrowed, because `velmd` must not depend
//! on `vellum-app`. So:
//!
//! - **No board is ever opened that already existed.** A second [`vellum_store::BoardDb`] over
//!   a live board file is the one mistake in this codebase with an irreversible outcome, and
//!   the way to be incapable of it is to never name an existing file.
//! - **Nothing here truncates.** There is no `File::create`, no `fs::write`, no copy and no
//!   move anywhere below; the only write is a SQLite transaction through `BoardDb` onto a path
//!   nothing was using. `tests/rule_zero.rs` greps this crate's own source to keep that true
//!   rather than trusting this paragraph.
//! - **A name that is taken includes its sidecars.** SQLite runs in WAL mode, so a board is
//!   three files — `x.vellum`, `x.vellum-wal`, `x.vellum-shm` — and CLAUDE.md records that a
//!   `-wal` left behind *replays its items back* into a file recreated at that name. A board
//!   whose `.vellum` is gone and whose log is not is not a free name; it is somebody else's
//!   content waiting to be adopted by whatever opens next. [`is_free`] asks about all three.
//!
//! # There is no selection to clear
//!
//! Worth stating rather than leaving to be rediscovered, because the defect is real: CLAUDE.md's
//! feedback 6 records an import that left all 596 items selected, which the user experienced as
//! *"select and move doesn't work"*. It cannot happen here, and not by care — **a selection is
//! `vellum-app` state and not document state.** `vellum-doc` mentions the word only in comments;
//! there is no field on a board, an item or a placement that could hold one. So a board this
//! route creates reaches the browser with nothing selected because there was nowhere for a
//! selection to have been written.
//!
//! # The ordering, which is the behaviour and not a preference
//!
//! **The import runs to completion in memory, before any file is created, and outside the
//! board lock.** Reversing those two halves is a real defect and not a style question:
//!
//! - **`velmd` can never clean up after itself.** It removes no file, by design and by test —
//!   so a board file created *before* an import that then answers "this was not Miro's" or
//!   fails halfway is a stray `.vellum` that nothing in this program is permitted to take
//!   away, ever. Every failure below leaves **zero bytes** on disk.
//! - It keeps the expensive half off the lock. `Board::add` is O(n²) in Loro's insert — 1k
//!   items in 18 ms, 100k in 19.6 s, measured — and at this route's cap that is the better
//!   part of a second. Only the save is serialised, which is `sync.rs`'s own rule: the lock
//!   exists so one board is open at a time, and anything else held under it is a throughput
//!   tax on every other request.
//!
//! It is also the desktop's rule seen from the other side. `actions::import_to_new_board`
//! creates and *opens* the board first, because there `paste` acts on whatever is in front of
//! the user and the order is what stops 596 items landing in the board they had open. Here
//! there is no "in front of", the board is a value, and the hazard is the opposite one.
//!
//! # The body cap
//!
//! There is no cap constant in this file, deliberately: the cap is [`crate::sync::MAX_BODY`],
//! **8 MiB**, applied by [`crate::sync::content_length`] before a byte of body is read.
//! Three reasons for reusing it rather than declaring a second number.
//!
//! - **A larger cap here would be silently unreachable.** [`crate::sync::read_body`] enforces
//!   `MAX_BODY` internally as well, so a 16 MiB import would be refused as *"that request body
//!   did not arrive"* — a truthful-sounding 400 about the wrong thing.
//! - **One content-length parser.** Two copies of a header parser is the sibling-drift defect
//!   CLAUDE.md's feedback 35 is entirely about: one gets a fix and the other does not.
//! - **The number fits this route on its own merits.** A clipboard payload is base64 text with
//!   no pixels in it — an image is a *resource reference* resolved from a `.rtb`, which this
//!   route does not take — and the measured reference payload for the 596-widget board is
//!   **1.1 MB**. 8 MiB is roughly seven times that, or about 4,300 widgets, which is larger
//!   than any board in the migration this was written for. It also means adding this route
//!   does not move the server's worst-case buffer ceiling at all: `MAX_CONNECTIONS` is 16, and
//!   16 × 8 MiB was already the accepted bound.
//!
//! # What a `.rtb` would add, and why there is no archive here
//!
//! [`vellum_import::pipeline::import`] takes an optional [`vellum_import::rtb::ArchiveSet`],
//! and it is the **only** source of image bytes — the clipboard carries references. This route
//! passes `None`, so an imported picture arrives placed, sized and counted, with no pixels,
//! and is reported in the answer's `missing_assets`. That is honest rather than ideal: an
//! archive is a 114 MB upload, which is a different route with a different cap, and `--data`
//! is a copy directory on a server rather than a folder the user drops files into.

use std::ffi::OsString;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use vellum_doc::Board;
use vellum_store::{BOARD_EXTENSION, BlobStore, BoardDb};

use crate::serve::{Server, printable, respond};

/// The route, spelled once.
///
/// ⚠ **Under `/api/v1/`, so `serve::needs_token` gates it**, and a test in this file asserts
/// that rather than leaving it to be noticed. A route that creates boards, reachable without
/// the token, would let anyone who can reach the socket fill the disk this person's
/// irreplaceable boards live on.
pub(crate) const PATH: &str = "/api/v1/import";

/// Whether a request path is this route.
///
/// Exact equality, not a prefix: there is nothing under this path, and a prefix match would
/// route `/api/v1/importer` here as well. Stated as a function so `serve.rs` asks rather than
/// spells, and so the rule is testable without a socket — the shape [`crate::sync::board_id`]
/// already uses for the same reason.
pub(crate) fn is_import(path: &str) -> bool {
    path == PATH
}

/// What a board is called when the request does not say.
///
/// The same words the desktop falls back to in `actions::clipboard_board_name`, because the
/// two produce boards that sit in the same picker and a person should not be able to tell
/// which door a board came through.
const FALLBACK_TITLE: &str = "Miro import";

/// How much of a supplied title is kept.
///
/// A title is document content and a board file has no opinion about its length; this is
/// about the board picker, where a title is one line in a card. The desktop is no stricter,
/// so this only ever trims something no interface could show.
const MAX_TITLE: usize = 120;

/// How many names [`free_path`] will try before giving up.
///
/// `Library::free_path` loops without a bound, which is fine in an application a person is
/// driving and is not fine on a socket: a directory holding `x`, `x-2` … `x-n` for a large `n`
/// is a request that stats `n` files under the board lock. Ten thousand is far past any real
/// library and turns a pathological one into an error rather than a stall.
const MAX_ATTEMPTS: usize = 10_000;

/// Answer one import request.
///
/// Takes the whole [`Server`] for the same reason [`crate::sync::handle`] does: it needs the
/// data directory, the blob store and the board lock, and three separate arguments would let a
/// later caller hand over a lock guarding something else.
///
/// `query` is the request's query string, and it is read for **one** key. ⚠ It is never logged
/// whole and must not be: `serve::authorised` accepts `?token=…`, so the query string is a
/// place this server's own secret legitimately appears.
pub(crate) fn handle(
    server: &Server,
    query: &str,
    body: &[u8],
    caller: Option<&crate::accounts::Caller>,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();

    // ⚠ **Strict, never `from_utf8_lossy`.** A clipboard payload is base64 inside an HTML
    // attribute, and lossy decoding replaces every bad byte with U+FFFD — which does not fail,
    // it produces a *different* payload that decodes to a different board. CLAUDE.md's trap 2
    // is this lesson about the delimiter and the general rule is the same one: decode strictly,
    // because leniency here hides the bug instead of surviving it.
    let Ok(html) = std::str::from_utf8(body) else {
        let why = b"an import payload must be UTF-8 text\n";
        return respond(stream, 400, "text/plain", why, origin);
    };
    let title = title_in(query);

    let blobs = BlobStore::open(&server.config.blobs)?;

    // ⚠ **Everything above the lock, and nothing on disk yet.** See the module header: this
    // program cannot remove a file, so a failure after a board file exists leaves litter
    // nothing is allowed to sweep up.
    //
    // `None` for the archive: there is no `.rtb` on this server, so items that need one import
    // without their pixels and say so in the answer.
    let imported = vellum_import::pipeline::import_to_new_board(html, None, &blobs);
    let (mut board, outcome) = match imported {
        Ok(Some(pair)) => pair,
        // Not Miro's HTML at all. The ordinary answer to a paste from anywhere else, and a 400
        // rather than a 500: the request was understood and the bytes were not what this route
        // takes.
        Ok(None) => {
            return respond(
                stream,
                400,
                "text/plain",
                b"that is not a Miro clipboard payload\n",
                origin,
            );
        }
        // ⚠ It *was* Miro's and could not be read — or the blob store could not be written,
        // which this error type cannot tell apart from the first. Answered 400 because the
        // overwhelmingly likely cause is the bytes, and logged in full because the other cause
        // is the operator's disk and only this line would ever say so.
        Err(error) => {
            eprintln!("velmd: import: a {} byte payload did not import: {error:#}", body.len());
            return respond(
                stream,
                400,
                "text/plain",
                b"that Miro payload could not be imported\n",
                origin,
            );
        }
    };

    // The title is set here rather than left to `import_to_new_board`, whose own naming step
    // reads the archive — so with `None` for the archive it is inert and the board would carry
    // the empty string `Board::new` starts with.
    board.set_title(&title)?;

    // ⚠ Refuse to create a file holding a document that cannot be read back, **before** the
    // file exists. `crate::sync` runs the same check for a sharper reason — a merged write of
    // the schema marker makes `from_bytes` refuse a board days later — and here the value is
    // the ordering: the last fallible thing before a name is claimed is a round trip through
    // the format, so a board this route creates has been read once already. It costs one extra
    // serialise on top of the save's own, which is **unmeasured**.
    Board::from_bytes(&board.to_bytes()?)?;

    let path = {
        // ⚠ The lock covers the *choice* of name as well as the write, and it has to. Two
        // imports arriving together would otherwise both see `miro-import.vellum` free, and
        // the second `BoardDb::open` would be a second writer on the first one's board — the
        // one outcome this whole file is arranged to make impossible.
        let _guard = server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        create(&server.config.data, &board, &title)?
    };
    // ⚠ The guard is gone by this line, and everything below writes to a socket. Holding it
    // across `respond` would turn a correctness lock into a throughput one and let a single
    // slow reader stall every board request for the length of the write timeout — the defect
    // a review already found on the read path.

    let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
        anyhow::bail!("the board just created has no usable id: {}", path.display())
    };

    report(id, &outcome);
    // ⚠ **Whoever imported it owns it**, and it is recorded here rather than inside the
    // pipeline: `vellum-import` knows nothing about accounts, and a board that arrives owned
    // by nobody silently belongs to the founder — right as a fallback, wrong as the answer
    // when somebody is signed in and asked for it.
    if let Some(caller) = caller {
        crate::accounts::claim_board(server, id, caller);
    }
    respond(stream, 200, "application/json", summary(id, &title, &outcome).as_bytes(), origin)
}

/// Write the board to a name nothing was using, and answer where it went.
///
/// Called with the board lock held. Split out from [`handle`] so the property that matters can
/// be tested without a socket: **calling this twice with the same title produces two boards**,
/// and neither of them is the other.
fn create(data: &Path, board: &Board, title: &str) -> anyhow::Result<PathBuf> {
    let path = free_path(data, title)?;
    // RULE ZERO: `BoardDb::open` creates and initialises a database at this path, and `save`
    // writes a snapshot into it. Neither can destroy a board's contents *here*, because
    // `free_path` has just established that nothing exists at this name — not the `.vellum`,
    // not its write-ahead log, not its shared-memory sidecar — and the board lock is held
    // across the check and the create, so nothing else in this process can claim the name in
    // between. This route never names a file that was already there.
    let mut db = BoardDb::open(&path)?;
    db.save(board)?;
    // Explicit rather than left to `Drop`, which discards the result: a failure to mark the
    // session closed makes the next open of this brand-new board report a crash.
    db.close()?;
    Ok(path)
}

/// A board path in `data` that nothing is using.
///
/// The desktop app's `Library::free_path` — `board`, then `board-2`, `board-3` — reimplemented
/// rather than borrowed, because `velmd` must not depend on `vellum-app`. The suffix matters
/// more than it looks: this person's Miro account has four boards called *Untitled*, and a
/// migration that answered "that name is taken" to three of them would be a migration nobody
/// could finish.
fn free_path(data: &Path, title: &str) -> anyhow::Result<PathBuf> {
    let stem = slug(title);
    let mut candidate = data.join(format!("{stem}.{BOARD_EXTENSION}"));
    for n in 2..MAX_ATTEMPTS {
        if is_free(&candidate) {
            return Ok(candidate);
        }
        candidate = data.join(format!("{stem}-{n}.{BOARD_EXTENSION}"));
    }
    anyhow::bail!("there are already {MAX_ATTEMPTS} boards called {stem:?} in {}", data.display())
}

/// Whether a board name is genuinely unused — **all three files of it**.
///
/// ⚠ A board in WAL mode is `x.vellum` plus `x.vellum-wal` plus `x.vellum-shm`, and CLAUDE.md
/// records the consequence in the one line worth repeating: resetting a scratch board with
/// `rm x.vellum` rather than `rm -f x.vellum*` puts the items *back*, because the log replays
/// into the file recreated at that name. So a directory holding a log with no board is not
/// offering a free name — it is offering somebody else's content to whatever opens there next.
/// `velmd` never removes a file, so it can never be the cause of that state; it can very
/// easily be the thing that walks into one.
fn is_free(candidate: &Path) -> bool {
    if candidate.exists() {
        return false;
    }
    ["-wal", "-shm"].iter().all(|suffix| {
        let mut sidecar = OsString::from(candidate.as_os_str());
        sidecar.push(suffix);
        !Path::new(&sidecar).exists()
    })
}

/// A title, as a file stem.
///
/// The desktop app's `library::slug`, reimplemented for the reason [`free_path`] gives, and it
/// is also the whole traversal defence on this route: **only ASCII letters and digits survive**
/// and everything else collapses to a hyphen, so there is no spelling of `..`, `/`, a NUL, a
/// drive letter or a leading dash that can come out of it. The path is then joined onto the
/// data directory rather than compared against a listing — the opposite of `board_by_id`'s
/// approach — which is safe precisely because this function's output alphabet is two dozen
/// characters wide.
fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        // A title of nothing but punctuation, or of nothing but non-ASCII — a board named in
        // Chinese slugs to the empty string, and an empty stem is `.vellum`, a hidden file
        // `list_boards` would happily serve and nobody could name.
        "board".to_owned()
    } else {
        trimmed.chars().take(64).collect()
    }
}

/// The title the request asked for, or the fallback.
///
/// ⚠ **One key is read and the rest of the query is not touched.** `serve::authorised` accepts
/// `?token=…`, so a query string is somewhere this server's own secret legitimately appears —
/// which is why nothing here returns, logs or echoes anything but the value of `title`.
///
/// `+` stays a literal plus, following `serve::percent_decode`'s own note: that convention
/// belongs to form-encoded *bodies*, and `encodeURIComponent` — which is what the client uses —
/// emits `%20` for a space. Reading it as a space would rename a board called `C++ notes`.
fn title_in(query: &str) -> String {
    for pair in query.split('&') {
        if let Some(raw) = pair.strip_prefix("title=") {
            let decoded = crate::serve::percent_decode(raw);
            let trimmed = decoded.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(MAX_TITLE).collect();
            }
        }
    }
    FALLBACK_TITLE.to_owned()
}

/// What the client is told: the id it needs, and what the import cost.
///
/// ⚠ **Every string here comes off the wire and goes through `serve::json_string`.** The title
/// is the caller's, and `source_board_id` is a field of the payload — a board id with a quote
/// in it would otherwise produce JSON the client cannot parse, which reads as the import having
/// failed after it succeeded.
///
/// `ImportOutcome::lossless` is deliberately not reported, and the reason is arithmetic rather
/// than taste: it is `total − degraded − missing` over `usize`, and nothing outside
/// `vellum-import` can establish that no item is ever counted in both subtrahends. Whether one
/// can is **unverified** — if it ever is, that expression wraps rather than saturating, and
/// `[profile.release]` has overflow checks off, so the client would be served a number near
/// `usize::MAX` as fact. The two figures it is built from are sent instead, which says the same
/// thing and cannot be wrong.
fn summary(id: &str, title: &str, outcome: &vellum_import::ImportOutcome) -> String {
    let degraded: usize = outcome.degraded.iter().map(|(_, n)| n).sum();
    let source = match &outcome.source_board_id {
        Some(source) => crate::serve::json_string(source),
        None => "null".to_owned(),
    };
    format!(
        "{{\"id\":{},\"title\":{},\"items\":{},\"degraded\":{degraded},\
         \"assets_recovered\":{},\"missing_assets\":{},\"source_board_id\":{source}}}",
        crate::serve::json_string(id),
        crate::serve::json_string(title),
        outcome.total(),
        outcome.assets_recovered,
        outcome.missing_assets.len(),
    )
}

/// The import, on the operator's terminal.
///
/// ⚠ **Line by line, each through `serve::printable`.** `ImportOutcome`'s `Display` is a report
/// of many lines and it carries strings from the payload — the keys of `unmapped_types`, the
/// source board id, every entry in `report.warnings`. One `printable` over the whole thing
/// would turn each of those newlines into a dot and hand back a single 1,000-character smear
/// with an ellipsis where the interesting half was.
///
/// The id needs it least and gets it anyway: it comes out of [`slug`], whose alphabet has no
/// control character in it — but a guard that depends on a fact established in another function
/// is the `locked: false` trap, and CLAUDE.md records a log injection through a board id that
/// shipped for exactly that reason.
fn report(id: &str, outcome: &vellum_import::ImportOutcome) {
    println!("velmd: imported a Miro payload as {}", printable(id));
    for line in outcome.to_string().lines() {
        println!("  {}", printable(line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::{Value, json};
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A scratch directory that is unique per call and is never cleared.
    ///
    /// Named from an atomic counter rather than the clock, following `manifest.rs` — CLAUDE.md
    /// records a fixture whose directory came from `unix_now()` in *seconds*, so two tests
    /// starting in the same second shared one and the first to finish deleted the other's data.
    /// Nothing is removed here either, which is what lets `tests/rule_zero.rs` scan this crate
    /// for `remove_` and expect zero hits with no exemption for test code.
    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("velmd-paste-{name}-{n}-{pid}"));
        std::fs::create_dir_all(dir.join("boards")).unwrap();
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        dir
    }

    /// Wraps objects the way Miro's clipboard does.
    ///
    /// ⚠ **The closing marker is here on purpose.** CLAUDE.md's trap 2: a payload is
    /// *delimited*, not prefixed, and feeding `(/miro-data-v1)` into base64 corrupts it. A
    /// fixture that omitted the marker would exercise the lenient path and prove nothing about
    /// the strict one, which is the only path a real paste takes. Byte-for-byte the shape
    /// `pipeline.rs`'s own tests build, so this fixture and the decoder cannot drift.
    fn clipboard(objects: Value) -> String {
        let payload =
            json!({ "boardId": "bTBja0JvYXJkSWQ=", "version": 2, "data": { "objects": objects } })
                .to_string();
        let shifted: Vec<u8> = payload.bytes().map(|b| b.wrapping_sub(197)).collect();
        format!(
            "<span data-meta=\"&lt;--(miro-data-v1){}(/miro-data-v1)--&gt;\"></span>",
            STANDARD.encode(&shifted)
        )
    }

    /// One sticky, placed absolutely so nothing depends on parent resolution.
    fn one_sticky(text: &str) -> Value {
        json!([{
            "id": 1,
            "type": 14,
            "widgetData": {
                "type": "sticker",
                "json": {
                    "_position": {
                        "offsetPx": { "x": 10.0, "y": 20.0 },
                        "schema": "canvasOffsetPx"
                    },
                    "size": { "width": 199, "height": 228 },
                    "text": format!("<p>{text}</p>")
                }
            }
        }])
    }

    fn server_for(dir: &Path) -> Server {
        Server {
            // Fresh and empty: these tests are about the importer, and an account store that
            // came from anywhere else would make them depend on a file none of them writes.
            accounts: std::sync::Mutex::new(crate::accounts::Accounts::open(dir)),
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

    /// A real loopback pair, because [`handle`] answers by writing to a socket.
    ///
    /// The server half is what `handle` is given; the client half is read afterwards so the
    /// status line can be asserted. A small JSON answer fits the kernel's buffer, so nothing
    /// here has to be read concurrently to keep the write from blocking.
    fn a_socket_pair(label: &str) -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{label}: {e}"));
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{label}: {e}"));
        let (server, _) = listener.accept().unwrap_or_else(|e| panic!("{label}: {e}"));
        (server, client)
    }

    /// Everything the client half received, as text.
    fn answer_on(mut client: TcpStream) -> String {
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
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

    /// How many files a directory holds, at the top level.
    fn files_in(dir: &Path) -> usize {
        std::fs::read_dir(dir).map(|listing| listing.flatten().count()).unwrap_or(0)
    }

    /// ⚠ The one thing `serve.rs` has to get right about this route, asserted where the path is
    /// defined. A route that *creates boards* reachable without the token would let anyone who
    /// can reach the socket write into the directory holding boards that cannot be re-imported.
    #[test]
    fn the_import_route_is_inside_the_token_gate() {
        assert!(PATH.starts_with("/api/v1/"), "the import path moved out from under needs_token");
    }

    /// Exact equality, so nothing near it is routed here.
    #[test]
    fn only_the_import_path_is_the_import_path() {
        assert!(is_import(PATH));
        for other in [
            "/api/v1/importer",
            "/api/v1/import/",
            "/api/v1/import/x",
            "/api/v1/boards/a/sync",
            "/import",
            "/",
        ] {
            assert!(!is_import(other), "{other} was routed to the importer");
        }
    }

    /// The cap is `sync`'s, and this fails the **build** if that number is ever tightened past
    /// what a real payload weighs — which would break this route silently, as a 400 about a
    /// body that did not arrive.
    ///
    /// A `const` block rather than a `#[test]`, because both sides are constants and clippy is
    /// right that a run-time assertion over two of them asserts nothing the compiler did not
    /// already know. This form is strictly stronger: it fails at `cargo build`, not at
    /// `cargo test`, so the number cannot be lowered on a branch nobody ran the suite on.
    const _: () = assert!(
        crate::sync::MAX_BODY >= 4 * 1024 * 1024,
        "the shared body cap no longer clears a real Miro payload; the measured reference is \
         1.1 MB and this route has no cap of its own"
    );

    #[test]
    fn a_title_is_read_from_the_query_and_nothing_else_is() {
        assert_eq!(title_in("title=BMW%202020%20530i"), "BMW 2020 530i");
        assert_eq!(title_in("token=deadbeef&title=Sensors"), "Sensors");
        assert_eq!(title_in("title=Sensors&token=deadbeef"), "Sensors");
        // No title, an empty one, and one that is only spaces all fall back rather than
        // producing a board with no name.
        assert_eq!(title_in("token=deadbeef"), FALLBACK_TITLE);
        assert_eq!(title_in(""), FALLBACK_TITLE);
        assert_eq!(title_in("title="), FALLBACK_TITLE);
        assert_eq!(title_in("title=%20%20"), FALLBACK_TITLE);
        // ⚠ A plus is a plus, and this fixture is the rule rather than an example of it.
        // `percent_decode` keeps `+` literal on purpose — that convention belongs to
        // form-encoded *bodies* — and `encodeURIComponent` sends `%20` for a space, so a board
        // called `C++ notes` arrives spelled out and comes back whole.
        assert_eq!(title_in("title=C%2B%2B%20notes"), "C++ notes");
        assert_eq!(title_in("title=a+b"), "a+b", "a plus was read as a space");
        // Bounded, so a megabyte of query does not become a megabyte of board title.
        let long = "t".repeat(10_000);
        assert_eq!(title_in(&format!("title={long}")).chars().count(), MAX_TITLE);
    }

    /// The traversal defence on this route, stated as the property rather than as a list of
    /// attempts: a stem out of [`slug`] is ASCII letters, digits and hyphens, so there is
    /// nothing in it that a path could be built out of.
    #[test]
    fn a_slug_cannot_name_anything_outside_the_directory() {
        for hostile in [
            "../../etc/passwd",
            "..",
            "/etc/shadow",
            "a/b\\c",
            "board\0name",
            "C:\\Windows",
            ".hidden",
        ] {
            let stem = slug(hostile);
            assert!(
                stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "{hostile:?} slugged to {stem:?}, which is not a bare stem"
            );
            assert!(!stem.is_empty(), "{hostile:?} slugged to nothing");
            assert_eq!(Path::new(&stem).components().count(), 1, "{stem:?} is more than one part");
        }
        // A title with no ASCII in it at all still names a file, rather than making `.vellum`.
        assert_eq!(slug("宝马"), "board");
        assert_eq!(slug("   "), "board");
        assert_eq!(slug("BMW 2020 530i g30"), "bmw-2020-530i-g30");
    }

    /// 🛑 The RULE ZERO property, driven rather than argued: importing twice under one name
    /// leaves **two** boards, and the first one still says what it said.
    #[test]
    fn a_second_import_of_the_same_name_never_touches_the_first() {
        let dir = scratch("collision");
        let data = dir.join("boards");

        // Two different documents under one name, so the assertion below can tell a board
        // that survived from a board that was written over with something identical.
        let mut first = Board::new();
        first.set_title("the first board").unwrap();
        let one = create(&data, &first, "Miro import").unwrap();

        let mut second = Board::new();
        second.set_title("the second board").unwrap();
        let two = create(&data, &second, "Miro import").unwrap();

        assert_eq!(one.file_name().unwrap(), "miro-import.vellum");
        assert_eq!(two.file_name().unwrap(), "miro-import-2.vellum");
        assert!(one.exists() && two.exists());

        // ⚠ The half a path comparison cannot see, and the half this test exists for: board
        // one still holds what it held. A build that opened the first board and saved the
        // second into it satisfies every assertion above this line.
        let mut back = BoardDb::open(&one).unwrap();
        assert_eq!(back.load().unwrap().unwrap().title(), "the first board");
        let mut other = BoardDb::open(&two).unwrap();
        assert_eq!(other.load().unwrap().unwrap().title(), "the second board");
    }

    /// ⚠ A write-ahead log with no board beside it is **not** a free name: recreating the
    /// `.vellum` there replays the log's items into it. A/B this by removing the sidecar half
    /// of [`is_free`] — the assertion below fails with the new board landing on `-wal`'s stem.
    #[test]
    fn a_stale_write_ahead_log_makes_a_name_taken() {
        let dir = scratch("sidecar");
        let data = dir.join("boards");
        std::fs::write(data.join("miro-import.vellum-wal"), b"a log from a board that was here")
            .unwrap();

        let path = free_path(&data, "Miro import").unwrap();
        assert_eq!(
            path.file_name().unwrap(),
            "miro-import-2.vellum",
            "a board was about to be created on top of a stale write-ahead log"
        );

        // And the shared-memory sidecar counts too — the sibling that a fix applied to one of
        // the two would miss.
        std::fs::write(data.join("miro-import-2.vellum-shm"), b"shared memory").unwrap();
        let third = free_path(&data, "Miro import").unwrap();
        assert_eq!(third.file_name().unwrap(), "miro-import-3.vellum");
    }

    /// 🛑 The sequencing, made executable: a body that is not Miro's must leave the data
    /// directory **exactly as it found it**. This program cannot remove a file, so a board
    /// created before the import was known to be good is litter forever.
    #[test]
    fn a_payload_that_is_not_miros_creates_nothing() {
        let dir = scratch("not-miro");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("not-miro");

        handle(&server, "", b"<p>copied from a web page</p>", None, &socket).unwrap();
        drop(socket);

        assert!(answer_on(client).starts_with("HTTP/1.1 400 "), "a foreign paste was accepted");
        assert_eq!(files_in(&server.config.data), 0, "a failed import left a file behind");
    }

    /// Bytes that are not text at all — a screenshot posted here by mistake. Refused before
    /// anything is decoded, and nothing is created.
    #[test]
    fn a_body_that_is_not_text_is_refused_before_it_is_decoded() {
        let dir = scratch("binary");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("binary");

        handle(&server, "", &[0x89, b'P', b'N', b'G', 0xFF, 0xFE], None, &socket).unwrap();
        drop(socket);

        assert!(answer_on(client).starts_with("HTTP/1.1 400 "));
        assert_eq!(files_in(&server.config.data), 0);
    }

    /// The whole route, end to end: a delimited payload in, a board on disk, and an id the
    /// board routes already answer to.
    #[test]
    fn a_miro_payload_becomes_a_board_the_other_routes_can_find() {
        let dir = scratch("happy");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("happy");

        let payload = clipboard(one_sticky("fan"));
        handle(&server, "title=BMW%202020%20530i%20g30", payload.as_bytes(), None, &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.starts_with("HTTP/1.1 200 "), "the import was refused: {answer}");
        assert!(answer.contains("\"id\":\"bmw-2020-530i-g30\""), "no id in {answer}");
        assert!(answer.contains("\"items\":1"), "no item count in {answer}");

        // ⚠ Found the way `serve.rs` finds it — by file stem, through the same function the
        // snapshot and sync routes use. An import that produced a board those two could not
        // name would be an import the browser cannot open.
        let path = crate::serve::board_by_id(&server.config.data, "bmw-2020-530i-g30")
            .expect("the new board is not in the listing");
        let mut db = BoardDb::open(&path).unwrap();
        let board = db.load().unwrap().expect("the new board holds no document");
        assert_eq!(board.title(), "BMW 2020 530i g30");
        assert_eq!(board.item_count(), 1);
    }

    /// With no title in the query the board still gets a name, and the id is the fallback's
    /// slug rather than something derived from the payload.
    #[test]
    fn an_untitled_import_is_named_rather_than_left_blank() {
        let dir = scratch("untitled");
        let server = server_for(&dir);
        let (socket, client) = a_socket_pair("untitled");

        handle(&server, "", clipboard(one_sticky("a")).as_bytes(), None, &socket).unwrap();
        drop(socket);

        let answer = answer_on(client);
        assert!(answer.contains("\"id\":\"miro-import\""), "no fallback id in {answer}");
        let path = crate::serve::board_by_id(&server.config.data, "miro-import").unwrap();
        let mut db = BoardDb::open(&path).unwrap();
        assert_eq!(db.load().unwrap().unwrap().title(), FALLBACK_TITLE);
    }

    /// A title with a quote in it must not produce JSON the client cannot parse — the failure
    /// that reads as the import having gone wrong after it went right.
    #[test]
    fn a_title_with_a_quote_in_it_stays_valid_json() {
        let json = summary("a-board", "the \"good\" board", &empty_outcome(0));
        assert!(json.contains("\\\"good\\\""), "{json}");
        assert_eq!(json.matches('{').count(), 1, "{json}");
    }

    /// A stand-in outcome, so [`summary`] can be tested without running an import.
    fn empty_outcome(items: usize) -> vellum_import::ImportOutcome {
        let mut board = Board::new();
        let blobs = BlobStore::open(scratch("outcome").join("blobs")).unwrap();
        let objects: Vec<Value> = (0..items).map(|_| one_sticky("x")[0].clone()).collect();
        vellum_import::import(&clipboard(Value::Array(objects)), None, &blobs, &mut board)
            .unwrap()
            .unwrap()
    }
}
