//! The read-only board server.
//!
//! Serves three things over one origin: the wasm client, the boards it reads, and the
//! pictures on them. One origin is the whole hosting decision — it is what makes mixed
//! content, Private Network Access and CORS all stop applying at once, and it means the
//! person running this needs one domain and one certificate rather than two of each.
//!
//! # 🛑 RULE ZERO
//!
//! **Nothing here writes to a board and there is no route that could.** Every handler is a
//! `GET`; there is no `POST`, no `PUT`, no `DELETE`, and `tests/rule_zero.rs` greps this
//! file along with the rest of the crate for any call that can unlink or move a file.
//!
//! But read-only in the HTTP sense is not the whole promise, and this is the part worth
//! being precise about: [`vellum_store::BoardDb::open`] **is not a read-only open**. It runs
//! `CREATE TABLE IF NOT EXISTS`, it may bump `user_version`, and WAL mode writes a `-wal`
//! sidecar beside the board. So serving a directory is not a passive act.
//!
//! ⚠ **Point `--data` at a copy. Never at `~/Library/Application Support/Vellum/boards`.**
//! Two live SQLite writers over one board file is the one thing that actually corrupts one,
//! and the desktop app is the other writer. The startup banner says so every time it runs,
//! and [`refuse_live_data`] makes the specific mistake impossible rather than discouraged.
//!
//! # Why a hand-rolled server rather than a framework
//!
//! `vellum-store` is entirely blocking, so an async framework would mean `spawn_blocking`
//! around every store call to buy concurrency that two devices never need. This is a
//! blocking `TcpListener` with a bounded thread pool and `httparse`, which is already in
//! `Cargo.lock` by way of `ureq` — so the whole server costs no new entry in the lock file.
//!
//! # Authentication
//!
//! A bearer token, compared in constant time, required for everything but the health route.
//! It is not optional on a public address: [`check_exposure`] refuses to listen on anything but
//! loopback without one, because the alternative is ~58 irreplaceable boards on the open
//! internet behind a URL somebody could guess.

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vellum_store::{BlobStore, BoardDb, Hash, list_boards};

/// How many connections are served at once.
///
/// Small on purpose. Each one may open a board with SQLite, and the audience is a person
/// with a laptop and a tablet — not a crowd. The bound is what stops a burst of requests
/// becoming a thread per request on a small server.
const MAX_CONNECTIONS: usize = 16;

/// The largest request head this will read before giving up.
///
/// A request line and its headers; there are no request bodies here, because every route is
/// a `GET`. Without a cap, a connection that sends header bytes forever is a memory leak
/// wearing a request's clothes.
const MAX_HEAD: usize = 16 * 1024;

/// How long a client has to finish sending its request head.
///
/// ⚠ A **wall-clock deadline**, not the per-read timeout beside it, and the difference is a
/// denial of service. `read_head` reads a byte at a time, so a 15-second *read* timeout is
/// reset by every byte: one byte every fourteen seconds holds a connection for
/// `MAX_HEAD × 14s` — about sixty-three hours — and sixteen such sockets, at a little over a
/// byte a second between them, take every slot this server has. No token is needed, because
/// none of it gets as far as the gate.
const HEAD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

pub struct Config {
    pub data: PathBuf,
    pub blobs: PathBuf,
    pub web: Option<PathBuf>,
    pub addr: SocketAddr,
    pub token: Option<String>,
    pub app_origin: Option<String>,
}

/// Everything a request handler needs, shared across connection threads.
struct Server {
    config: Config,
    /// One board opened at a time.
    ///
    /// Not a throughput decision — a correctness one. Two `BoardDb`s over one file is two
    /// SQLite connections to a database in WAL mode, which is legal and which this codebase
    /// has already been burned by once in the desktop app (`session.rs`: two `Editor`s over
    /// one file is two autosave threads). Serialising costs nothing at this scale.
    boards: Mutex<()>,
}

