//! The board server.
//!
//! ⚠ **It said "the read-only board server" for as long as it was one, and kept saying it
//! through `/sync` and three more.** Four routes write now. What is still true — and is the
//! sentence the old title was reaching for — is that **none of them can destroy a board**;
//! that is stated properly under RULE ZERO below rather than compressed into a title that
//! goes stale the next time a verb is added.
//!
//! Serves three things over one origin: the wasm client, the boards it reads, and the
//! pictures on them. One origin is the whole hosting decision — it is what makes mixed
//! content, Private Network Access and CORS all stop applying at once, and it means the
//! person running this needs one domain and one certificate rather than two of each.
//!
//! # 🛑 RULE ZERO
//!
//! **Nothing here removes a file, and nothing here can destroy a board.** Four routes write,
//! and each is safe for its own reason rather than by a shared rule:
//!
//! - `POST /api/v1/boards/{id}/sync` **merges** a Loro update, so it can add and cannot
//!   remove what it did not add — and `sync.rs` takes a labelled restore point before the
//!   first change a board ever receives from the web, which makes even a merge reversible.
//! - `POST /api/v1/import` and `POST /api/v1/boards` only ever **create**, at a stem nothing
//!   is using, claimed with `create_new` — `O_CREAT | O_EXCL`, so the kernel refuses rather
//!   than this program having to remember. See `manage.rs` for why landing on an existing
//!   board would be worse than truncating one.
//! - `POST /api/v1/boards/{id}/rename` changes a **title inside a document**. No file is ever
//!   renamed: `.vellum` is what `BoardDb::open` insists on, and a board's id in this API *is*
//!   its file stem, so renaming the file would 404 every tab already open on it.
//!
//! There is no `DELETE` and no route that can remove anything, and `tests/rule_zero.rs` greps
//! this file along with the rest of the crate for any call that could unlink or move one.
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

use crate::{accounts, library_api, manage, paste, sync};
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
pub(crate) struct Server {
    pub(crate) config: Config,
    /// One board opened at a time.
    ///
    /// Not a throughput decision — a correctness one. Two `BoardDb`s over one file is two
    /// SQLite connections to a database in WAL mode, which is legal and which this codebase
    /// has already been burned by once in the desktop app (`session.rs`: two `Editor`s over
    /// one file is two autosave threads). Serialising costs nothing at this scale.
    pub(crate) boards: Mutex<()>,
    /// Who may sign in, who is signed in, and which boards each of them may see.
    ///
    /// ⚠ **Additional to `config.token`, never a replacement for it.** The bearer token is what
    /// the desktop app's sync sends and what the wasm client fetches its board with, and
    /// neither has a cookie jar — so requiring a session would break sync on the user's Mac
    /// silently, which is the one way this change could cost them work.
    pub(crate) accounts: Mutex<crate::accounts::Accounts>,
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
    // ⚠ The second half of this used to read "and no route can change a board". That stopped
    // being true the moment `/sync` landed, and a banner that overstates what a program will
    // not do is worse than no banner: it is the `locked: false` trap printed to a terminal.
    println!("This program never removes a file.");
    println!("Three routes change what is on disk and none of them can destroy a board.");
    println!("`POST /sync` only ever *merges* — a Loro update cannot remove what it did not");
    println!("add, and a labelled restore point is taken before the first change any board");
    println!("ever receives from the web. `POST /api/v1/import` and `POST /api/v1/boards`");
    println!("only ever *create*, at a name nothing was using, claimed exclusively so the");
    println!("kernel refuses rather than this program having to remember. A board's name is");
    println!("changed inside the document; no file is ever renamed.");
    println!("It does open boards with SQLite, which writes a -wal sidecar: point --data at");
    println!("a copy, never at the directory the desktop app is using.");