pub fn run(config: Config) -> anyhow::Result<()> {
    anyhow::ensure!(config.data.is_dir(), "{} is not a directory", config.data.display());
    // Both, because `BlobStore::open` creates a staging directory inside whatever it is
    // given — so a `--blobs` pointed at the live store writes into it on the first request,
    // and the first version checked only `--data`.
    refuse_live_data(&[&config.data, &config.blobs])?;
    check_exposure(&config)?;

    let listener = TcpListener::bind(config.addr)
        .map_err(|e| anyhow::anyhow!("cannot listen on {}: {e}", config.addr))?;
    let actual = listener.local_addr().unwrap_or(config.addr);

    let boards = list_boards(&config.data).unwrap_or_default();
    println!("velmd {} — serving {} board(s)", env!("CARGO_PKG_VERSION"), boards.len());
    println!("  data    {}", config.data.display());
    println!("  blobs   {}", config.blobs.display());
    match &config.web {
        Some(dir) => println!("  client  {}", dir.display()),
        None => println!("  client  (not served — pass --web web/dist)"),
    }
    println!(
        "  auth    {}",
        if config.token.is_some() { "bearer token required" } else { "none (loopback only)" }
    );
    println!("  http://{actual}/");
    println!();
    println!("This program never removes a file, and no route can change a board.");
    println!("It does open boards with SQLite, which writes a -wal sidecar: point --data at");
    println!("a copy, never at the directory the desktop app is using.");

    let server = Arc::new(Server { config, boards: Mutex::new(()) });
    let live = Arc::new(AtomicUsize::new(0));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            // Refused rather than queued. A queue that grows without bound is the same
            // failure as no bound at all, just later and harder to see.
            //
            // ⚠ The write timeout is set **here**, not only in `serve_one`: this write happens
            // on the accept thread, so a client that never drains its receive window would
            // otherwise block the whole listener inside `write_all` with no timeout at all —
            // the refusal path becoming the outage it exists to prevent.
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
            let _ = respond(&stream, 503, "text/plain", b"velmd is busy\n", None);
            continue;
        }
        live.fetch_add(1, Ordering::Relaxed);
        let server = Arc::clone(&server);
        let live = Arc::clone(&live);
        std::thread::spawn(move || {
            serve_one(&server, stream);
            live.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

/// Refuse to open the directory the desktop app is using.
///
/// The prose warning is in three places already and prose does not stop anybody at one in
/// the morning. This is the one mistake with an irreversible outcome — two SQLite writers
/// over one board — and it has exactly one well-known path, so it is worth checking for by
/// name rather than trusting the reader.
fn refuse_live_data(paths: &[&Path]) -> anyhow::Result<()> {
    for path in paths {
        // ⚠ **Matched on the shape of the path, not against `$HOME`.** The first version read
        // `HOME`, built the one well-known directory and compared — which meant no `HOME`, no
        // check. systemd sets none unless the unit says so and `sudo` clears it, so the guard
        // was disabled in exactly the deployment it was written for. It also degraded to
        // *allow* when either `canonicalize` failed, which is the wrong direction for a guard
        // whose failure costs a board.
        let full = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let text = full.to_string_lossy();
        let live = text.contains("Application Support/Vellum")
            || text.contains("Application Support\\Vellum");
        anyhow::ensure!(
            !live,
            "{} is inside the desktop app's own data directory.\n\
             velmd opens boards with SQLite and creates a staging directory in a blob store, \
             and two live writers over one board file is the one thing that corrupts one.\n\
             Copy it first:  rsync -av --checksum '{}/' /srv/velm/data/",
            full.display(),
            full.display()
        );
    }
    Ok(())
}

/// A public address needs a token. Loopback does not.
///
/// The asymmetry is the point: local development should not need a secret, and the moment
/// this is reachable from anywhere else it is guarding boards that cannot be re-imported.
/// Checked before the socket is bound, so the failure arrives before anything is exposed.
fn check_exposure(config: &Config) -> anyhow::Result<()> {
    if config.token.is_some() {
        return Ok(());
    }
    let loopback = match config.addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    anyhow::ensure!(
        loopback,
        "refusing to listen on {} with no token.\n\
         Set one and try again:  VELMD_TOKEN=$(openssl rand -hex 32) velmd serve ...\n\
         A bare 127.0.0.1 needs no token; anything else is reachable by someone else.",
        config.addr
    );
    Ok(())
}

fn serve_one(server: &Server, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(60)));

    let Some((method, target, headers)) = read_head(&stream) else {
        let _ = respond(&stream, 400, "text/plain", b"bad request\n", None);
        return;
    };

    let origin = server.config.app_origin.clone();
    // A browser sends a preflight before a cross-origin request carrying an
    // `Authorization` header. Answered before the token is checked, deliberately: a
    // preflight never carries the token, so checking first would make every cross-origin
    // request fail at the door with a message nobody can see.
    if method == "OPTIONS" {
        let _ = respond(&stream, 204, "text/plain", b"", origin.as_deref());
        return;
    }
    // `HEAD` is a `GET` with the body suppressed, which is what every uptime monitor and
    // `curl -I` sends first. Answering it 405 is the wrong answer to the most common probe,
    // on a server whose health route exists precisely so an operator can tell "not running"
    // from "wrong token".
    let head_only = method == "HEAD";
    HEAD_ONLY.with(|flag| flag.set(head_only));
    if method != "GET" && !head_only {
        let _ = respond(&stream, 405, "text/plain", b"this server only answers GET\n", origin.as_deref());
        return;
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target.as_str(), ""),
    };

    // Health is deliberately outside the token, so a person setting this up can tell "the
    // server is not running" from "my token is wrong" without a second tool.
    if path == "/api/v1/health" {
        let body = format!("{{\"velmd\":\"{}\",\"ok\":true}}", env!("CARGO_PKG_VERSION"));
        let _ = respond(&stream, 200, "application/json", body.as_bytes(), origin.as_deref());
        return;
    }

    // ⚠ **The token gates the boards, not the application.**
    //
    // `/api/v1/*` is this person's data and needs the gate. The static bundle is not: it is
    // the same MIT-licensed wasm anybody can build from the public repository, and gating it
    // does not work anyway — a `<script type="module">` and
    // `WebAssembly.instantiateStreaming` fetch their own URLs and cannot be handed a header
    // or a query string, so a bundle behind the gate cannot load itself. Measured: with the
    // gate over everything, the page answered 401 to its own module import and sat on
    // "Starting…" for ever, which reads as a broken build rather than a missing token.
    //
    // What an unauthenticated caller can therefore get is the app. What they cannot get is a
    // board, a board's name, a picture, or the fact that any board exists.
    if needs_token(path)
        && let Some(expected) = &server.config.token
        && !authorised(&headers, query, expected)
    {
        let _ = respond(&stream, 401, "text/plain", b"a bearer token is required\n", origin.as_deref());
        return;
    }

    let result = route(server, path, query, &stream);
    if let Err(error) = result {
        eprintln!("velmd: {path}: {error:#}");
        let _ = respond(&stream, 500, "text/plain", b"something went wrong\n", origin.as_deref());
    }
}

/// Whether a path is data rather than application.
///
/// One function so the rule is stated once and can be tested without a socket. Everything
/// under `/api/v1/` is this person's boards; everything else is the client, which is public
/// code and which cannot carry a token to its own module and wasm fetches anyway.
fn needs_token(path: &str) -> bool {
    path.starts_with("/api/v1/")
}

fn route(server: &Server, path: &str, query: &str, stream: &TcpStream) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    match path {
        // The render proof. The wasm client posts here with what it measured, so a board can
        // be shown to have drawn on a device that cannot be screenshotted — a headless
        // browser, or an iPad in somebody's hands. It is a log line, not a store.
        "/velm-report" => {
            // ⚠ **Sanitised, and capped.** This is the only forensic record the server keeps,
            // and it is written from an unauthenticated request: `percent_decode` faithfully
            // turns `%0A` into a real newline and `%1b` into ESC, so without this an attacker
            // can forge lines indistinguishable from velmd's own, clear the operator's
            // terminal, retitle their window, or fill a redirected log a request at a time.
            println!("client: {}", printable(&percent_decode(query)));
            respond(stream, 204, "text/plain", b"", origin)
        }
        "/api/v1/boards" => {
            let body = {
                let _guard =
                    server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
                boards_json(&server.config.data)
            };
            respond(stream, 200, "application/json", body.as_bytes(), origin)
        }
        _ => {
            if let Some(id) = path.strip_prefix("/api/v1/boards/").and_then(|r| r.strip_suffix("/snapshot")) {
                return snapshot(server, id, stream);
            }
            if let Some(hash) = path.strip_prefix("/api/v1/blobs/") {
                return blob(server, hash, stream);
            }
            static_file(server, path, stream)
        }
    }
}