    let accounts = Mutex::new(crate::accounts::Accounts::open(&config.data));
    let server = Arc::new(Server { config, boards: Mutex::new(()), accounts });
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

/// Whether this request reached us through a reverse proxy.
///
/// The three headers every proxy in common use sets — `X-Forwarded-For` and `X-Real-IP` by
/// convention, `Forwarded` by RFC 7239. Presence is the whole test: the *value* is attacker
/// -influenced and is never read, so there is nothing here to spoof into a bypass.
pub(crate) fn proxied(headers: &std::collections::BTreeMap<String, String>) -> bool {
    const FORWARDING: [&str; 3] = ["x-forwarded-for", "x-real-ip", "forwarded"];
    // `read_head` lower-cases every name as it parses, so these compare directly — but
    // `eq_ignore_ascii_case` anyway, because a guard that depends on an upstream detail is
    // one that a change upstream turns off silently, which is this file's own recent lesson.
    headers.keys().any(|name| FORWARDING.iter().any(|marker| name.eq_ignore_ascii_case(marker)))
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
    // ⚠ **Shorter than [`HEAD_DEADLINE`], and the ordering is the whole guard.**
    //
    // At fifteen seconds this was *longer* than the ten-second deadline, so the deadline
    // could never bind first: a connection sending **zero bytes** sat inside one blocking
    // `read` for the full fifteen. With [`MAX_CONNECTIONS`] at 16 and no per-address limit,
    // sixteen silent sockets reconnecting every fifteen seconds take every slot — before the
    // token is checked, at no cost, from anywhere.
    //
    // It has to be *shorter* rather than merely different, and the arithmetic is worth
    // writing down because getting it wrong is subtle: `read_head` can only test the deadline
    // between reads, so a timeout of `t` against a deadline of `d` closes a silent connection
    // after `t × ceil(d / t)`. At 8s against 10s that is **16 seconds** — worse than the
    // fifteen this replaced, from a change that looks like a tightening.
    //
    // Three seconds is far past any real client's pause between the packets of one request
    // head, and 3 × 4 = 12s is the true bound on silence.
    //
    // ⚠ **`SO_RCVTIMEO` is per-read and socket-wide, so it governs the body too** — and
    // three seconds is *not* enough there. TCP's initial retransmit timeout is one second and
    // doubles, so two consecutive losses of one segment produce a gap of three seconds or
    // more, and a cellular-to-Wi-Fi handover does the same; a legitimate multi-megabyte sync
    // would fail on a lossy link. The body's read is raised at its own site, after the token
    // has been checked — which is the right place for it, because by then the connection has
    // proved it is a client rather than a silence.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(60)));

    let Some(Head { method, target, headers, leftover }) = read_head(&stream) else {
        let _ = respond(&stream, 400, "text/plain", b"bad request\n", None);
        return;
    };

    // ⚠ **A tokenless server must not answer a request that came through a proxy.**
    //
    // `check_exposure` inspects the *bind address* and lets loopback through with no token,
    // on the reasoning that local development should not need a secret. That reasoning is
    // right and the guard is incomplete, because the canonical way to serve this on the
    // internet is **exactly** a loopback bind: WebGPU needs a secure context, velmd
    // terminates no TLS, so the documented deployment is Caddy or nginx on 443 forwarding to
    // `127.0.0.1:8787`. Written the obvious way — no token, since it is "only listening on
    // localhost" — that publishes every board with no gate at all, and the startup check
    // says nothing because the address really is loopback.
    //
    // A forwarding header is the one signal that distinguishes the two. It is set by every
    // reverse proxy and it is *absent* from a genuine local request, so refusing here costs
    // nothing to the case the exemption exists for and closes the case it accidentally
    // blessed. A header can be forged — but only by someone already able to reach a loopback
    // socket, who has this person's machine and does not need to.
    if server.config.token.is_none() && proxied(&headers) {
        eprintln!(
            "velmd: refusing a forwarded request: this server has no VELMD_TOKEN set.\n\
             It is bound to a loopback address, which needs no token — but something is \
             forwarding requests to it from elsewhere, which means the boards behind it are \
             reachable with no gate at all.\n\
             Set one and restart:  VELMD_TOKEN=$(openssl rand -hex 32) velmd serve ..."
        );
        let _ = respond(
            &stream,
            403,
            "text/plain",
            b"this server is not configured to be reached from outside this machine\n",
            None,
        );
        return;
    }

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
    if method != "GET" && method != "POST" && !head_only {
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
    // ⚠ **The carve-out that makes accounts reachable at all**, and it was written, tested and
    // left with no caller — the eleventh time in this repository that a hop between a gesture
    // and its effect had none. A browser arriving at the sign-in page has no token: it is
    // there to obtain the thing that would authorise it. Without this a server with a
    // `$VELMD_TOKEN` set answers 401 to its own sign-in route and no account can ever be used,
    // on a page that looks like it is simply refusing the right password.
    //
    // Exactly two routes, and `accounts::is_exempt_from_the_bearer_gate` names why each: the
    // session route, which cannot require what it hands out, and `whoami`, whose whole job is
    // to answer *"you are nobody"*. Every board, name and picture stays behind the gate.
    //
    // ⚠ **Two ways to be authorised, and the gate fires when *either* is configured.** A
    // tokenless server that has accounts on it would otherwise have accounts deciding nothing
    // — anybody could read every board and the sign-in page would be decoration. The
    // consequence is stated rather than discovered: a desktop syncing to a tokenless server
    // starts answering 401 the moment the first account is made, so `$VELMD_TOKEN` is what
    // that machine keeps using. `accounts::configured` fails **closed** on a poisoned lock,
    // because its `false` is the answer that opens the boards.
    let gated = server.config.token.is_some() || accounts::configured(server);
    let caller = match &server.config.token {
        Some(expected) if authorised(&headers, query, expected) => Some(accounts::Caller::Token),
        _ => accounts::identity(server, &headers).map(accounts::Caller::Account),
    };
    if needs_token(path) && !accounts::is_exempt_from_the_bearer_gate(path) && gated && caller.is_none()
    {
        let _ = respond(&stream, 401, "text/plain", b"sign in, or send a bearer token\n", origin.as_deref());
        return;
    }

    // ⚠ **After the token check, before `route`.** The order is load-bearing: a 401 must cost
    // zero buffered bytes, so the body is read only once the token has passed. And the `Err`
    // arm is not optional — a POST does not go through `route`, which is where the log line
    // and the 500 live, so without it a poisoned lock is a silently dropped connection.
    // ⚠ **Signing out is a DELETE and is answered before the POST block**, because the two
    // would otherwise both want `/api/v1/session` and the first match would win by accident
    // rather than by decision. It is also the one state-changing route that carries no body.
    if method == "DELETE" && accounts::is_session(path) {
        if let Err(error) = accounts::sign_out(server, &headers, &stream) {
            eprintln!("velmd: {}: {error:#}", printable(path));
            let _ = respond(&stream, 500, "text/plain", b"something went wrong\n", origin.as_deref());
        }
        return;
    }

    if method == "POST" {
        // ⚠ **Four routes answer POST now, and the 405 below has to name all of them.** An
        // error string that mentions only sync is a false claim in the one place a person
        // reads when they are already confused about why nothing happened.
        //
        // The dispatch is an enum rather than four `if`s with four bodies, because everything
        // after it — the length cap, the timeout raise, the body read, the 500 — is the same
        // for all four and was worth writing once. What differs is exactly two things: how
        // many bytes the route will accept, and which function gets them.
        enum Post<'a> {
            Sync(&'a str),
            Import,
            Create,
            /// Capitalised, and that is not a style preference: `tests/rule_zero.rs` greps
            /// this crate for a bare `rename(` and would fire on the lowercase spelling.
            Rename(&'a str),
            SignIn,
            CreateAccount,
        }
        // ⚠ Signing out is a **DELETE**, and it is answered before the POST block below,
        // because a session that could only be ended by a POST would be one a browser's own
        // navigation could be tricked into ending. `SameSite=Strict` is what actually stops
        // that; the method is the part a reader checks first.
        let post = if let Some(id) = sync::board_id(path) {
            Some(Post::Sync(id))
        } else if paste::is_import(path) {
            Some(Post::Import)
        } else if manage::is_create(path) {
            Some(Post::Create)
        } else if accounts::is_session(path) {
            Some(Post::SignIn)
        } else if accounts::is_accounts(path) {
            Some(Post::CreateAccount)
        } else {
            manage::rename_target(path).map(Post::Rename)
        };
        let Some(post) = post else {
            let _ = respond(
                &stream,
                405,
                "text/plain",
                b"POST answers a board's sync route, the importer, and making or naming a board\n",
                origin.as_deref(),
            );
            return;
        };
        // ⚠ **A name is a kilobyte and a board is megabytes, so the cap is per route** — and
        // it is checked here, before a byte of body is read, rather than after. `manage` owns
        // its own parser for the sentences: sync's refusal says *"that sync request is too
        // large"*, and an operator who reads that after a failed **create** goes looking in
        // the wrong file.
        let length = match match post {
            // A username and a password are smaller than a board's name and much smaller
            // than a board; its own cap, so the refusal names the route the person was using.
            Post::SignIn | Post::CreateAccount => accounts::content_length(&headers),
            Post::Create | Post::Rename(_) => manage::content_length(&headers),
            Post::Sync(_) | Post::Import => sync::content_length(&headers),
        } {
            Ok(length) => length,
            Err(refusal) => {
                let _ = respond(
                    &stream,
                    refusal.status,
                    "text/plain",
                    refusal.message.as_bytes(),
                    origin.as_deref(),
                );
                return;
            }
        };
        // The head is in and the token has passed, so this is a real client rather than a
        // silence holding a slot. A body can be megabytes over a lossy link, where a three-
        // second gap between segments is two retransmits rather than a stall — see the
        // arithmetic on the head's timeout above.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        let Some(body) = sync::read_body(&stream, &leftover, length) else {
            let _ = respond(
                &stream,
                400,
                "text/plain",
                b"that request body did not arrive\n",
                origin.as_deref(),
            );
            return;
        };
        // One 500 path for all four. The `Err` arm is not optional and not shared with
        // anything: a POST never reaches `route`, which is where the log line and the 500
        // otherwise live.
        let answered = match post {
            // Decoded, because a board id is a file stem and real ones have spaces in them.
            // The same decode the snapshot route needs, and it was the sibling check that
            // found it: a board reachable for reading and not for syncing would be a board
            // that loads and then silently never updates.
            Post::Sync(id) => sync::handle(server, &percent_decode(id), &body, &stream),
            Post::Import => paste::handle(server, query, &body, caller.as_ref(), &stream),
            Post::Create => manage::create(server, &body, caller.as_ref(), &stream),
            Post::Rename(id) => {
                // ⚠ Renaming is a write to a board, so it needs the same visibility check a
                // read does — and `may_see` answering false must give the same 404 the read
                // gives, or the *difference* between the two answers tells a stranger the
                // board exists.
                let id = percent_decode(id);
                if accounts::may_see(server, &id, caller.as_ref()) {
                    manage::rename_board(server, &id, &body, &stream)
                } else {
                    respond(&stream, 404, "text/plain", b"no such board\n", origin.as_deref())
                }
            }
            Post::SignIn => accounts::sign_in(server, &headers, &body, &stream),
            Post::CreateAccount => accounts::create_account(server, &headers, &body, &stream),
        };
        if let Err(error) = answered {
            eprintln!("velmd: {}: {error:#}", printable(path));
            let _ = respond(&stream, 500, "text/plain", b"something went wrong\n", origin.as_deref());
        }
        return;
    }

    let result = route(server, path, query, &headers, caller.as_ref(), &stream);
    if let Err(error) = result {
        eprintln!("velmd: {}: {error:#}", printable(path));
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

/// ⚠ `headers` is threaded in for the account routes alone: `whoami` and the account list
/// answer from the **session cookie**, which is a header, and neither has a body to carry it.
/// Every other arm ignores it.
fn route(
    server: &Server,
    path: &str,
    query: &str,
    headers: &std::collections::BTreeMap<String, String>,
    caller: Option<&crate::accounts::Caller>,
    stream: &TcpStream,
) -> anyhow::Result<()> {
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
        // The start screen's own route: the board list plus the two things the desktop shows
        // beside it — which boards are starred and which folder each is in. A second route
        // rather than four more keys on the one above, so a client written against the older
        // shape keeps working byte for byte. See `library_api`'s header.
        // Accounts. ⚠ These three sit **outside** the bearer gate by
        // `accounts::is_exempt_from_the_bearer_gate` — a person who has not signed in yet has
        // no token and no cookie, so a sign-in route behind the gate could never be reached
        // and the server could never be set up at all.
        accounts::PATH_WHOAMI => accounts::whoami(server, headers, stream),
        accounts::PATH_ACCOUNTS => accounts::list_accounts(server, headers, stream),
        library_api::PATH => library_api::handle(server, caller, stream),
        "/api/v1/boards" => {
            let body = {
                let _guard =
                    server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
                boards_json(&server.config.data, server, caller)
            };
            respond(stream, 200, "application/json", body.as_bytes(), origin)
        }
        _ => {
            if let Some(id) = path.strip_prefix("/api/v1/boards/").and_then(|r| r.strip_suffix("/snapshot")) {
                // ⚠ **Decoded, because a board id is a file stem and real ones have spaces
                // in them.** A browser sends `encodeURIComponent(id)`, so
                // *"BMW 2020 530i g30"* arrives as `BMW%202020%20530i%20g30` and matched
                // nothing: measured, a **404 on almost every board this user owns**, while a
                // board with a one-word name worked perfectly — which is what made it look
                // like a problem with particular boards rather than with every name.
                //
                // Decoding does not weaken the traversal defence, and that is worth stating
                // because it is the reason this is safe: `board_by_id` **compares** the id
                // against the stems of a directory listing rather than joining it onto a
                // path, so a decoded `../` is a stem that does not exist rather than a way
                // out of the directory.
                // ⚠ **The visibility check, and it belongs here rather than inside
                // `snapshot`.** A 404 rather than a 403: telling a stranger that a board
                // exists but is not theirs is telling them it exists. `board_by_id` already
                // answers 404 for a name nothing holds, so the two are indistinguishable from
                // outside, which is the point.
                let id = percent_decode(id);
                if !accounts::may_see(server, &id, caller) {
                    return respond(stream, 404, "text/plain", b"no such board\n", origin);
                }
                return snapshot(server, &id, stream);
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
fn boards_json(
    data: &Path,
    server: &Server,
    caller: Option<&crate::accounts::Caller>,
) -> String {
    let mut rows = Vec::new();
    for index in list_boards(data).unwrap_or_default() {
        let Some(id) = index.path.file_stem().and_then(|s| s.to_str()) else { continue };
        // ⚠ Filtered here rather than at the card: a board this caller may not see must not
        // appear in the list at all. Its *name* and its item count are already information —
        // "a board called Payroll exists" is most of what somebody would want to know.
        if !accounts::may_see(server, id, caller) {
            continue;
        }
        // `modified` is milliseconds since the epoch, so the client formats it in the
        // reader's own locale rather than the server's. `0` for a clock the file predates,
        // which the picker renders as no date at all rather than as 1970.
        let modified = index
            .modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        rows.push(format!(
            "{{\"id\":{},\"title\":{},\"items\":{},\"modified\":{modified}}}",
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
pub(crate) fn board_by_id(data: &Path, id: &str) -> Option<PathBuf> {
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
    // ⚠ **`/` is the board picker, not the viewer.** It used to be `index.html`, which opens
    // *one* board and falls back to its own inline placeholder list when no `?board=` is
    // given — the list `chrome.js` itself calls *"wrong now that there is one"*. So the
    // ~1,200 lines of `boards.html` shipped as "the front door that did not exist" were
    // reachable only by pressing Back **inside a board you had already opened**, and the
    // address the hosting guide tells people to open landed on the placeholder.
    //
    // A front door nobody is routed to is not a front door. `index.html` is still served at
    // its own name, which is what every board link the picker builds points at.
    let relative = static_target(path);
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
pub(crate) fn same_secret(a: &str, b: &str) -> bool {
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

/// A parsed request head, and whatever the reader took past it.
struct Head {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    /// ⚠ See [`read_head`]: the body usually arrives inside the head parser's buffer.
    leftover: Vec<u8>,
}

/// The request head, and **whatever the reader took past it**.
///
/// ⚠ The fourth element is not tidiness. `BufReader` fills its 8 KB buffer from the socket
/// however few bytes the read asked for, so by the time the loop below sees `\r\n\r\n` the
/// first kilobytes of any body are already inside it — and a sync POST is small enough that
/// head and body arrive in one segment, which on loopback is every request. Dropping the
/// reader there loses them, and the body read then waits for bytes that were already
/// delivered until the timeout fires and the client gets a 400. Not an edge case: the
/// default path.
fn read_head(stream: &TcpStream) -> Option<Head> {
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
        // ⚠ **The name is kept even when the value will not decode**, and that is a guard
        // rather than tidiness. `proxied()` tests for the *presence* of a forwarding header
        // and its doc says the value "is never read, so there is nothing here to spoof" — but
        // dropping the whole entry here meant the value *was* read, by this `from_utf8`, and
        // that read decided presence. A tokenless server behind the documented proxy, sent
        // `X-Forwarded-For: \xC3\x28`, saw no forwarding header and served every board.
        //
        // An undecodable value becomes empty rather than absent: nothing in this crate reads
        // a header value except `authorization`, where an empty string fails the compare, and
        // `content-length`, where it fails to parse. Both are the safe direction.
        let value = std::str::from_utf8(header.value).unwrap_or_default();
        headers.insert(header.name.to_ascii_lowercase(), value.to_owned());
    }
    Some(Head { method, target, headers, leftover: reader.buffer().to_vec() })
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

/// [`respond`], plus response headers of the caller's own.
///
/// ⚠ **One writer, not two.** `Set-Cookie` is the only thing that needs this, and a second
/// response function for it would be a second copy of the status table, the CORS headers and
/// the framing — three things that would then drift apart silently. `respond` delegates here
/// with an empty slice, so every response in this program goes down one path.
///
/// Each entry is a whole header line without its terminator: `"Set-Cookie: velm_session=…"`.
pub(crate) fn respond_with(
    stream: &TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    origin: Option<&str>,
    extra: &[String],
) -> anyhow::Result<()> {
    respond_inner(stream, status, content_type, body, origin, extra)
}

pub(crate) fn respond(
    stream: &TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    origin: Option<&str>,
) -> anyhow::Result<()> {
    respond_inner(stream, status, content_type, body, origin, &[])
}

fn respond_inner(
    mut stream: &TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    origin: Option<&str>,
    extra: &[String],
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
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
             Access-Control-Allow-Headers: authorization, content-type\r\n\
             Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
             Vary: Origin\r\n"
        ));
    }
    // ⚠ After the CORS block and before the blank line, so a caller's header cannot displace
    // one of ours and cannot land in the body. Each entry is a whole line without its
    // terminator; `Set-Cookie` is the only thing that uses this today.
    for line in extra {
        head.push_str(line);
        head.push_str("\r\n");
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
pub(crate) fn printable(raw: &str) -> String {
    const MAX: usize = 1000;
    let mut out: String =
        raw.chars().take(MAX).map(|c| if is_unprintable(c) { '.' } else { c }).collect();
    if raw.chars().nth(MAX).is_some() {
        out.push('…');
    }
    out
}

/// Which file a request path names.
///
/// ⚠ **`/` is the board picker, not the viewer.** It used to be `index.html`, which opens
/// *one* board and falls back to its own inline placeholder list when no `?board=` is given —
/// the list `chrome.js` itself calls *"wrong now that there is one"*. So the page shipped as
/// "the front door that did not exist" was reachable only by pressing Back **inside a board
/// you had already opened**, and the address the hosting guide tells people to open landed on
/// the placeholder. A front door nobody is routed to is not a front door.
///
/// `index.html` is still served at its own name, which is what every board link the picker
/// builds points at.
fn static_target(path: &str) -> &str {
    if path == "/" { "boards.html" } else { path.trim_start_matches('/') }
}

/// Whether a character must not reach a terminal.
///
/// ⚠ **`char::is_control` is not enough, and the gap is not academic.** Rust defines it as
/// Unicode category **Cc only** — `\0..=\x1f` and `\x7f..=\x9f`. It says nothing about
/// category **Cf**, which carries U+202E RIGHT-TO-LEFT OVERRIDE and the U+2066..=U+2069
/// isolates: a log line containing one renders *reversed* from that point on, so a forged
/// suffix can be made to read as though it came first. U+200B ZERO WIDTH SPACE hides a
/// segment outright. None of them is a control character by Rust's definition and every one
/// of them defeats what this function exists to guarantee.
///
/// Category is not directly available without a Unicode table, so the ranges are named. They
/// are the ones that alter the *order or visibility* of what follows. A joiner does neither —
/// U+200C and U+200D are orthography, and a soft hyphen is inert — so those are left alone
/// rather than replaced with a dot nobody can account for.
fn is_unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            // Bidi overrides and embeddings, and the isolates that replaced them.
            // ⚠ **Not the whole `200b..200f` block.** U+200C ZWNJ and U+200D ZWJ are inside
            // it and are *orthography*: every ZWJ emoji sequence in a board name would log
            // as its pieces — `👩‍💻` as two characters and a dot — and Persian and
            // Devanagari break at every ZWNJ. The doc above this promised they were left
            // alone and the range said otherwise.
            '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            // Line and paragraph separators. Category Zl/Zp rather than Cc, so `is_control`
            // misses them — and a log consumer that splits on them sees two lines, which is
            // exactly the promise this function exists to keep.
            | '\u{2028}' | '\u{2029}'
            // The Arabic letter mark, and the deprecated tag block, which some renderers
            // still fold into the text before it.
            | '\u{061c}' | '\u{e0000}'..='\u{e007f}'
            // The byte-order mark, which some terminals treat as a directional hint.
            | '\u{feff}'
            // Interlinear annotation, which hides what follows it.
            | '\u{fff9}'..='\u{fffb}'
        )
}

pub(crate) fn percent_decode(raw: &str) -> String {
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
            // ⚠ **`+` is a literal plus.** It means a space only in
            // `application/x-www-form-urlencoded`, which is a *body* encoding — applying it
            // to a path segment means a board named `C++ notes` decodes to `C  notes` and
            // answers 404. The picker's own links are safe either way because
            // `encodeURIComponent` emits `%2B`, so this only ever bit a hand-typed or copied
            // URL: the exact case somebody hits once and cannot explain.
            //
            // The query string is decoded by this same function, and a token is hex, so
            // nothing there wants the form rule either.
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
pub(crate) fn json_string(raw: &str) -> String {
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
    /// ⚠ A board id is a **file stem**, and real ones have spaces: this user's boards are
    /// named things like *"BMW 2020 530i g30"*. A browser sends `encodeURIComponent`, so
    /// without a decode almost every board on the server answered 404 while a one-word name
    /// worked — which reads as a problem with particular boards rather than with every name.
    ///
    /// Measured before the fix: `GET /api/v1/boards/BMW%202020%20530i%20g30/snapshot` gave
    /// **404**, and the same board under a one-word stem gave **200, 931,984 bytes**.
    #[test]
    fn a_board_id_with_spaces_survives_the_trip_through_a_url() {
        assert_eq!(percent_decode("BMW%202020%20530i%20g30"), "BMW 2020 530i g30");
        // The three other shapes a real stem reaches this function in.
        assert_eq!(percent_decode("Cars%20%26%20Bikes"), "Cars & Bikes");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        assert_eq!(percent_decode("plain-name"), "plain-name");
        // ⚠ A literal plus, not a space: that rule belongs to form bodies, and applying it
        // here made a board called `C++ notes` unreachable by a hand-typed URL.
        assert_eq!(percent_decode("C++ notes"), "C++ notes");
        assert_eq!(percent_decode("C%2B%2B%20notes"), "C++ notes");
    }

    /// Decoding must not become a way out of the data directory.
    ///
    /// It cannot be, and the reason is structural rather than careful: `board_by_id`
    /// **compares** an id against the stems of a directory listing instead of joining it onto
    /// a path, so a decoded `../` is simply a stem no board has. The assertion here is that
    /// the decode is honest about what it produced — a guard that silently mangled the input
    /// would be a guard nobody could reason about.
    #[test]
    fn a_traversal_in_an_id_decodes_to_something_no_board_is_called() {
        let decoded = percent_decode("..%2F..%2Fetc%2Fpasswd");
        assert_eq!(decoded, "../../etc/passwd");
        // No file stem contains a separator, so this matches nothing in any listing.
        assert!(decoded.contains('/'), "the decode must not hide what it produced");
    }

    /// ⚠ **A tokenless loopback server must refuse a forwarded request.**
    ///
    /// The exemption exists so local development needs no secret, and the deployment the
    /// hosting guide describes — TLS terminated by a reverse proxy, velmd on `127.0.0.1` —
    /// is *also* a loopback bind. Written the obvious way, with no token because "it is only
    /// on localhost", that publishes ~58 irreplaceable boards with no gate, and the startup
    /// check stays quiet because the address really is loopback.
    ///
    /// Both halves are the assertion: a plain local request is still served, and a forwarded
    /// one is not. Testing only the second would pass on a build that had simply removed the
    /// exemption, which is a different behaviour with a different cost.
    #[test]
    fn a_tokenless_server_can_tell_a_local_request_from_a_forwarded_one() {
        let local = std::collections::BTreeMap::from([
            ("host".to_owned(), "127.0.0.1:8787".to_owned()),
            ("user-agent".to_owned(), "curl/8".to_owned()),
        ]);
        assert!(!proxied(&local), "a genuine local request must still be answered");

        for marker in ["x-forwarded-for", "x-real-ip", "forwarded", "X-Forwarded-For"] {
            let through = std::collections::BTreeMap::from([
                ("host".to_owned(), "boards.example.com".to_owned()),
                (marker.to_owned(), "203.0.113.9".to_owned()),
            ]);
            assert!(proxied(&through), "{marker} must be recognised as a proxy");
        }
    }

    /// ⚠ The root is the picker. It was `index.html`, which opens one board — so the page
    /// that lists them was reachable only from inside a board somebody had already opened,
    /// and the address the hosting guide names landed on a placeholder list that the
    /// client's own code calls obsolete.
    /// ⚠ The three characters this must never eat, and the four it must.
    ///
    /// A board name is document content and reaches a log line. Mangling a real name is a
    /// smaller harm than letting a control character through, which is why the first version
    /// took the whole `200b..200f` block — but it took the two **joiners** with it, and those
    /// are orthography: every ZWJ emoji and every Persian word breaks at one.
    #[test]
    fn a_log_line_keeps_real_writing_and_loses_what_moves_it() {
        // Survive: joiners, a soft hyphen, and letters from scripts that genuinely run right
        // to left. An Arabic name is not an attack.
        for keep in ["👩\u{200d}💻 Notes", "مرحبا", "עברית", "می\u{200c}شود", "汽车 · 2020"] {
            assert_eq!(printable(keep), keep, "{keep:?} is writing, not a control");
        }
        // Do not: a newline, an escape, a bidi *override*, and a paragraph separator.
        assert_eq!(printable("a\nb"), "a.b");
        assert_eq!(printable("a\u{1b}[2Jb"), "a.[2Jb");
        assert_eq!(printable("a\u{202e}b"), "a.b");
        assert_eq!(printable("a\u{2029}b"), "a.b");
    }

    #[test]
    fn the_root_is_the_board_list_and_not_one_board() {
        assert_eq!(static_target("/"), "boards.html");
        // Both pages keep their own names: every link the picker builds points at the second.
        assert_eq!(static_target("/boards.html"), "boards.html");
        assert_eq!(static_target("/index.html"), "index.html");
        assert_eq!(static_target("/vellum_web_bg.wasm"), "vellum_web_bg.wasm");
    }

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
        // ⚠ **This used to assert `"a b"`, and it was asserting the bug.** `+` means a space
        // only in `application/x-www-form-urlencoded`, a *body* encoding. Every caller here
        // builds its string with `encodeURIComponent`, which emits `%2B` for a plus and
        // `%20` for a space — so nothing ever wanted the form rule, and applying it made a
        // board named `C++ notes` unreachable by a hand-typed URL.
        assert_eq!(percent_decode("a+b"), "a+b");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        // A stray `%` is kept rather than swallowed: a report is a diagnostic, and losing a
        // character from one is worse than showing it oddly.
        assert_eq!(percent_decode("100% done"), "100% done");
    }
}