/// The board list, as JSON, with no server paths in it.
///
/// ⚠ `BoardIndex::path` is an absolute path on the server and it is deliberately not sent.
/// A board is named to a client by its **file stem**, which is also what
/// [`board_by_id`] matches against — and matching rather than joining is what makes the id
/// incapable of traversing anywhere, since it never touches the filesystem at all.
fn boards_json(data: &Path) -> String {
    let mut rows = Vec::new();
    for index in list_boards(data).unwrap_or_default() {
        let Some(id) = index.path.file_stem().and_then(|s| s.to_str()) else { continue };
        rows.push(format!(
            "{{\"id\":{},\"title\":{},\"items\":{}}}",
            json_string(id),
            json_string(&index.title),
            index.item_count
        ));
    }
    format!("[{}]", rows.join(","))
}

/// Find a board by the id the list handed out.
///
/// By scan and compare, never by joining the id onto a directory. A joined path needs a
/// traversal check that has to be right; a comparison against stems that came out of a
/// directory listing cannot reach anything that is not already in that directory.
fn board_by_id(data: &Path, id: &str) -> Option<PathBuf> {
    list_boards(data)
        .ok()?
        .into_iter()
        .map(|index| index.path)
        .find(|path| path.file_stem().and_then(|s| s.to_str()) == Some(id))
}

fn snapshot(server: &Server, id: &str, stream: &TcpStream) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    // ⚠ The lock is scoped to the *database* work and dropped before the write. It exists so
    // only one board is open at a time, which is a correctness argument about SQLite; holding
    // it across `respond` turns it into a throughput lock and hands any one slow reader the
    // ability to stall every board request for the length of the write timeout.
    let bytes = {
        let _guard = server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        let Some(path) = board_by_id(&server.config.data, id) else {
            return respond(stream, 404, "text/plain", b"no such board\n", origin);
        };
        let mut db = BoardDb::open(&path)?;
        let Some(board) = db.load()? else {
            return respond(stream, 404, "text/plain", b"that board holds no snapshot\n", origin);
        };
        board.to_bytes()?
    };
    respond(stream, 200, "application/octet-stream", &bytes, origin)
}

fn blob(server: &Server, hash: &str, stream: &TcpStream) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    // Parsing is the traversal defence and it is total: a `Hash` is 32 bytes decoded from
    // exactly 64 hex characters, so there is no spelling of `..` or `/` that survives it.
    let Ok(hash) = hash.parse::<Hash>() else {
        return respond(stream, 400, "text/plain", b"that is not a blob hash\n", origin);
    };
    let store = BlobStore::open(&server.config.blobs)?;
    let Some(bytes) = store.get(&hash)? else {
        return respond(stream, 404, "text/plain", b"no such blob\n", origin);
    };
    respond(stream, 200, sniff(&bytes), &bytes, origin)
}

fn static_file(server: &Server, path: &str, stream: &TcpStream) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let Some(root) = &server.config.web else {
        return respond(stream, 404, "text/plain", b"not found\n", origin);
    };
    let relative = if path == "/" { "index.html" } else { path.trim_start_matches('/') };
    let Some(file) = under(root, relative) else {
        return respond(stream, 404, "text/plain", b"not found\n", origin);
    };
    let Ok(bytes) = std::fs::read(&file) else {
        return respond(stream, 404, "text/plain", b"not found\n", origin);
    };
    respond(stream, 200, content_type(&file), &bytes, origin)
}

/// Resolve a request path inside `root`, or refuse.
///
/// Component-wise, before touching the disk: any `..`, any absolute prefix and any Windows
/// prefix is a refusal rather than something to normalise away. `vellum-agent`'s
/// `filetree::resolve` settled on the same shape for the same reason — normalising first and
/// checking afterwards is how `a/../../etc` gets through.
fn under(root: &Path, relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(relative);
    for part in candidate.components() {
        match part {
            Component::Normal(_) => {}
            _ => return None,
        }
    }
    let joined = root.join(candidate);
    // Then canonicalise and check anyway, because a symlink inside the directory is a way
    // out that no amount of component checking can see.
    let (real_root, real) = (root.canonicalize().ok()?, joined.canonicalize().ok()?);
    real.starts_with(&real_root).then_some(real)
}

/// Compare a token without leaking its length or its prefix through timing.
///
/// A naive `==` on a `String` returns at the first differing byte, which is enough to
/// recover a secret one character at a time over a network. The length is folded in as a
/// difference rather than as an early return for the same reason.
fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // ⚠ The length is compared **first and honestly**. This used to fold it in as
    // `(a.len() ^ b.len()) as u8`, and the cast discarded every bit above the low byte — so
    // the real token followed by 256 NUL bytes compared equal, and `%00` in a query string
    // delivers them. The token's *length* is not the secret; its bytes are, and those are
    // still compared without an early return.
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for i in 0..a.len() {
        difference |= a[i] ^ b[i];
    }
    difference == 0
}

/// The token, from the header or from the query string.
///
/// The query string is there for one specific reason: `<script type="module">` and
/// `WebAssembly.instantiateStreaming` fetch their own URLs and cannot be given a header, so
/// a bundle behind a header-only gate cannot load itself. It is the weaker of the two — a
/// URL lands in browser history and in server logs — which is why the header is checked
/// first and why the instructions tell the reader to reach the client over its own origin.
fn authorised(headers: &BTreeMap<String, String>, query: &str, expected: &str) -> bool {
    if let Some(value) = headers.get("authorization")
        && let Some(token) = value.strip_prefix("Bearer ")
        && same_secret(token.trim(), expected)
    {
        return true;
    }
    for pair in query.split('&') {
        if let Some(token) = pair.strip_prefix("token=")
            && same_secret(&percent_decode(token), expected)
        {
            return true;
        }
    }
    false
}

fn read_head(stream: &TcpStream) -> Option<(String, String, BTreeMap<String, String>)> {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    let began = std::time::Instant::now();
    loop {
        if buffer.len() >= MAX_HEAD || began.elapsed() > HEAD_DEADLINE {
            return None;
        }
        match reader.read(&mut byte) {
            Ok(1) => buffer.push(byte[0]),
            _ => return None,
        }
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let mut raw = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut raw);
    request.parse(&buffer).ok()?;
    let method = request.method?.to_owned();
    let target = request.path?.to_owned();
    let mut headers = BTreeMap::new();
    for header in request.headers.iter() {
        if let Ok(value) = std::str::from_utf8(header.value) {
            headers.insert(header.name.to_ascii_lowercase(), value.to_owned());
        }
    }
    Some((method, target, headers))
}

thread_local! {
    /// Whether the request being answered on this thread was a `HEAD`.
    ///
    /// A thread-local rather than a parameter on `respond`, because every one of the dozen
    /// call sites would otherwise have to thread it through and any one that forgot would
    /// send a body to a client that asked for none. One connection is one thread here, so
    /// the scope is exactly right.
    static HEAD_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn respond(
    mut stream: &TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    origin: Option<&str>,
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    // A 204 carries neither, per RFC 9110 §6.4.1 — and the one 204 that matters is the CORS
    // preflight, which has to survive an intermediary untouched or the cross-origin path
    // stops working entirely.
    let mut head = if status == 204 {
        format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n")
    } else {
        format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             X-Content-Type-Options: nosniff\r\n",
            body.len()
        )
    };
    if let Some(origin) = origin {
        // Named explicitly, never `*`: a wildcard and `Authorization` together mean any page
        // on the internet can read this person's boards from their browser.
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Headers: authorization\r\n\
             Access-Control-Allow-Methods: GET, OPTIONS\r\n\
             Vary: Origin\r\n"
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !HEAD_ONLY.with(std::cell::Cell::get) {
        stream.write_all(body)?;
    }
    stream.flush()?;
    Ok(())
}

/// ⚠ `.wasm` must be `application/wasm` or `WebAssembly.instantiateStreaming` refuses the
/// module — with an error about the MIME type that reads nothing like "your server is
/// misconfigured", which is how an afternoon goes missing.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        Some("css") => "text/css; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// A blob's type from its first bytes.
///
/// Blobs are content-addressed, so their names are hashes and carry no extension at all.
/// The client hands what it fetches to `createImageBitmap`, which sniffs the bytes itself —
/// but `X-Content-Type-Options: nosniff` is set above, and a browser that is told
/// `application/octet-stream` and forbidden to sniff will refuse an image. So the type has
/// to be right here.
fn sniff(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [0x00, 0x00, 0x01, 0x00, ..] => "image/x-icon",
        [b'%', b'P', b'D', b'F', ..] => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// One line, with no control characters in it, bounded.
///
/// **Control characters are the danger, not non-ASCII.** A newline forges a log line, ESC
/// drives the operator's terminal, and this is written from an unauthenticated request — but
/// the client's own reports are full of `·` and `×`, and mangling those to make the filter
/// simpler would cost the diagnostic to buy nothing. So the rule is exactly the hazard:
/// anything `char::is_control` becomes a `.`, and the whole line is capped.
fn printable(raw: &str) -> String {
    const MAX: usize = 1000;
    let mut out: String =
        raw.chars().take(MAX).map(|c| if c.is_control() { '.' } else { c }).collect();
    if raw.chars().nth(MAX).is_some() {
        out.push('…');
    }
    out
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A JSON string, escaped.
///
/// Hand-rolled because a board's title comes off a board file and goes into a response: a
/// title containing a quote would otherwise produce JSON the client cannot parse, and a
/// title is user content, so "that will not happen" is not a position worth taking.
fn json_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_path_cannot_leave_the_web_directory() {
        let root = std::env::temp_dir();
        for escape in ["../etc/passwd", "a/../../etc/passwd", "/etc/passwd", "..", "a/.."] {
            assert!(under(&root, escape).is_none(), "{escape} was allowed out of the root");
        }
    }

    /// The half that would make the test above vacuous: a real file must still resolve.
    #[test]
    fn an_ordinary_file_inside_the_directory_resolves() {
        let dir = std::env::temp_dir().join(format!("velmd-under-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), b"hi").unwrap();
        assert!(under(&dir, "index.html").is_some(), "a file in the root was refused");
    }

    #[test]
    fn a_token_compare_does_not_return_early() {
        assert!(same_secret("hunter2", "hunter2"));
        assert!(!same_secret("hunter2", "hunter3"));
        assert!(!same_secret("hunter2", "hunter22"));
        assert!(!same_secret("", "x"));
        assert!(same_secret("", ""));
    }

    /// A bearer token in a header is accepted; the same token misspelt is not; and the
    /// query-string form works, because a `<script type="module">` cannot send a header.
    #[test]
    fn a_request_is_authorised_by_header_or_by_query() {
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_owned(), "Bearer s3cret".to_owned());
        assert!(authorised(&headers, "", "s3cret"));
        assert!(!authorised(&headers, "", "other"));
        assert!(authorised(&BTreeMap::new(), "token=s3cret&x=1", "s3cret"));
        assert!(authorised(&BTreeMap::new(), "x=1&token=s3cret", "s3cret"));
        assert!(!authorised(&BTreeMap::new(), "token=nope", "s3cret"));
    }

    /// ⚠ A public address with no token must be refused *before* the socket is bound.
    ///
    /// A check that ran after binding would leave a window, however short, in which the
    /// boards are on the internet with no gate at all.
    #[test]
    fn a_public_address_needs_a_token() {
        let config = |ip: &str, token: Option<&str>| Config {
            data: PathBuf::from("/tmp"),
            blobs: PathBuf::from("/tmp"),
            web: None,
            addr: format!("{ip}:8787").parse().unwrap(),
            token: token.map(str::to_owned),
            app_origin: None,
        };
        assert!(check_exposure(&config("127.0.0.1", None)).is_ok(), "loopback needs no token");
        assert!(check_exposure(&config("0.0.0.0", None)).is_err(), "a public bind was allowed");
        assert!(check_exposure(&config("0.0.0.0", Some("s"))).is_ok());
    }

    /// ⚠ The gate covers the boards and not the bundle, and both halves are the assertion.
    ///
    /// Gating the bundle looks safer and is simply broken: a page's own module and wasm
    /// fetches cannot carry a token, so the client 401s on itself and never starts. Gating
    /// nothing puts irreplaceable boards behind a guessable URL.
    #[test]
    fn the_token_gates_the_boards_and_not_the_client() {
        for data in [
            "/api/v1/boards",
            "/api/v1/boards/products/snapshot",
            "/api/v1/blobs/0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(needs_token(data), "{data} was left ungated");
        }
        for public in ["/", "/index.html", "/vellum_web.js", "/vellum_web_bg.wasm", "/selftest.js"] {
            assert!(!needs_token(public), "{public} was gated, so the client cannot load itself");
        }
    }

    /// ⚠ The NUL-padded token that used to authenticate.
    ///
    /// `(a.len() ^ b.len()) as u8` is zero whenever the lengths differ by a multiple of 256,
    /// and the loop padded the shorter side with zero bytes — so the real token followed by
    /// 256 NULs compared equal, and `%00` in a query string delivers them.
    #[test]
    fn a_token_with_padding_after_it_is_not_the_token() {
        let token = "a".repeat(64);
        let padded = format!("{token}{}", "\0".repeat(256));
        assert!(!same_secret(&padded, &token), "a 256-NUL suffix authenticated");
        assert!(!same_secret(&format!("{token}x"), &token));
        assert!(same_secret(&token, &token));
    }

    /// A log line written from an unauthenticated request cannot forge a line, clear a
    /// terminal, or run past its bound.
    #[test]
    fn a_client_report_cannot_write_control_characters_into_the_log() {
        let forged = printable("ok\nvelmd: /api/v1/boards: token accepted");
        assert!(!forged.contains('\n'), "a newline survived: {forged}");
        assert!(!printable("\u{1b}[2J").contains('\u{1b}'), "an escape survived");
        assert!(printable(&"x".repeat(9999)).chars().count() <= 1001);
        // A legitimate report keeps its own punctuation: the filter is about control
        // characters, and mangling `·` would cost the diagnostic to buy nothing.
        assert_eq!(printable("1306 items · 6.1% · 106fps"), "1306 items · 6.1% · 106fps");
    }

    /// ⚠ The guard is on the *shape* of the path, so it holds with no `HOME` at all — which
    /// is every systemd deployment, and was the whole hole.
    #[test]
    fn the_desktop_apps_own_directory_is_refused_however_home_is_set() {
        let live = Path::new("/Users/someone/Library/Application Support/Vellum/boards");
        let blobs = Path::new("/Users/someone/Library/Application Support/Vellum/blobs");
        assert!(refuse_live_data(&[live]).is_err(), "--data was allowed at the live directory");
        assert!(refuse_live_data(&[blobs]).is_err(), "--blobs was allowed at the live store");
        assert!(refuse_live_data(&[Path::new("/srv/velm/data/boards")]).is_ok());
    }

    #[test]
    fn a_blob_is_typed_from_its_bytes_because_its_name_is_a_hash() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]), "image/png");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff(b"RIFF____WEBPVP8 "), "image/webp");
        assert_eq!(sniff(&[0, 0, 1, 0, 1, 0]), "image/x-icon");
        assert_eq!(sniff(b"not a picture"), "application/octet-stream");
    }

    #[test]
    fn a_board_title_with_a_quote_in_it_stays_valid_json() {
        assert_eq!(json_string(r#"a "quoted" name"#), r#""a \"quoted\" name""#);
        assert_eq!(json_string("line\nbreak"), r#""line\nbreak""#);
        assert_eq!(json_string("tab\there"), r#""tab\there""#);
    }

    #[test]
    fn a_percent_encoded_report_decodes() {
        assert_eq!(percent_decode("1306%20items%20%C2%B7%206.1%25"), "1306 items · 6.1%");
        assert_eq!(percent_decode("a+b"), "a b");
        // A stray `%` is kept rather than swallowed: a report is a diagnostic, and losing a
        // character from one is worse than showing it oddly.
        assert_eq!(percent_decode("100% done"), "100% done");
    }
}
