//! Accounts: who may sign in, and which boards they may see.
//!
//! ```text
//! GET    /api/v1/whoami      {"username":…,"admin":…}  or 401
//! POST   /api/v1/session     {"username","password"}   -> 204 + Set-Cookie
//! DELETE /api/v1/session                               -> 204 + a cleared cookie
//! POST   /api/v1/accounts    admin only, except the very first one
//! GET    /api/v1/accounts    admin only: usernames, never hashes
//! ```
//!
//! # ⚠ The bearer token still works, unchanged, and that is not negotiable
//!
//! `$VELMD_TOKEN` is what the desktop app's sync sends and what the wasm client sends today.
//! Accounts are an **additional** way to be authorised, never a replacement: a request
//! carrying the right bearer token is authorised exactly as it was before this file existed,
//! and nothing here can refuse one. Breaking that breaks sync on the user's own Mac, which is
//! the one machine holding the originals of ~45 boards that cannot be re-imported.
//!
//! The two are checked in one place — see [`request_identity`] and the note in `serve.rs`'s
//! `authorised` — so "a token OR a session" is stated once rather than in five handlers.
//!
//! # 🛑 RULE ZERO — this file writes one file and can destroy nothing
//!
//! It owns `<data>/accounts.json` and touches nothing else in the data directory. It never
//! opens a `.vellum`, never asks SQLite for anything, and holds no path a request can steer.
//!
//! **The log is append-only.** Every change — an account created, an account removed, a
//! board's ownership recorded — is one more line on the end of the file. Nothing is ever
//! rewritten, moved, truncated or swapped, so there is no window in which the file is
//! half-anything.
//!
//! ## Why append-only rather than the usual write-a-temp-file-then-swap
//!
//! The decisive argument is **setup mode**, not durability in the abstract.
//!
//! With no accounts, this server has to serve a page that creates the first one with no
//! authentication — that is the only way a server can be set up at all. So "are there any
//! accounts?" is the question that decides whether a stranger may claim the owner account,
//! and it is answered by looking at this file. A whole-file write is `create + truncate`:
//! crash in the middle of one and the file is present and empty, which is
//! **indistinguishable from never having been set up**. The next visitor to reach `/signin`
//! is offered the owner account of a server holding ~45 irreplaceable boards.
//!
//! Appending cannot produce that state. A crash mid-append can lose at most the record being
//! written; every record before it is bytes that were already on disk and were never
//! reopened for writing. The accounts that existed a moment ago still parse, and setup stays
//! closed.
//!
//! The usual answer — write `accounts.json.tmp`, then rename it over the real file — is
//! atomic and is **not available here**: `tests/rule_zero.rs` forbids the call that performs
//! the swap, bare and qualified, with no acknowledgement escape, because a move is a delete
//! from wherever the file used to be. That rule is doing its job and this file is not a
//! reason to weaken it. Append-only is not the consolation prize: it is a better answer for
//! *this* file, for the reason above.
//!
//! ## What the file actually looks like
//!
//! One JSON object per line — JSON Lines — despite the `.json` name, which the wire contract
//! fixed. Said loudly because a person will `cat` it: `jq .` over the whole file will not
//! parse it, and `jq -c . accounts.json` per line will.
//!
//! ```text
//! {"v":1,"kind":"account","username":"owner","hash":"$argon2id$…","admin":true,"at":…}
//! {"v":1,"kind":"board","board":"planning","owner":"owner","shared":["sam"],"at":…}
//! {"v":1,"kind":"account-removed","username":"sam","at":…}
//! ```
//!
//! Replayed in order, last record wins per key. That is the whole format.
//!
//! **It grows and is never compacted**, and the arithmetic is why that is fine: a record is
//! about 250 bytes, and the events are "somebody made an account" and "somebody made a
//! board". A household with six accounts and two hundred boards writes about fifty
//! kilobytes, once. Compaction would need the swap this file cannot perform, so a `velmd`
//! that compacted would be a `velmd` that could move a file — which is exactly the capability
//! RULE ZERO removes.
//!
//! ## The torn tail, in three parts
//!
//! 1. **Setup is open if and only if the file is absent or zero bytes.** Not "holds no
//!    parseable account" — a crash during the very first append leaves a non-empty file with
//!    nothing in it that parses, and treating that as "never set up" is the hole this whole
//!    design exists to close. Any non-empty file closes setup, parseable or not.
//! 2. **A file that will not parse is said out loud and refuses sign-in.** It is not silently
//!    replaced by defaults, which is what `library_api::read_filing` correctly does for the
//!    desktop's *preferences* — the difference is that this file is the gate rather than
//!    decoration, and degrading a gate to its default means opening it.
//! 3. **Nobody is locked out by that.** The `$VELMD_TOKEN` path is untouched, and the person
//!    running the server has the disk: the log is line-oriented text and the damage is always
//!    the last line.
//!
//! And the fourth part, which is the one a reader would not guess: an append whose write was
//! torn leaves a final line with no newline on the end, so the *next* append would run onto
//! it and lose two records instead of one. [`Accounts::append`] closes the tail with a
//! newline first when the file does not end in one.
//!
//! # Never logged: a password, a hash, a session id, a bearer token
//!
//! Not at any level, not in an error, not in a `Debug`. Nothing in this file formats one, no
//! type holding one derives `Debug`, and the ones that need a `Debug` write a redaction by
//! hand — CLAUDE.md's feedback 34 found four `#[derive(Debug)]`s over secrets whose own doc
//! comments promised they were never printed, latent only because nothing formatted them yet.
//! A derive is a standing invitation for the next field.
//!
//! A **username** is logged, through [`printable`], like everything else this server writes:
//! it arrives from an unauthenticated request, and `sync.rs`'s own `short` is the record of
//! what happens when a comment argues that a control character cannot get this far.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use serde::{Deserialize, Serialize};

use crate::serve::{Server, json_string, printable, proxied, respond_with, same_secret};
use crate::sync::Refusal;

// ----- the wire ---------------------------------------------------------------------------

pub const PATH_SESSION: &str = "/api/v1/session";
pub const PATH_ACCOUNTS: &str = "/api/v1/accounts";
pub const PATH_WHOAMI: &str = "/api/v1/whoami";

/// Whether this path is the session route — `POST` to sign in, `DELETE` to sign out.
///
/// The routing rule lives here rather than in `serve.rs`, following [`crate::manage::is_create`]
/// and [`crate::sync::board_id`] exactly: it can be tested without a socket, and the one fact
/// `serve.rs` has to get right — that this path is exempt from the bearer gate, because a
/// browser signing in has no bearer token to send — is asserted in the same file that spells
/// the path.
pub fn is_session(path: &str) -> bool {
    path == PATH_SESSION
}

/// Whether this path is the accounts route — `POST` to create one, `GET` to list them.
pub fn is_accounts(path: &str) -> bool {
    path == PATH_ACCOUNTS
}

/// Whether this path is the identity route.
pub fn is_whoami(path: &str) -> bool {
    path == PATH_WHOAMI
}

/// Paths that must answer without a bearer token, whatever else the gate decides.
///
/// ⚠ **This is the carve-out that makes accounts reachable at all.** `serve.rs` gates
/// everything under `/api/v1/` behind `$VELMD_TOKEN` when one is configured, and a browser
/// arriving at the sign-in page has no token — it is trying to obtain the thing that will
/// authorise it. Without this, a server with a token set answers 401 to its own sign-in
/// route and no account can ever be used.
///
/// Deliberately **only** the two routes that cannot require what they hand out. `whoami` is
/// in the list because its whole job is to answer *"you are nobody"* with a 401 that means
/// "sign in", and a 401 that means "your token is wrong" is the same status carrying the
/// opposite instruction. Everything else — every board, every name, every picture — stays
/// behind the gate exactly as it was.
pub fn is_exempt_from_the_bearer_gate(path: &str) -> bool {
    is_session(path) || is_whoami(path)
}

/// The largest sign-in or account-creation body this will read.
///
/// ⚠ **Its own cap, refused before a byte is read**, for the reason [`crate::manage::MAX_BODY`]
/// gives at length: sharing sync's eight megabytes would let sixteen connections reserve
/// eight megabytes each to deliver a word. Four kilobytes is far past [`MAX_PASSWORD`] plus
/// [`MAX_USERNAME`] at four bytes per character with JSON's escaping around them, so nothing
/// a person could type is refused here.
pub const MAX_BODY: usize = 4 * 1024;

/// How many bytes of body the request head promises, or why it will not be read.
///
/// Its own function rather than [`crate::sync::content_length`] or [`crate::manage`]'s, for
/// the sentences — the same argument `manage.rs` makes. An operator who reads *"that sync
/// request is too large"* after a failed sign-in goes looking in the wrong file.
///
/// `Transfer-Encoding` is refused outright rather than implemented, for the reason sync gives:
/// chunked framing is a second body parser reachable from the network, and the only client is
/// a `fetch` that always sets a `Content-Length`.
pub fn content_length(headers: &BTreeMap<String, String>) -> Result<usize, Refusal> {
    if headers.contains_key("transfer-encoding") {
        return Err(Refusal {
            status: 400,
            message: "send a Content-Length; this server does not read chunked bodies\n",
        });
    }
    let Some(raw) = headers.get("content-length") else {
        return Err(Refusal { status: 400, message: "signing in needs a Content-Length\n" });
    };
    let Ok(length) = raw.trim().parse::<u64>() else {
        return Err(Refusal { status: 400, message: "that Content-Length is not a number\n" });
    };
    if length > MAX_BODY as u64 {
        return Err(Refusal { status: 413, message: "that is far too long to be a sign-in\n" });
    }
    // Infallible once the cap above has passed on every target this builds for, and written
    // as a conversion anyway: `usize` is not guaranteed to be 64 bits, and a silent
    // truncation here would read a short body and call it complete.
    let Ok(length) = usize::try_from(length) else {
        return Err(Refusal { status: 413, message: "that is far too long to be a sign-in\n" });
    };
    Ok(length)
}

// ----- statuses ---------------------------------------------------------------------------

/// Signed in, and not allowed to do this anyway.
///
/// ⚠ **`serve::respond` must learn this arm, and 409 and 429 below with it.** It maps a
/// status to its reason phrase from a fixed list of eight and an unlisted number falls
/// through to *"Internal Server Error"*, so today this would put `HTTP/1.1 403 Internal
/// Server Error` on the wire. `manage.rs` met the same wall and answered by using a status
/// the responder already spoke, which was right there because **200 was equally correct** for
/// what it was saying.
///
/// Here it is not. 400 does not mean *"you are not an admin"*, 401 means *"authenticate"* to
/// somebody who already has, and a client that branches on the code — which is every client,
/// since nobody parses a reason phrase — would be told the wrong thing. So the codes are
/// correct and `respond`'s match gains three arms. That change ships in the same hunk as
/// `respond_with`, which this file needs regardless: a cookie is a header and `respond` can
/// write no header this file can choose.
const FORBIDDEN: u16 = 403;

/// That username is taken.
const CONFLICT: u16 = 409;

/// Too many attempts. See the rate-limiting section.
const TOO_MANY: u16 = 429;

// ----- the shape of an account --------------------------------------------------------------

/// The longest username, in characters.
const MAX_USERNAME: usize = 64;

/// The shortest password, in characters.
///
/// **A length floor and nothing else — no composition rule.** NIST SP 800-63B is explicit
/// that requiring a digit and a symbol makes passwords *worse*, because it produces
/// `Password1!` and a sticky note; length is the thing that actually costs an attacker.
/// Twelve rather than eight because the only rate limit here is [`MIN_INTERVAL`] —
/// one attempt per second per username — and what is behind it is boards that cannot be
/// re-imported.
const MIN_PASSWORD: usize = 12;

/// The longest password, in characters.
///
/// Argon2's cost does not grow with the password's length, so this is not about work: it is
/// about the body cap above meaning something, and about a stored record staying a line.
const MAX_PASSWORD: usize = 256;

/// One account, as replayed from the log.
///
/// ⚠ **No `#[derive(Debug)]`, and the redaction below is written by hand.** `hash` is an
/// Argon2 PHC string: it is not a password, and it is also the entire input an offline
/// cracker needs. See the module header on feedback 34's four latent derives.
#[derive(Clone)]
struct Account {
    /// The username as stored — already folded by [`fold_username`], so it *is* the key.
    username: String,
    /// The Argon2id PHC string. Never logged, never formatted, never sent.
    hash: String,
    admin: bool,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The username is not a secret and is the only thing worth seeing here. The hash is
        // replaced by a fixed word rather than by its length, which is itself a fact about
        // the parameters somebody chose.
        f.debug_struct("Account")
            .field("username", &self.username)
            .field("hash", &"<redacted>")
            .field("admin", &self.admin)
            .finish()
    }
}

/// Who a board belongs to, and who else may see it.
#[derive(Clone, Debug, Default)]
struct Ownership {
    owner: String,
    shared: Vec<String>,
}

/// Who a request is, once a session or a token has answered for it.
///
/// ⚠ **This is a capability, not a description, and the only thing that may mint one is
/// [`identity`].** Every authorisation decision downstream — may this create an account, list
/// them, see that board — is a field read on this struct, so a hand-built `Identity { admin:
/// true, .. }` anywhere in this crate is an admin session nobody signed in for. It is
/// deliberately constructed in exactly two places in this file, both of which resolve the
/// username against the live accounts map first.
///
/// A `Debug` here is fine and useful: a username is not a secret, and the session id that
/// produced it is deliberately not carried on this type — so there is nothing to redact and
/// nothing a future field can quietly join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub username: String,
    pub admin: bool,
}

/// A live session.
///
/// ⚠ No `Debug`: the map's **key** is the session id, and a `Debug` of the map would print
/// every one of them. This type carries no secret itself and still does not get a derive,
/// because the thing that prints it is the container.
#[derive(Clone)]
struct Session {
    /// The username, not an [`Identity`].
    ///
    /// ⚠ **Deliberately not a cached `admin` flag.** An admin demoted — or removed — while
    /// their tab is open must lose the power on their next request, not when they happen to
    /// sign in again. Resolving through the accounts map on every request is a `BTreeMap`
    /// lookup, and the alternative is a stale copy of an authorisation decision.
    username: String,
    created: Instant,
    last_used: Instant,
}

// ----- sessions ---------------------------------------------------------------------------

/// The cookie's name.
const COOKIE: &str = "velm_session";

/// The same cookie, under the prefix a browser enforces.
///
/// ⚠ **`__Host-` is the only defence against a sibling subdomain fixing somebody's session.**
/// A host-only cookie and a `Domain=`-scoped one of the same name are indistinguishable to a
/// server: both arrive in one `Cookie:` header and the reader takes the first. So whoever
/// controls *any* host under the registrable domain — a blog, an abandoned CNAME, a
/// shared-hosting subdomain — can set `velm_session=<an id they signed in with>;
/// Domain=example.com`, and RFC 6265 §5.4 lets them order it first. The victim then uses the
/// site believing they are themselves, and every board they make is filed under the attacker's
/// name.
///
/// The prefix makes that impossible at the browser: `__Host-` is refused unless the cookie is
/// `Secure`, `Path=/` and has **no** `Domain`, which is precisely the shape a sibling cannot
/// forge. It requires `Secure`, so it is used only when this server knows it is on HTTPS —
/// and on plain loopback, where a sibling subdomain does not exist, the plain name is correct.
const COOKIE_HOST_PREFIXED: &str = "__Host-velm_session";

/// How many bytes of OS randomness a session id is, before hex.
///
/// 32 bytes is 256 bits, which is not a number chosen for a threat model — it is the point
/// past which the guess is not the weak link by any margin anybody will ever measure. Hex
/// rather than base64 so the value is safe in a cookie, a log line and a URL with no
/// escaping rules to get right anywhere.
const SESSION_BYTES: usize = 32;

/// How long a session survives without being used.
///
/// Twelve hours: a person who signed in on a tablet in the morning is still signed in that
/// evening, and one who put the tablet on a shelf is not signed in next week. Sliding,
/// because the alternative — a hard expiry — signs somebody out in the middle of using the
/// thing, which is the moment they are least able to see why.
const SESSION_IDLE: Duration = Duration::from_secs(12 * 60 * 60);

/// How long a session survives at all, however much it is used.
///
/// Thirty days. A sliding window alone never ends, so a device used daily would hold a
/// session for ever and a stolen one would too. This is the backstop that makes "sign in
/// again eventually" true.
const SESSION_MAX: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How many sessions are held at once.
///
/// Each sign-in mints one and nothing but expiry removes them, so without a cap a signed-in
/// account with a script is unbounded memory. Expired entries go first; if the map is still
/// full the **oldest** goes, which signs somebody out early rather than refusing to sign
/// anybody in — availability for the person who is entitled to be here beats tidiness.
const MAX_SESSIONS: usize = 256;

// ----- rate limiting -----------------------------------------------------------------------

// The three bounds on how often a password may be checked, and why each one exists.
//
// ⚠ **The threat here has two sides that pull in opposite directions**, and a limiter that
// answers only one of them makes the other worse.
//
// **Cracking.** Without a limit this is an offline cracker with a network interface. Argon2
// is slow by construction, which buys a great deal, and nothing about it stops somebody
// running attempts continuously for a month against an owner account holding ~45
// irreplaceable boards.
//
// **Exhaustion, which the obvious fix makes worse.** A password check is by far the most
// expensive thing this server can be asked to do — the default parameters are 19 MiB and
// two passes — and it is the one thing reachable without a token. `MAX_CONNECTIONS` is 16,
// so sixteen simultaneous sign-ins would be 304 MiB and every other request 503ing. **The
// connection bound is not a defence here; it is the amplifier.**
//
// **And a lockout is a denial of service against the person it protects.** Any rule of the
// form *"N failures on this username and it is shut"* hands a stranger the ability to shut
// the owner out of their own boards from anywhere, for free, for ever. So there is no
// lockout in this file. Nothing here can make a correct password stop working for longer
// than one second.
//
// The three that survive all of that are the constants below.
//
// ⚠ They were an `impl Limits` block on a unit struct, which reads well and draws
// `dead_code: struct is never constructed` — rustc reports a struct used only as a
// namespace, and CI runs clippy with warnings denied. Free constants, like every other
// constant in this file.
/// The shortest gap between two password checks **for the same username**.
///
/// One second. It is the only bound that survives a distributed attempt — many addresses,
/// one account — which is precisely what the per-address budget cannot see. And it is not
/// a lockout by any reading: the owner who mistypes their password waits one second,
/// which is shorter than the Argon2 verification they are waiting for anyway.
///
/// ⚠ **It is stamped for a username that does not exist, exactly as for one that does.**
/// Stamping only real usernames would make the limiter itself a user-enumeration oracle —
/// probe twice in a second and a 429 means *"that account is real"* — which would defeat
/// the dummy verify in [`Accounts::sign_in`] through a side channel it cannot see.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// How many **failures** one address may accumulate before it is refused outright.
///
/// Failures, not attempts, and that asymmetry is the whole reason this cannot lock
/// anybody out: somebody signing in correctly ten times a day never approaches it, and a
/// stranger's failures accrue against the stranger's own address. A success clears the
/// address's record, so nine typos followed by the right password leaves no residue.
const MAX_FAILURES: usize = 10;

/// The window those failures are counted in.
const WINDOW: Duration = Duration::from_secs(15 * 60);

/// How many addresses and usernames the limiter will remember.
///
/// Both maps are keyed by something the attacker chooses, so both need a bound or they
/// are the memory exhaustion the limiter exists to prevent. Expired entries are dropped
/// first; if a map is still full the **oldest** entry goes.
///
/// ⚠ That means an attacker with thousands of addresses can push their own record out and
/// start again — which costs nothing that was not already lost, because a per-address
/// budget is defeated by having many addresses **by definition**. The alternative, of
/// refusing keys once the map is full, would let that same attacker fill it and lock the
/// owner out. Between an attacker who was already unbounded and an owner who is newly
/// shut out, this picks the owner.
const MAX_TRACKED: usize = 4096;

/// One address's recent failures.
#[derive(Clone, Debug)]
struct Failures {
    count: usize,
    /// When the window started. The whole record expires together rather than sliding, which
    /// is cruder and cannot leak a per-attempt timing signal.
    since: Instant,
}

// ----- the store ----------------------------------------------------------------------------

/// Everything this file knows, and the only thing that writes `accounts.json`.
///
/// Lives behind a `Mutex` on [`Server`]. ⚠ **Never hold this lock and the board lock at the
/// same time.** Take one, extract a value, drop it, take the other — the rule the rest of
/// this crate already follows for the board lock, stated at `manage::create` and
/// `sync::handle`. Two locks taken in two orders is the one deadlock this server can have.
///
/// ⚠ No `#[derive(Debug)]`: it holds every hash and every live session id.
pub struct Accounts {
    /// `<data>/accounts.json`.
    log: PathBuf,
    /// Username (folded) to account.
    accounts: BTreeMap<String, Account>,
    /// The first account ever created — see [`Accounts::founder`].
    founder: Option<String>,
    /// Board id (its file stem) to who owns it.
    boards: BTreeMap<String, Ownership>,
    /// Session id to session.
    sessions: BTreeMap<String, Session>,
    /// A real Argon2id hash of a password nobody has, verified against when the username is
    /// unknown. See [`Accounts::sign_in`].
    decoy: String,
    /// Whether the log ends in a newline. See the module header's fourth part.
    tail_is_newline: bool,
    /// Whether the file was absent or zero bytes when this was built — the **only** thing
    /// that opens setup.
    setup_open: bool,
    /// Whether the file is present and did not parse. Refuses sign-in; does not open setup.
    unreadable: bool,
    failures: BTreeMap<String, Failures>,
    attempts: BTreeMap<String, Instant>,
}

impl std::fmt::Debug for Accounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Counts and paths, never contents: the maps are keyed by session id and hold hashes.
        f.debug_struct("Accounts")
            .field("log", &self.log)
            .field("accounts", &self.accounts.len())
            .field("boards", &self.boards.len())
            .field("sessions", &self.sessions.len())
            .field("setup_open", &self.setup_open)
            .field("unreadable", &self.unreadable)
            .finish()
    }
}

// ----- the log ------------------------------------------------------------------------------

/// The file's name inside `--data`.
const LOG: &str = "accounts.json";

/// The format version every record carries.
///
/// Written, and **not enforced on read**. A record from a newer build with a version this
/// one does not know is skipped rather than fatal, and unknown *keys* are ignored by serde —
/// the two halves `library_api::Filing` spells out, for the same reason: a version skew
/// between two builds must not turn into a server that refuses to start.
const VERSION: u32 = 1;

fn is_false(flag: &bool) -> bool {
    !*flag
}

/// One line of the log.
///
/// A single flat struct with a `kind` rather than a serde-tagged enum, because the reader
/// this format is really for is a person with `less`: every line has the same shape, and the
/// fields that do not apply are simply absent.
///
/// ⚠ `#[serde(default)]` on the container and no `deny_unknown_fields`, deliberately, and
/// both halves are load-bearing — `library_api::Filing` argues it at length. Unknown keys
/// being ignored is what lets a file written by a newer build parse here; the container
/// default is what lets one written by an older build parse, from before a field existed.
///
/// ⚠ No `#[derive(Debug)]`: `hash` is on it.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Record {
    v: u32,
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    hash: String,
    #[serde(skip_serializing_if = "is_false")]
    admin: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    board: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    owner: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    shared: Vec<String>,
    /// Seconds since the epoch, for a person reading the file. **Nothing reads it back.**
    ///
    /// ⚠ Seconds, and said out loud: `library_api::TrashedBoard::at` carries the same unit
    /// beside a sibling in milliseconds, and CLAUDE.md records `unix_now() * 1_000` — a clock
    /// that ticks once a second — costing an animation its whole existence. Since no decision
    /// here depends on it, a wrong unit is cosmetic, which is exactly why it is worth saying
    /// that it *is* seconds rather than leaving somebody to guess.
    at: u64,
}

const KIND_ACCOUNT: &str = "account";
const KIND_ACCOUNT_REMOVED: &str = "account-removed";
const KIND_BOARD: &str = "board";

fn now_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

impl Accounts {
    /// Read the log and build the store. Called once, from `serve::run`.
    ///
    /// Never fails: a data directory with no accounts file is the ordinary first run, and a
    /// file that will not parse is a state this has to be able to *report* rather than a
    /// reason to refuse to start — the server still has to serve the bearer-token path,
    /// which is what the desktop's sync is using while somebody repairs the file.
    pub fn open(data: &Path) -> Self {
        let log = data.join(LOG);
        let mut store = Accounts {
            log,
            accounts: BTreeMap::new(),
            founder: None,
            boards: BTreeMap::new(),
            sessions: BTreeMap::new(),
            // ⚠ **Hashed here, at construction, rather than pasted in as a constant.** The
            // decoy has to have been produced by *these* parameters or the timing it exists
            // to flatten is not flat — and a PHC string written by hand into the source is a
            // guess that fails every verify at run time while looking perfectly plausible in
            // a diff. One hash at startup, and the parameters match by construction.
            decoy: hash_password(DECOY_PASSWORD).unwrap_or_default(),
            tail_is_newline: true,
            setup_open: true,
            unreadable: false,
            failures: BTreeMap::new(),
            attempts: BTreeMap::new(),
        };
        store.replay();
        store
    }

    /// Read every line and apply it. Split out so a test can rebuild a store from a file.
    fn replay(&mut self) {
        let bytes = match std::fs::read(&self.log) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The ordinary first run. Not logged: a directory with no accounts file is
                // completely normal, and a warning printed for a normal state is a warning
                // people stop reading — `library_api::read_filing`'s rule.
                self.setup_open = true;
                self.tail_is_newline = true;
                return;
            }
            Err(error) => {
                // ⚠ **An I/O failure must not open setup.** A permissions fault, a disk that
                // will not read, a directory where a file should be — none of those is
                // evidence that nobody has set this server up, and treating them as such
                // offers the owner account to whoever asks next.
                eprintln!(
                    "velmd: {} could not be read: {} — sign-in is refused until this is fixed. \
                     The bearer token is unaffected.",
                    printable(&self.log.display().to_string()),
                    printable(&error.to_string())
                );
                self.setup_open = false;
                self.unreadable = true;
                return;
            }
        };

        // Point 1 of the module header's three: **absent or zero bytes, and nothing else.**
        self.setup_open = bytes.is_empty();
        self.tail_is_newline = bytes.last().is_none_or(|byte| *byte == b'\n');

        let text = String::from_utf8_lossy(&bytes);
        let mut damaged = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Record>(line) else {
                damaged += 1;
                continue;
            };
            if record.v != VERSION {
                // A record from a build this one does not understand. Skipped, not fatal —
                // the alternative is that a downgrade bricks the server.
                continue;
            }
            self.apply(record);
        }

        if damaged > 0 {
            // Point 2: said out loud, and it refuses sign-in. Not silently defaulted — this
            // file is the gate, and degrading a gate to its default means opening it.
            //
            // The count is safe to print; the lines are not, and are not.
            eprintln!(
                "velmd: {} has {damaged} line(s) that did not parse — sign-in is refused \
                 until this is fixed. The last line is the usual damage after a crash, and \
                 removing it by hand is the repair. The bearer token is unaffected.",
                printable(&self.log.display().to_string())
            );
            self.unreadable = true;
        }
    }

    /// Fold one record into the in-memory state. Order matters; last wins.
    fn apply(&mut self, record: Record) {
        match record.kind.as_str() {
            KIND_ACCOUNT => {
                if record.username.is_empty() || record.hash.is_empty() {
                    return;
                }
                // ⚠ The founder is the **first** account record in the file, derived rather
                // than stored. A second field naming the founder is a second source of truth
                // able to disagree with the first line of the file, which is this
                // repository's most-repeated defect: a stored flag beside a derivable fact.
                if self.founder.is_none() {
                    self.founder = Some(record.username.clone());
                }
                self.accounts.insert(
                    record.username.clone(),
                    Account {
                        username: record.username,
                        hash: record.hash,
                        admin: record.admin,
                    },
                );
            }
            KIND_ACCOUNT_REMOVED => {
                self.accounts.remove(&record.username);
                // ⚠ **Their boards are not touched, and no board record is written.**
                // Ownership reverts by *derivation* — see [`Accounts::owner_of`], which falls
                // back to the founder when the recorded owner no longer has an account. That
                // is a RULE ZERO posture as much as a tidiness one: removing an account must
                // be incapable of changing anything about a board, and the surest way to make
                // it incapable is for it to write nothing about one.
                //
                // It also means every session that account held stops resolving, because
                // [`Accounts::identity_of`] looks the username up live rather than trusting a
                // flag copied at sign-in.
                self.sessions.retain(|_, session| session.username != record.username);
            }
            KIND_BOARD => {
                if record.board.is_empty() || record.owner.is_empty() {
                    return;
                }
                self.boards.insert(
                    record.board,
                    Ownership { owner: record.owner, shared: record.shared },
                );
            }
            // An unknown kind from a newer build. Skipped for the reason `v` is.
            _ => {}
        }
    }

    /// Put one record on the end of the file, and only then change memory.
    ///
    /// ⚠ The order is the durability: memory is updated by the caller **after** this returns
    /// `Ok`, so a failed write can never leave a server believing in an account it did not
    /// record. The reverse order would create an account that exists until the process
    /// restarts, which is the worst of both — it works, so nobody investigates, and then it
    /// is gone.
    fn append(&mut self, record: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        line.push('\n');

        // RULE ZERO: this is the only write in this file, and it cannot destroy anything.
        // `append(true)` is `O_APPEND` — every write goes to the end of the file whatever the
        // offset says, so there is no seek to get wrong and no way to land on a byte that is
        // already there. `create(true)` **without** `truncate` creates the file when it is
        // absent and opens it as it is when it is not. No other file is ever opened here, and
        // this one is not a board.
        let mut file = open_for_append(&self.log)?;

        // The module header's fourth part. A previous append whose write was torn leaves a
        // final line with no newline on it; running this record onto that line would produce
        // one unparseable line and lose *two* records instead of one.
        if !self.tail_is_newline {
            file.write_all(b"\n")?;
        }
        file.write_all(line.as_bytes())?;
        // ⚠ Not left to the buffer or to `Drop`. This function returns to a handler that is
        // about to tell somebody their account exists, and a `204` for bytes still in a page
        // cache is a promise this server has not kept. `sync.rs` makes the same argument for
        // closing a database explicitly rather than dropping it.
        file.sync_all()?;
        self.tail_is_newline = true;
        Ok(())
    }
}

/// Open the log for appending, refusing to truncate and asking for private permissions.
///
/// `0o600` matters more here than anywhere else in this crate: the file is a list of Argon2
/// hashes, which is exactly the input an offline cracker wants, and a world-readable one on a
/// shared box hands it over. The mode applies at creation only — an existing file's
/// permissions are the operator's, and quietly changing them would be this program having an
/// opinion about a file somebody may have deliberately set up.
///
/// RULE ZERO: the only file this ever opens is `<data>/accounts.json`, which is never a
/// board — `path` comes from [`Accounts::log`], which is `data.join(LOG)` with a constant
/// name, so no request can steer it. `create` **without** `truncate`, plus `append`, means
/// this can add bytes to the end of that one file and can destroy nothing, here or anywhere.
fn open_for_append(path: &Path) -> std::io::Result<std::fs::File> {
    // RULE ZERO: `create(true).append(true)` is `O_CREAT | O_APPEND` and there is deliberately
    // no `.truncate(true)` and no `.write(true)` beside it. Every write lands at the end of
    // the file whatever the offset says, so there is no seek to get wrong and no way to land
    // on a byte that is already there.
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        // RULE ZERO: permissions only. `OpenOptionsExt::mode` applies at creation and cannot
        // truncate, move or remove anything; it is named here because the scan matches on
        // `OpenOptions` and every match has to say what it does.
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

// ----- passwords ------------------------------------------------------------------------------

/// The password the decoy hash is made from.
///
/// Not a secret and not required to be one: its only job is to be *a* valid password so that
/// [`hash_password`] produces a real PHC string with the real parameters, which
/// [`Accounts::sign_in`] can then verify against when the username does not exist. Verifying
/// against it always fails — it is never any account's password, because an account's
/// password is hashed with its own salt and this string is never offered as one.
const DECOY_PASSWORD: &str = "velmd has no account by that name";

/// Hash a password into a PHC string.
///
/// ⚠ **Through the [`PasswordHasher`] trait, never the raw KDF**, and this is the decision
/// most likely to be second-guessed as ceremony. The trait's output is a PHC string —
/// `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>` — which carries **the algorithm, the
/// version, the parameters and the salt inside the value itself.** That is what lets the cost
/// be raised later: a verify reads the parameters out of the stored string, so an account
/// hashed at today's cost keeps working after the cost is raised, and only new and re-set
/// passwords get the new one. A bare 32-byte digest plus parameters written as constants in
/// this file cannot be re-tuned without invalidating every password already set — which, on a
/// server whose owner account is the only way back to ~45 irreplaceable boards, means the
/// tuning never happens.
///
/// [`Argon2::default`] is **Argon2id**, v0x13, `m=19456 KiB, t=2, p=1` — the parameters
/// RFC 9106 recommends for the memory-constrained case, and the ones the crate's authors
/// chose as the default so that this decision is not made by whoever wrote this line. Argon2
/// **id** rather than i or d because it is the hybrid: side-channel resistance on the first
/// pass, GPU resistance on the rest.
///
/// The salt is 16 bytes from [`OsRng`] — the operating system's own generator, `getrandom`
/// underneath — because a salt from a fast PRNG seeded from the clock is a salt an attacker
/// can enumerate, which turns per-account hashes back into one rainbow table.
fn hash_password(password: &str) -> Option<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(password.as_bytes(), &salt).ok().map(|hash| hash.to_string())
}

/// Whether this password produces that stored hash.
///
/// ⚠ **Constant-time, and it is worth naming exactly which part is.** The comparison of the
/// derived bytes against the stored ones is constant-time *by construction* inside
/// `password_hash` — it uses `subtle`, not `==`. That is the part this function does not have
/// to get right, and `serve::same_secret` is deliberately **not** used here: it is the right
/// tool for a session id and the wrong one for a PHC string, which is not compared as a
/// string at all.
///
/// What is **not** constant-time, and is handled at the call site rather than here, is
/// whether the username existed at all. See [`Accounts::sign_in`].
fn verify_password(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        // A stored hash that will not parse. Refused rather than treated as "no password
        // set", which is the reading that would let it in.
        return false;
    };
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}

/// A username reduced to the one spelling this file stores and compares.
///
/// **Folded to lowercase, and ASCII only.** Two accounts differing only in case is a
/// phishing surface inside a household — `owner` and `Owner` reading as one person on a
/// board list — and a person who typed `Owner` when they signed up should be able to sign in
/// as `owner` a month later. Restricting to ASCII settles Unicode confusables outright
/// rather than by a normalisation table this file would have to keep current: `оwner` with a
/// Cyrillic о is a different account that looks identical in every font.
///
/// Returns the reason it is refused, so the client can be told which rule it broke rather
/// than *"invalid"*.
fn fold_username(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("a username cannot be empty\n");
    }
    if trimmed.chars().count() > MAX_USERNAME {
        return Err("that username is too long\n");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err("a username may hold only letters, digits, and . - _\n");
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Whether a password is long enough to be one. See [`MIN_PASSWORD`].
fn check_password(raw: &str) -> Result<(), &'static str> {
    let length = raw.chars().count();
    if length < MIN_PASSWORD {
        // The number is in the sentence: "too short" without it is a guessing game.
        return Err("a password needs at least 12 characters\n");
    }
    if length > MAX_PASSWORD {
        return Err("that password is too long\n");
    }
    Ok(())
}

// ----- signing in -------------------------------------------------------------------------

/// What a sign-in attempt decided, before anything is written to the socket.
///
/// The same shape as `manage::Outcome` and `sync::Outcome`, and for the same reason: every
/// exit from the lock is a value rather than a written reply, so no path can hold the
/// accounts lock across a socket write and let one slow reader stall every other account
/// request for the length of the write timeout.
///
/// ⚠ No `#[derive(Debug)]`: [`Outcome::SignedIn`] carries a live session id.
enum Outcome {
    SignedIn { id: String, identity: Identity },
    Wrong,
    Throttled,
    Refused(&'static str),
    Broken,
}

/// Where a request came from, as far as the limiter and the cookie are concerned.
///
/// Built by the handler from the socket and the headers, so [`Accounts`] itself needs no
/// networking and can be tested without one.
pub struct Wire {
    /// The peer address, or `None` when it could not be read or the request was proxied.
    ///
    /// ⚠ **`None` when proxied, and that is a decision rather than a limitation.** Behind a
    /// reverse proxy — which is the documented internet deployment — every request has the
    /// *same* peer address, so a per-address failure budget becomes a global one and ten
    /// failures from a stranger lock out the household. `X-Forwarded-For` carries the real
    /// client and is written by the client, so trusting it lets an attacker rotate a header
    /// for unlimited budget while *also* letting them spend somebody else's.
    ///
    /// Neither is worth having, so behind a proxy the address budget simply does not apply
    /// and the bounds that remain are the per-username interval and the fact that only one
    /// password is ever verified at a time. **An internet deployment should rate-limit
    /// `/api/v1/session` at the proxy**, which is the layer that actually knows who the
    /// client is; that belongs in the hosting instructions and is named here so it is not
    /// discovered later.
    address: Option<String>,
    /// Whether the cookie must carry `Secure`.
    secure: bool,
}

impl Accounts {
    /// Whether this server has never been set up.
    ///
    /// **Absent file or zero bytes, and nothing else** — the module header's first point.
    pub fn setup_open(&self) -> bool {
        self.setup_open && self.accounts.is_empty()
    }

    /// Whether any account exists. `serve.rs` reads this to decide whether the gate applies.
    pub fn any(&self) -> bool {
        // ⚠ **Three states, not one, and the two extra ones are why this is not
        // `!self.accounts.is_empty()`.** That expression answers *how many records parsed*,
        // which is a different question from *does this server have accounts* — and the gate
        // reads this, so the difference is the gate turning itself off.
        //
        // A file that could not be read, one whose every line was damaged, and — the sharpest
        // — one written by a **newer** velmd, where `replay` skips each record on a version
        // mismatch without counting it as damage, all leave `accounts` empty. On a tokenless
        // server that answers "no accounts", which stops the gating, which serves every board
        // to anybody, silently and with nothing printed.
        //
        // So: yes if any record parsed, yes if the file was unreadable, and yes if setup has
        // been closed — because setup closes when an account is made and never reopens, so a
        // closed setup is proof an account existed even when none is loaded now.
        !self.accounts.is_empty() || self.unreadable || !self.setup_open()
    }

    /// The founding account — the first one ever created here.
    ///
    /// Every board with no ownership record belongs to them; see [`Accounts::may_see`].
    pub fn founder(&self) -> Option<&str> {
        self.founder.as_deref()
    }

    /// Check a password and mint a session.
    ///
    /// ⚠ **The whole verification happens under the caller's lock, deliberately**, which
    /// means at most one Argon2 hash is ever in flight in this process. That is the bound on
    /// the exhaustion half above, and it is a stronger one than a semaphore would be
    /// because there is no second concurrency primitive to get right: 19 MiB, once, whatever
    /// arrives. The cost is that a sign-in briefly serialises other account operations —
    /// about a tenth of a second, on a server whose whole audience is a household — and the
    /// bound on the queue is `MAX_CONNECTIONS`, so the worst case is sixteen attempts taking
    /// a second and a half between them.
    ///
    /// ⚠ **A dummy verify runs when the username is unknown**, against [`Accounts::decoy`].
    /// Returning early there would make the *response time* an oracle: a fast refusal means
    /// no such account, a slow one means the account exists and the password was wrong — so
    /// an attacker learns every username on the server without guessing a single password,
    /// and then spends their whole budget on names that exist. The decoy is a real hash with
    /// the real parameters, so the two paths cost the same work by construction rather than
    /// by a sleep somebody tuned once.
    fn sign_in(&mut self, raw_username: &str, password: &str, wire: &Wire) -> Outcome {
        if self.unreadable {
            return Outcome::Broken;
        }
        // Folding failures are refused *before* the limiter is touched: an empty or
        // over-long username is a client bug rather than a guess, and stamping it would let a
        // broken client throttle a real account whose name it is mangling.
        let Ok(username) = fold_username(raw_username) else {
            return Outcome::Refused("that is not a username on this server\n");
        };

        let now = Instant::now();

        // ⚠ **The address gate runs first, and only an attempt that passes it stamps the
        // username.** The other order is a lockout by the back door: an attacker whose
        // address budget is exhausted would go on refreshing the owner's one-second interval
        // for ever, and the owner — whose own address is fine — would never find a gap.
        if let Some(address) = &wire.address
            && self.address_is_over_budget(address, now)
        {
            return Outcome::Throttled;
        }

        if let Some(last) = self.attempts.get(&username)
            && now.duration_since(*last) < MIN_INTERVAL
        {
            // ⚠ **Refused without re-stamping.** Stamping on the refusal would let one
            // attempt per second hold the interval open permanently, which is the lockout
            // this file promises not to have.
            return Outcome::Throttled;
        }
        // Stamped for a username that does not exist exactly as for one that does — see
        // `MIN_INTERVAL`, where the enumeration oracle this closes is written out.
        self.stamp_attempt(username.clone(), now);

        let stored = match self.accounts.get(&username) {
            Some(account) => account.hash.clone(),
            None => self.decoy.clone(),
        };
        let correct = verify_password(password, &stored);
        // Membership is re-read rather than remembered from the branch above, so that a
        // decoy verify which somehow returned true still cannot sign anybody in.
        let account = self.accounts.get(&username).cloned();

        let Some(account) = account.filter(|_| correct) else {
            if let Some(address) = &wire.address {
                self.record_failure(address.clone(), now);
            }
            // ⚠ **One sentence for both wrong-password and no-such-account.** The timing is
            // flattened above; saying *"no such user"* in the body would hand back through
            // the response what the dummy verify just spent a tenth of a second hiding.
            return Outcome::Wrong;
        };

        // A success clears this address's record, so nine typos followed by the right
        // password leave no residue for the next person on the same connection.
        if let Some(address) = &wire.address {
            self.failures.remove(address);
        }

        let id = new_session_id();
        self.evict_sessions(now);
        self.sessions.insert(id.clone(), Session {
            username: account.username.clone(),
            created: now,
            last_used: now,
        });
        Outcome::SignedIn {
            id,
            identity: Identity { username: account.username, admin: account.admin },
        }
    }

    /// Whether this address has failed too often lately, dropping the record if it has aged out.
    fn address_is_over_budget(&mut self, address: &str, now: Instant) -> bool {
        match self.failures.get(address) {
            Some(record) if now.duration_since(record.since) >= WINDOW => {
                self.failures.remove(address);
                false
            }
            Some(record) => record.count >= MAX_FAILURES,
            None => false,
        }
    }

    fn record_failure(&mut self, address: String, now: Instant) {
        let entry = self.failures.entry(address).or_insert(Failures { count: 0, since: now });
        if now.duration_since(entry.since) >= WINDOW {
            *entry = Failures { count: 0, since: now };
        }
        entry.count += 1;
        Self::bound(&mut self.failures, |record| record.since, MAX_TRACKED);
    }

    fn stamp_attempt(&mut self, username: String, now: Instant) {
        self.attempts.insert(username, now);
        Self::bound(&mut self.attempts, |at| *at, MAX_TRACKED);
    }

    /// Hold a limiter map to [`MAX_TRACKED`] by dropping its oldest entries.
    ///
    /// See that constant for why *oldest* rather than *refuse new*: refusing would let an
    /// attacker fill the map and shut the owner out, which trades an attacker who was already
    /// unbounded for an owner who is newly locked out.
    fn bound<V>(map: &mut BTreeMap<String, V>, age: impl Fn(&V) -> Instant, limit: usize) {
        if map.len() <= limit {
            return;
        }
        let mut ages: Vec<(Instant, String)> =
            map.iter().map(|(key, value)| (age(value), key.clone())).collect();
        ages.sort_by_key(|(at, _)| *at);
        for (_, key) in ages.into_iter().take(map.len() - limit) {
            map.remove(&key);
        }
    }

    /// Drop expired sessions, then the oldest, until there is room for one more.
    fn evict_sessions(&mut self, now: Instant) {
        self.sessions.retain(|_, session| !expired(session, now));
        while self.sessions.len() >= MAX_SESSIONS {
            let oldest = self
                .sessions
                .iter()
                .min_by_key(|(_, session)| session.created)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else { break };
            self.sessions.remove(&oldest);
        }
    }

    /// Who this session id is, refreshing its idle clock — or nobody.
    ///
    /// ⚠ **Compared with [`same_secret`], not with `BTreeMap::get`.** A map lookup on a
    /// secret compares keys byte by byte with an early return, and this one is presented by
    /// the client on every request: that is the timing oracle `same_secret`'s own doc
    /// describes, spelled with a container instead of a `==`. The scan is over at most
    /// [`MAX_SESSIONS`] entries on a server whose audience is a household, so the cost is
    /// nothing and the property is the house's existing paranoia applied where it belongs.
    fn identity_of(&mut self, presented: &str, now: Instant) -> Option<Identity> {
        let matched = self.holder_of(presented)?;
        // ⚠ Read, decide, *then* take the mutable borrow. Testing expiry through a
        // `get_mut` and removing inside that `if` is a borrow of the map held across a
        // second use of it, which does not compile — and the version that does compile is
        // usually the one that forgets to drop the expired entry.
        if expired(self.sessions.get(&matched)?, now) {
            self.sessions.remove(&matched);
            return None;
        }
        let session = self.sessions.get_mut(&matched)?;
        session.last_used = now;
        let username = session.username.clone();
        // Resolved live, never from a flag copied at sign-in: an account removed or demoted
        // while its tab is open loses the power on its next request.
        let account = self.accounts.get(&username)?;
        Some(Identity { username: account.username.clone(), admin: account.admin })
    }

    fn sign_out(&mut self, presented: &str) {
        if let Some(id) = self.holder_of(presented) {
            self.sessions.remove(&id);
        }
    }

    /// The stored session id equal to the presented one, compared in constant time.
    ///
    /// One function so the scan is written once: two copies of a constant-time compare is
    /// two places for the next person to reach for `BTreeMap::get` in.
    fn holder_of(&self, presented: &str) -> Option<String> {
        self.sessions.keys().find(|held| same_secret(held.as_str(), presented)).cloned()
    }
}

/// Whether a session has run out either way. See [`SESSION_IDLE`] and [`SESSION_MAX`].
///
/// ⚠ **`Instant`, not `SystemTime`.** Sessions live in memory and die with the process, so
/// there is nothing for a wall clock to be right about — and a wall clock that jumps (NTP
/// stepping a server that has just booted, a laptop waking in another timezone) would sign
/// everybody out for a reason nobody could ever reconstruct. `Instant` is monotonic and
/// cannot.
fn expired(session: &Session, now: Instant) -> bool {
    now.duration_since(session.last_used) >= SESSION_IDLE
        || now.duration_since(session.created) >= SESSION_MAX
}

/// A fresh session id: [`SESSION_BYTES`] from the operating system, in hex.
fn new_session_id() -> String {
    let mut bytes = [0u8; SESSION_BYTES];
    // ⚠ `OsRng` and nothing else. It is `getrandom` underneath — `getentropy` on macOS,
    // `getrandom(2)` on Linux — which is the kernel's own pool. Any userspace PRNG here is a
    // seed somebody has to have got right, and a session id is a bearer credential: guessing
    // one is being signed in as its owner.
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(SESSION_BYTES * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ----- accounts, and who may make one --------------------------------------------------------

/// What an account change decided, before anything is written to the socket.
///
/// ⚠ No `#[derive(Debug)]`: nothing here carries a secret today, and a derive is what lets
/// the next field carry one silently.
enum Change {
    Made(Identity),
    Gone,
    Taken,
    NotAllowed,
    Refused(&'static str),
    Broken,
}

impl Accounts {
    /// Make an account.
    ///
    /// # ⚠ The first one is unauthenticated, and it stops being so under the same lock
    ///
    /// A server with no accounts has to let *somebody* make the first one or it can never be
    /// set up at all: there is no authority to check against, because creating the authority
    /// is the request. So while [`Accounts::setup_open`] is true this is open, and the account
    /// it makes is the **owner** — `admin` is forced true rather than read from the body, so
    /// a setup request cannot produce a server whose only account cannot manage accounts.
    ///
    /// **The race is closed by the lock, not by a check-then-act.** Two setup requests
    /// arriving together would both see "no accounts" if the read and the write were separate
    /// critical sections — and the second one would make a *second* owner. Here the caller
    /// holds the accounts lock across the whole of this function, so the second request
    /// evaluates `setup_open()` **after** the first has appended and inserted: the map is
    /// non-empty, setup is shut, and the second request is refused for want of an admin
    /// session. There is no window because there is no gap between the test and the write.
    ///
    /// Once one account exists this needs an admin, for ever. The only way back to an open
    /// setup is an empty `accounts.json`, which nothing in this program can produce.
    fn create_account(
        &mut self,
        raw_username: &str,
        password: &str,
        admin: bool,
        by: Option<&Identity>,
    ) -> Change {
        if self.unreadable {
            return Change::Broken;
        }
        let first = self.setup_open();
        if !first && !by.is_some_and(|caller| caller.admin) {
            return Change::NotAllowed;
        }
        let username = match fold_username(raw_username) {
            Ok(username) => username,
            Err(reason) => return Change::Refused(reason),
        };
        if let Err(reason) = check_password(password) {
            return Change::Refused(reason);
        }
        if self.accounts.contains_key(&username) {
            return Change::Taken;
        }
        let Some(hash) = hash_password(password) else {
            return Change::Broken;
        };
        // The first account is always an admin whatever the body asked for; every later one
        // is what the admin making it asked for.
        let admin = first || admin;

        let record = Record {
            v: VERSION,
            kind: KIND_ACCOUNT.to_owned(),
            username: username.clone(),
            hash: hash.clone(),
            admin,
            at: now_seconds(),
            ..Record::default()
        };
        if let Err(error) = self.append(&record) {
            // The path and the reason, never the record: it holds the hash.
            eprintln!(
                "velmd: could not record an account in {}: {}",
                printable(&self.log.display().to_string()),
                printable(&error.to_string())
            );
            return Change::Broken;
        }
        // Only after the bytes are on disk — see [`Accounts::append`] on why this order is
        // the durability rather than a style.
        if self.founder.is_none() {
            self.founder = Some(username.clone());
        }
        self.setup_open = false;
        self.accounts
            .insert(username.clone(), Account { username: username.clone(), hash, admin });
        Change::Made(Identity { username, admin })
    }

    /// Remove an account. Boards it owned revert to the founder.
    ///
    /// ⚠ **No route reaches this yet, and that is stated rather than left to be discovered.**
    /// The wire contract this file was written against has no `DELETE /api/v1/accounts/{name}`,
    /// so the verb exists, is tested, and has no caller — which is this repository's signature
    /// defect, recorded nine times in CLAUDE.md. It is written now because the *rules* around
    /// removal are the part worth getting right while the reasoning is in hand, and because
    /// `serve.rs` adding one arm is a smaller change than discovering these three rules later:
    ///
    /// - **Removing an account never removes a board.** Nothing here writes a board record;
    ///   ownership reverts by derivation in [`Accounts::may_see`]. RULE ZERO, and it is
    ///   structural: this function is *incapable* of changing anything about a board.
    /// - **The founder cannot be removed.** Every board with no ownership record falls back to
    ///   them, so removing them would orphan ~45 boards in one request.
    /// - **The last admin cannot be removed.** A server with accounts and no admin can never
    ///   make another one — setup does not reopen — so that request is a one-way door out of
    ///   account management.
    // ⚠ The `allow` is the reachability gap, deliberately left visible rather than hidden by
    // inventing a route this file was not given. Delete it in the same edit that adds one.
    #[allow(dead_code)]
    fn remove_account(&mut self, raw_username: &str, by: &Identity) -> Change {
        if self.unreadable {
            return Change::Broken;
        }
        if !by.admin {
            return Change::NotAllowed;
        }
        let Ok(username) = fold_username(raw_username) else {
            return Change::Refused("that is not a username on this server\n");
        };
        let Some(account) = self.accounts.get(&username).cloned() else {
            return Change::Gone;
        };
        if self.founder.as_deref() == Some(username.as_str()) {
            return Change::Refused(
                "the first account cannot be removed: every board with no owner recorded \
                 belongs to it\n",
            );
        }
        if account.admin && self.accounts.values().filter(|a| a.admin).count() <= 1 {
            return Change::Refused(
                "that is the only admin left; another one has to exist first\n",
            );
        }
        let record = Record {
            v: VERSION,
            kind: KIND_ACCOUNT_REMOVED.to_owned(),
            username: username.clone(),
            at: now_seconds(),
            ..Record::default()
        };
        if let Err(error) = self.append(&record) {
            eprintln!(
                "velmd: could not record an account change in {}: {}",
                printable(&self.log.display().to_string()),
                printable(&error.to_string())
            );
            return Change::Broken;
        }
        self.accounts.remove(&username);
        self.sessions.retain(|_, session| session.username != username);
        Change::Gone
    }
}

// ----- boards -------------------------------------------------------------------------------

/// Who is asking.
///
/// ⚠ **The bearer token is its own kind of caller and it sees everything.** It is the
/// server-wide secret — the one the desktop app's sync sends, and the one that existed before
/// accounts did — so narrowing it to a particular account's boards would break sync on the
/// user's own Mac the day accounts are created. Accounts are a way to give *other people*
/// less than the token has, never a way to give the token less than it had.
#[derive(Clone, Debug)]
pub enum Caller {
    /// Presented the right `$VELMD_TOKEN`.
    Token,
    /// Presented a valid session cookie.
    Account(Identity),
}

impl Accounts {
    /// Who owns a board, applying the fallback.
    ///
    /// # The rule for a board with no ownership record
    ///
    /// **It belongs to the founder** — the first account ever made here. That covers three
    /// cases with one sentence, which is why it is a fallback rather than an enumeration at
    /// setup time:
    ///
    /// - the ~45 boards already in the data directory when accounts were switched on,
    /// - a board dropped into `--data` by hand or by `velmd import` afterwards,
    /// - a board whose recorded owner has since been removed.
    ///
    /// The alternative — walking the directory at setup and writing a record per board — is
    /// worse in every direction: it needs a directory listing inside the one request that
    /// must not fail, it is wrong the moment a board arrives afterwards, and a board that
    /// appeared between the listing and the write would be owned by nobody. A rule that
    /// answers for boards that do not exist yet cannot go stale.
    fn owner_of(&self, board: &str) -> Option<&str> {
        match self.boards.get(board) {
            // A recorded owner whose account is gone falls through to the founder, which is
            // what makes removing an account incapable of hiding a board.
            //
            // `as_str()` rather than `&record.owner`: a coercion from `&String` to `&str`
            // does not travel through `Option`, so the borrowed form has to be spelled.
            Some(record) if self.accounts.contains_key(&record.owner) => Some(record.owner.as_str()),
            _ => self.founder(),
        }
    }

    /// Whether this caller may see this board.
    ///
    /// ⚠ **An admin is not privileged here, deliberately.** `admin` is the power to create and
    /// remove *accounts*; the contract says a board is visible to its owner, to anyone it is
    /// shared with, and to nobody else, and quietly adding "or any admin" would mean the one
    /// account that can make other accounts can also read every board on the server without
    /// that ever having been agreed. If an admin needs to read somebody's board, the owner
    /// shares it — which leaves a record.
    fn allowed(&self, board: &str, caller: &Caller) -> bool {
        let Caller::Account(identity) = caller else {
            // `Caller::Token` — see the type's own note.
            return true;
        };
        // With no accounts at all there is nothing to enforce and no founder to fall back to;
        // this cannot be reached through a session, since a session requires an account.
        if self.accounts.is_empty() {
            return true;
        }
        if self.owner_of(board) == Some(identity.username.as_str()) {
            return true;
        }
        self.boards
            .get(board)
            .is_some_and(|record| record.shared.contains(&identity.username))
    }

    /// Record who a newly created board belongs to.
    ///
    /// Called after `POST /api/v1/boards` and `POST /api/v1/import` have made one. A board
    /// with no record still resolves — to the founder — so a missed call here is a board that
    /// belongs to the owner rather than a board nobody can open.
    fn claim(&mut self, board: &str, owner: &str) -> std::io::Result<()> {
        let record = Record {
            v: VERSION,
            kind: KIND_BOARD.to_owned(),
            board: board.to_owned(),
            owner: owner.to_owned(),
            at: now_seconds(),
            ..Record::default()
        };
        self.append(&record)?;
        self.boards.insert(
            board.to_owned(),
            Ownership { owner: owner.to_owned(), shared: Vec::new() },
        );
        Ok(())
    }
}

// ----- what `serve.rs` calls ------------------------------------------------------------------

/// Who this request is, from its session cookie — or nobody.
///
/// ⚠ **This is the function `serve::authorised` calls to ask "does this carry a valid
/// session?".** The two ways to be authorised are checked in one place so the rule is stated
/// once: a valid bearer token **or** a valid session. A request carrying the right token is
/// authorised exactly as it was before this file existed, whatever this answers.
///
/// It takes the accounts lock for the length of a map scan and drops it before returning.
/// ⚠ Never call it while holding `Server::boards`.
pub fn identity(server: &Server, headers: &BTreeMap<String, String>) -> Option<Identity> {
    let presented = session_cookie(headers)?;
    let mut accounts = server.accounts.lock().ok()?;
    accounts.identity_of(&presented, Instant::now())
}

/// Whether this server has any accounts at all.
///
/// ⚠ `serve.rs` needs this for the gate: today the gate fires only when `$VELMD_TOKEN` is
/// configured, so a tokenless server with accounts on it would have accounts that decide
/// nothing. See the report accompanying this file — the rule is *gate when a token is set
/// **or** any account exists*, and the consequence has to be chosen knowingly, because a
/// tokenless desktop syncing to that server starts answering 401 the moment the first account
/// is made.
/// Whether this server still has no accounts, so a client can offer *set up* over *sign in*.
///
/// ⚠ Fails **closed** in the direction that matters here, which is the opposite of
/// [`configured`]'s: a poisoned lock answers `false`, meaning "already set up", because a
/// wrong `true` would invite a stranger to try to found a server that is already somebody's.
pub fn setup_is_open(server: &Server) -> bool {
    server.accounts.lock().is_ok_and(|accounts| accounts.setup_open())
}

pub fn configured(server: &Server) -> bool {
    // ⚠ **A poisoned lock answers `true` here, and the direction is the whole point.** This
    // is the only function in this file whose `false` *opens* something: under the gate rule
    // above, a tokenless server that answers "no accounts" stops gating and serves every
    // board. `is_ok_and` would answer `false` on poison, so a panic in any handler would
    // unlock the boards — which is feedback 45's `refuse_live_data` and feedback 48's restore
    // point, twice recorded: **a guard whose failure mode is permit is not a guard.**
    //
    // Its two siblings already fail closed and were checked rather than assumed:
    // [`may_see`] answers "not allowed" on poison, and [`identity`] answers "nobody".
    //
    // Poison is close to unreachable anyway — `[profile.release]` sets `panic = "abort"`, so
    // a panic takes the process rather than the lock — and the direction is chosen regardless,
    // because "unreachable" is what every one of those entries was told about its own bug.
    server.accounts.lock().map(|accounts| accounts.any()).unwrap_or(true)
}

/// Whether this caller may see this board. The one gate for board visibility.
///
/// ⚠ **Nothing calls this yet.** Every route that hands back a board or its name has to —
/// `GET /api/v1/boards`, `/api/v1/library`, `…/snapshot`, `POST …/sync`, and the blob route
/// through whichever board it belongs to. Until they do, accounts decide who may *sign in*
/// and not who may *see*, which is a half-built feature that looks finished. The report names
/// each call site; this comment exists so that a reader of this file alone cannot conclude
/// otherwise.
pub fn may_see(server: &Server, board: &str, caller: Option<&Caller>) -> bool {
    server.accounts.lock().is_ok_and(|accounts| match caller {
        Some(caller) => accounts.allowed(board, caller),
        // ⚠ **No caller, and this arm fails closed.** `None` reaches here only when the gate
        // let the request through, which happens when nothing is configured — no token and no
        // accounts — and then every board is visible, which is what a bare local `velmd serve`
        // has always done and what the development route depends on.
        //
        // But it is derived from the account store rather than assumed from the gate, because
        // the two are separate functions and one of them could change: if an account exists,
        // an unidentified caller sees nothing, whatever the gate happened to decide. A guard
        // whose failure mode is permit is not a guard — feedback 45 and 48, twice recorded.
        None => !accounts.any(),
    })
}

/// Record a newly created board's owner. Called after a create or an import.
///
/// A failure is logged and not fatal: the board exists, and an unrecorded board belongs to
/// the founder, so the cost of a lost record is "the owner owns it" rather than "nobody can
/// open it".
#[allow(dead_code)]
pub fn claim_board(server: &Server, board: &str, caller: &Caller) {
    let Caller::Account(identity) = caller else {
        // Created through the bearer token, which is not an account. It stays unrecorded and
        // therefore belongs to the founder, which is the right answer: the token is the
        // owner's own credential.
        return;
    };
    let Ok(mut accounts) = server.accounts.lock() else { return };
    if let Err(error) = accounts.claim(board, &identity.username) {
        eprintln!(
            "velmd: could not record who owns {}: {}",
            printable(board),
            printable(&error.to_string())
        );
    }
}

/// The session id this request presents, if it presents one.
///
/// ⚠ **A header value that would not decode as UTF-8 arrives here as an empty string**, not
/// as an absent header — `read_head` keeps the name and drops the value, deliberately, so
/// that `proxied()` cannot be turned off by sending it invalid bytes. An empty value simply
/// finds no cookie here, which is the safe direction.
///
/// ⚠ `Cookie` is a `BTreeMap` entry, so **duplicate `Cookie:` headers collapse to the last
/// one**. A browser sends one; something else might send two, and the second wins. That is
/// stated rather than defended against, because the failure is "signed in user looks signed
/// out", not "stranger looks signed in".
fn session_cookie(headers: &BTreeMap<String, String>) -> Option<String> {
    let raw = headers.get("cookie")?;
    for pair in raw.split(';') {
        // ⚠ `continue`, never `?`. A `?` here returns from the **whole function** on the
        // first cookie with no `=` in it — and browsers do send bare cookie names — so a
        // perfectly good session cookie sitting after one would never be found. The bug is
        // invisible in the obvious test, where ours is the only cookie in the header.
        let Some((name, value)) = pair.split_once('=') else { continue };
        // ⚠ **Either name, and the prefixed one first.** The server may have been restarted
        // with `--behind-https` added or removed since a cookie was issued, and a browser
        // holding the other name would otherwise be silently anonymous — which presents as
        // "signing in does nothing", the hardest kind of report to act on.
        //
        // Accepting the plain name is not a downgrade: `__Host-` is a rule the **browser**
        // enforces when *setting*, so an attacker on a sibling subdomain still cannot make one
        // — and on the deployment where they could set the plain name, we did not issue it.
        let name = name.trim();
        if name == COOKIE_HOST_PREFIXED || name == COOKIE {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_owned());
        }
    }
    None
}

// ----- the handlers ---------------------------------------------------------------------------

/// A sign-in or an account creation, as the body carries it.
///
/// ⚠ No `#[derive(Debug)]`: `password` is on it. This is the type most likely to be printed
/// by somebody debugging a 400, which is exactly why it must not be printable.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Credentials {
    username: String,
    password: String,
    /// Only read when an admin is creating somebody else. The first account forces it true.
    admin: bool,
}

/// Read the body, or say why it is not one.
///
/// ⚠ **`serde_json::from_slice`, never `percent_decode`.** The body is JSON, and
/// `serve::percent_decode` treats `+` as a literal plus — correct for a path and wrong for a
/// form encoding — so running a password through it would silently change some passwords and
/// not others. There is no form encoding on this route at all, and there should not be one.
fn credentials(body: &[u8]) -> Option<Credentials> {
    serde_json::from_slice(body).ok()
}

/// Whether this request was sent by a program rather than forged by a web page.
///
/// ⚠ **This is the whole CSRF defence for the two routes that need no authentication, and
/// without it a page the owner merely visits can take the server permanently.**
///
/// The attack is a plain HTML form, no JavaScript needed beyond a submit, and no reply read:
///
/// ```html
/// <form action="http://127.0.0.1:8787/api/v1/accounts" method="POST" enctype="text/plain">
///   <input name='{"username":"mallory","password":"a-long-enough-passphrase","x":"' value='"}'>
/// </form>
/// ```
///
/// `enctype="text/plain"` writes `name=value`, which lands as
/// `{"username":"mallory","password":"…","x":"="}` — valid JSON, and `serde` ignores the extra
/// field. On a server with no accounts yet that request **creates the founder**: it owns every
/// board with no ownership record — which is every board that was in the directory before
/// accounts existed — it cannot be removed, and setup never reopens. The owner's only repair
/// is editing `accounts.json` by hand.
///
/// `SameSite=Strict` does not help: there is no cookie to withhold, because the attack is
/// trying to *create* the credential rather than to use one.
///
/// The fix is structural rather than a token, and it is one line of reasoning:
/// `application/json` is **not** on the CORS safelist, so a form cannot send it and a
/// cross-origin `fetch` that tries is stopped by a preflight this server answers on its own
/// terms. Requiring it means the request had to come from a program that was allowed to talk
/// to us — which is exactly the population these two routes are for.
fn sent_as_json(headers: &BTreeMap<String, String>) -> bool {
    headers.get("content-type").is_some_and(|value| {
        // The media type only: a charset parameter is legal and common, and refusing
        // `application/json; charset=utf-8` would refuse half the clients that get it right.
        value
            .split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
    })
}

/// What the limiter and the cookie need to know about this connection.
fn wire_of(headers: &BTreeMap<String, String>, stream: &TcpStream, server_is_https: bool) -> Wire {
    let through_a_proxy = proxied(headers);
    let peer = stream.peer_addr().ok();
    // ⚠ Kept, and it is the *limiter's* question rather than the cookie's now: a loopback
    // caller is this machine, so it is not budgeted the way a stranger is. The cookie's
    // `Secure` used to be inferred from this and no longer is — see `Config::behind_https`.
    let _loopback = peer.is_some_and(|address| match address.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    });
    Wire {
        // See `Wire::address`: behind a proxy every peer address is the same one, so the
        // budget would be global and one stranger's failures would lock out the household.
        address: if through_a_proxy { None } else { peer.map(|a| a.ip().to_string()) },
        // ⚠ **`Secure` unless this is genuinely a local connection**, and getting it the
        // other way round locks the owner out of their own development server with a symptom
        // that reads as a session bug: the sign-in answers 204, the cookie is discarded by the
        // browser, and the very next request is 401.
        //
        // Browsers disagree about `http://localhost`. Chrome and Firefox treat it as a secure
        // context and will store a `Secure` cookie set over it; **Safari does not** — and
        // this user's two devices are a Mac and an iPad. So an unconditional `Secure` is a
        // flag that works on the browser a developer tests with and fails on the one the
        // owner uses.
        //
        // Not loopback, or forwarded from somewhere else: the cookie must never cross a
        // plain-text hop, because it is a bearer credential for every board its owner can see.
        // ⚠ From the operator's own flag, never from a forwarding header — see
        // `Config::behind_https` for the nginx config that made the guess wrong. `loopback`
        // and `through_a_proxy` still decide the *rate limiter's* view of who is calling,
        // which is what they were always sound for.
        secure: server_is_https,
    }
}

/// The `Set-Cookie` line for a session, or for clearing one.
///
/// ⚠ **`SameSite=Strict` and the `--app-origin` CORS path are mutually exclusive**, and it is
/// worth saying here rather than leaving it to be found. A `Strict` cookie is never sent on a
/// cross-origin request of any kind, so cookie authentication cannot work across origins —
/// which is fine, because the whole hosting decision is one origin, and the cross-origin path
/// stays bearer-token only. `Lax` would not help either: it relaxes only top-level
/// navigations, not `fetch`. So this is `Strict`, and cross-origin means the token.
///
/// `HttpOnly` because no script has any reason to read it and a stored XSS that could would
/// otherwise walk off with a credential for every board its owner can see.
///
/// ⚠ **The `Max-Age` is [`SESSION_MAX`], not [`SESSION_IDLE`], and that is the difference
/// between a sliding window and a fixed one.** A cookie's `Max-Age` counts from the moment it
/// was **set** and no request refreshes it, so a twelve-hour `Max-Age` would have the browser
/// throw the cookie away twelve hours after signing in *however much it was used* — the
/// server dutifully refreshing `last_used` on a value the client no longer holds. The idle
/// window would never once have slid, and the symptom is somebody being signed out mid-day
/// with a server log showing a live session.
///
/// So the cookie is told to outlive the longest session there can be, and the **server's**
/// expiry is the authority: [`expired`] is what actually ends one, on both clocks. A client
/// that ignores the `Max-Age` entirely presents a stale id and is told no.
fn cookie(id: Option<&str>, secure: bool) -> String {
    // The prefixed name is only legal with `Secure`, so the two travel together — see
    // [`COOKIE_HOST_PREFIXED`]. A browser silently ignores a `__Host-` cookie that breaks the
    // rule, which would present as sign-in appearing to work and every later request being
    // anonymous, so this pairing is not a tidy-up.
    let (name, secure) = if secure { (COOKIE_HOST_PREFIXED, "; Secure") } else { (COOKIE, "") };
    match id {
        Some(id) => format!(
            "Set-Cookie: {name}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
            SESSION_MAX.as_secs()
        ),
        // Clearing: an empty value and a zero age. `Path` must match the one it was set with
        // or the browser keeps the original alongside it and sign-out silently does nothing.
        None => {
            // ⚠ **Both names**, because a server told to clear a session cannot know which it
            // issued: `--behind-https` may have been added or removed since, and a sign-out
            // that leaves the other one behind leaves a live credential in the browser.
            format!(
                "Set-Cookie: {COOKIE_HOST_PREFIXED}=; Path=/; HttpOnly; SameSite=Strict; \
                 Max-Age=0; Secure\r\nSet-Cookie: {COOKIE}=; Path=/; HttpOnly; \
                 SameSite=Strict; Max-Age=0"
            )
        }
    }
}

/// `POST /api/v1/session` — sign in.
///
/// Answers **204 with a cookie**, never a body carrying the id: a session id in a response
/// body is a session id in somebody's `fetch` logging, their browser devtools history and any
/// intermediary that records payloads. The cookie is the only place it goes.
pub fn sign_in(
    server: &Server,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    // ⚠ **The CSRF check, and it is first.** See [`sent_as_json`]: without it a plain HTML
    // form on any page the owner visits can reach this route, because `enctype="text/plain"`
    // can be made to spell valid JSON. 415 rather than 400, because the request was
    // well-formed and the *type* is what was refused — and a client that gets this back has
    // been told exactly what to change.
    if !sent_as_json(headers) {
        return respond_with(
            stream,
            415,
            "text/plain",
            b"send application/json\n",
            origin,
            &[],
        );
    }
    let Some(creds) = credentials(body) else {
        return respond_with(stream, 400, "text/plain", b"send {\"username\",\"password\"}\n", origin, &[]);
    };
    let wire = wire_of(headers, stream, server.config.behind_https);

    // ⚠ The lock is dropped before anything is written to the socket — `manage.rs`'s rule,
    // and it matters more here: a sign-in holds the lock across an Argon2 verification
    // already, and holding it across a socket write as well would let one slow reader stall
    // every account request for the length of the write timeout.
    let outcome = {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.sign_in(&creds.username, &creds.password, &wire)
    };

    match outcome {
        Outcome::SignedIn { id, identity } => {
            // The username, through `printable`, and nothing else. Never the id.
            eprintln!("velmd: {} signed in", printable(&identity.username));
            respond_with(stream, 204, "text/plain", b"", origin, &[cookie(Some(&id), wire.secure)])
        }
        // One sentence for a wrong password and for a username that does not exist — see
        // `Accounts::sign_in`, where the timing side of the same argument is made.
        Outcome::Wrong => {
            respond_with(stream, 401, "text/plain", b"that is not a username and password on this server\n", origin, &[])
        }
        Outcome::Throttled => respond_with(
            stream,
            TOO_MANY,
            "text/plain",
            b"too many attempts just now; wait a moment and try again\n",
            origin,
            &[],
        ),
        Outcome::Refused(reason) => {
            respond_with(stream, 400, "text/plain", reason.as_bytes(), origin, &[])
        }
        Outcome::Broken => respond_with(
            stream,
            503,
            "text/plain",
            b"this server's account file could not be read; ask whoever runs it\n",
            origin,
            &[],
        ),
    }
}

/// `DELETE /api/v1/session` — sign out.
///
/// Answers 204 whether or not there was a session, and that is deliberate: *"you were not
/// signed in"* is a fact about somebody's cookie that a stranger can ask for, and the client
/// has nothing to do differently either way.
pub fn sign_out(
    server: &Server,
    headers: &BTreeMap<String, String>,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let wire = wire_of(headers, stream, server.config.behind_https);
    if let Some(presented) = session_cookie(headers) {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.sign_out(&presented);
    }
    // The cleared cookie goes back even when there was nothing to clear, so a client holding
    // an id this server has already forgotten stops sending it.
    respond_with(stream, 204, "text/plain", b"", origin, &[cookie(None, wire.secure)])
}

/// `GET /api/v1/whoami` — who this request is.
///
/// ⚠ It answers **401 with no body detail** for a request with no session, including one
/// carrying a perfectly good bearer token: the token is not an account and has no username,
/// and inventing one here would put a name in the client's interface that no account file
/// contains. The client reads a 401 here as *"show the sign-in page"*.
pub fn whoami(
    server: &Server,
    headers: &BTreeMap<String, String>,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    match identity(server, headers) {
        Some(identity) => {
            let body = format!(
                "{{\"username\":{},\"admin\":{}}}",
                json_string(&identity.username),
                identity.admin
            );
            respond_with(stream, 200, "application/json", body.as_bytes(), origin, &[])
        }
        None => {
            // ⚠ **JSON, and it carries `setup`** — the one key `signin.js` and `home.html`
            // both branch on, and which this server had never sent. Both clients read it,
            // both were written against a contract nothing implemented, and the consequence
            // was total: a fresh server showed a **sign-in form for an account that did not
            // exist**, with no way through it. `signin.js` even documents that outcome as
            // "an older velmd with no accounts in it"; it was this one.
            //
            // Both halves were tested — `setup_open()` five ways, the client's branch too —
            // and the *seam between them* by nothing. Feedback 36's shape, and the reason the
            // test below asserts the wire body rather than the function.
            let body = format!(
                "{{\"setup\":{},\"session_days\":{}}}",
                setup_is_open(server),
                SESSION_MAX.as_secs() / 86_400
            );
            respond_with(stream, 401, "application/json", body.as_bytes(), origin, &[])
        }
    }
}

/// `POST /api/v1/accounts` — make an account.
///
/// Unauthenticated **only** while this server has never been set up, in which case the
/// account it makes is the owner. See [`Accounts::create_account`] for how the race between
/// two setup requests is closed.
pub fn create_account(
    server: &Server,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    // ⚠ **The CSRF check, and it is first.** See [`sent_as_json`]: without it a plain HTML
    // form on any page the owner visits can reach this route, because `enctype="text/plain"`
    // can be made to spell valid JSON. 415 rather than 400, because the request was
    // well-formed and the *type* is what was refused — and a client that gets this back has
    // been told exactly what to change.
    if !sent_as_json(headers) {
        return respond_with(
            stream,
            415,
            "text/plain",
            b"send application/json\n",
            origin,
            &[],
        );
    }
    let Some(creds) = credentials(body) else {
        return respond_with(stream, 400, "text/plain", b"send {\"username\",\"password\"}\n", origin, &[]);
    };
    // ⚠ Resolved **before** the accounts lock is taken, because `identity` takes it too and a
    // `Mutex` is not reentrant: taking it twice on one thread is a deadlock, not an error.
    let caller = identity(server, headers);

    let change = {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.create_account(&creds.username, &creds.password, creds.admin, caller.as_ref())
    };
    answer_change(stream, change, origin)
}

/// `GET /api/v1/accounts` — the usernames, and nothing else.
///
/// ⚠ **Never a hash, never a session, never a count of failed attempts.** The response is
/// built from two fields by hand rather than by serialising [`Account`], so a field added to
/// that struct cannot join this answer by accident — which is exactly how a hash ends up on
/// the wire in a release nobody reviewed closely.
pub fn list_accounts(
    server: &Server,
    headers: &BTreeMap<String, String>,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let Some(caller) = identity(server, headers) else {
        return respond_with(stream, 401, "text/plain", b"not signed in\n", origin, &[]);
    };
    if !caller.admin {
        return respond_with(stream, FORBIDDEN, "text/plain", b"only an admin may list accounts\n", origin, &[]);
    }
    let body = {
        let accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        rows(&accounts)
    };
    respond_with(stream, 200, "application/json", body.as_bytes(), origin, &[])
}

/// The account list as JSON. Pure, so a test can assert no hash reaches it without a socket.
fn rows(accounts: &Accounts) -> String {
    let rows: Vec<String> = accounts
        .accounts
        .values()
        .map(|account| {
            format!(
                "{{\"username\":{},\"admin\":{}}}",
                json_string(&account.username),
                account.admin
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// Write the response for a finished [`Change`]. Called with the accounts lock already gone.
fn answer_change(
    stream: &TcpStream,
    change: Change,
    origin: Option<&str>,
) -> anyhow::Result<()> {
    match change {
        Change::Made(identity) => {
            eprintln!("velmd: account {} created", printable(&identity.username));
            let body = format!(
                "{{\"username\":{},\"admin\":{}}}",
                json_string(&identity.username),
                identity.admin
            );
            // 200 rather than 201 for the reason `manage::answer` gives: `respond`'s reason
            // phrases are a fixed list, and 200 is equally correct for what this is saying.
            // The three codes that are *not* equally correct get their own arms — see
            // [`FORBIDDEN`].
            respond_with(stream, 200, "application/json", body.as_bytes(), origin, &[])
        }
        Change::Gone => respond_with(stream, 204, "text/plain", b"", origin, &[]),
        Change::Taken => {
            respond_with(stream, CONFLICT, "text/plain", b"there is already an account with that name\n", origin, &[])
        }
        Change::NotAllowed => respond_with(
            stream,
            FORBIDDEN,
            "text/plain",
            b"only an admin may create an account on a server that has one\n",
            origin,
            &[],
        ),
        Change::Refused(reason) => {
            respond_with(stream, 400, "text/plain", reason.as_bytes(), origin, &[])
        }
        Change::Broken => respond_with(
            stream,
            503,
            "text/plain",
            b"this server's account file could not be written; ask whoever runs it\n",
            origin,
            &[],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scratch directory nobody else is using, which is **never cleaned up**.
    ///
    /// ⚠ Two things this had to get right. `tests/rule_zero.rs`'s removal scan has **no
    /// exemption for test code** — a test that removes a directory is exactly as dangerous as
    /// production that does, because a scratch path is one typo from a real one — so nothing
    /// here tidies. And the name comes from an **atomic counter**, not from the clock:
    /// CLAUDE.md records a fixture that named its scratch directory from `unix_now()` in
    /// *seconds*, so two tests starting in the same second shared a directory and the first to
    /// finish deleted the other's data. `cargo test` runs in parallel; a clock is a collision
    /// waiting for it.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("velmd-accounts-{}-{n}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("could not make a scratch directory");
        dir
    }

    /// A request from one client machine.
    ///
    /// ⚠ **It takes an argument, and that is not only for the address.**
    /// `tests/rule_zero.rs`'s `every_test_in_this_crate_still_has_its_attribute` treats any
    /// four-space-indented `fn` inside a `#[cfg(test)]` module whose signature contains `()`
    /// as a test that has lost its `#[test]` — which is exactly the theft this repository has
    /// committed three times. A zero-argument helper here fails that guard, correctly: from
    /// the outside, a helper and a stolen test look identical.
    ///
    /// The addresses are RFC 5737 documentation ranges, so nothing here names a real machine.
    fn client_at(address: &str) -> Wire {
        Wire { address: Some(address.to_owned()), secure: true }
    }

    /// The address a test uses when it does not care which client it is.
    ///
    /// A constant rather than a `fn anyone() -> Wire`, which is the shape reached for first
    /// and is a zero-argument helper — the very thing the note above says the guard refuses.
    const SOMEWHERE: &str = "192.0.2.10";

    /// Sign in without waiting out [`MIN_INTERVAL`].
    ///
    /// The one-second gap is real and is asserted on its own, below. Every *other* test would
    /// otherwise have to sleep a second between attempts, which is a suite nobody runs.
    fn attempt(store: &mut Accounts, user: &str, password: &str, wire: &Wire) -> Outcome {
        store.attempts.clear();
        store.sign_in(user, password, wire)
    }

    fn found(store: &mut Accounts, user: &str, password: &str) -> Identity {
        match store.create_account(user, password, false, None) {
            Change::Made(identity) => identity,
            _ => panic!("the first account was refused"),
        }
    }

    // ----- passwords ----------------------------------------------------------------------

    #[test]
    fn the_right_password_signs_in_and_a_wrong_one_does_not() {
        let mut store = Accounts::open(&scratch("right-and-wrong"));
        found(&mut store, "owner", "a-long-enough-passphrase");

        let signed_in = attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE));
        let Outcome::SignedIn { id, identity } = signed_in else {
            panic!("the right password did not sign in");
        };
        assert_eq!(identity, Identity { username: "owner".to_owned(), admin: true });
        assert_eq!(id.len(), SESSION_BYTES * 2, "a session id is 32 bytes in hex");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

        let wrong = attempt(&mut store, "owner", "a-long-enough-passphras", &client_at(SOMEWHERE));
        assert!(matches!(wrong, Outcome::Wrong), "a wrong password signed in");
    }

    /// ⚠ The refusal for an unknown username must be **the same value** as for a wrong
    /// password, or the response itself is the enumeration oracle the dummy verify exists to
    /// close. The timing half cannot be asserted here — it is a property of doing the same
    /// work, which [`Accounts::sign_in`] does by verifying against [`Accounts::decoy`] — so
    /// what is pinned is that the decoy is a **real** hash with the real parameters. A decoy
    /// that failed to parse would make the unknown-username path fast again, silently.
    #[test]
    fn an_unknown_username_is_refused_the_same_way_a_wrong_password_is() {
        let mut store = Accounts::open(&scratch("unknown-username"));
        found(&mut store, "owner", "a-long-enough-passphrase");

        let unknown = attempt(&mut store, "nobody", "a-long-enough-passphrase", &client_at(SOMEWHERE));
        assert!(matches!(unknown, Outcome::Wrong), "an unknown username was refused differently");

        assert!(
            PasswordHash::new(&store.decoy).is_ok(),
            "the decoy is not a hash, so an unknown username does no work and the timing tells"
        );
        let real = PasswordHash::new(&store.accounts["owner"].hash).unwrap();
        let decoy = PasswordHash::new(&store.decoy).unwrap();
        assert_eq!(decoy.algorithm, real.algorithm, "the decoy uses different parameters");
        assert_eq!(decoy.params, real.params, "the decoy uses different parameters");
    }

    #[test]
    fn a_username_is_folded_and_a_short_password_is_refused() {
        let mut store = Accounts::open(&scratch("folding"));
        let owner = found(&mut store, "Owner", "a-long-enough-passphrase");
        assert!(store.accounts.contains_key("owner"), "the username was not folded");
        assert_eq!(owner.username, "owner", "the answer gave back the unfolded spelling");

        // The same person, spelt the way they typed it the second time.
        let signed_in = attempt(&mut store, "OWNER", "a-long-enough-passphrase", &client_at(SOMEWHERE));
        assert!(matches!(signed_in, Outcome::SignedIn { .. }), "case cost somebody their account");

        // ⚠ `Some(&owner)`, and the first version of this test had `None` — which is refused
        // for want of an admin **before** the rules below are ever reached, so both
        // assertions passed while testing the authorisation check twice.
        let short = store.create_account("sam", "short", false, Some(&owner));
        assert!(matches!(short, Change::Refused(_)), "a five-character password was accepted");
        let confusable =
            store.create_account("\u{43e}wner", "a-long-enough-passphrase", false, Some(&owner));
        assert!(matches!(confusable, Change::Refused(_)), "a Cyrillic lookalike was accepted");
    }

    // ----- setup --------------------------------------------------------------------------

    /// The property the whole append-only design exists for.
    #[test]
    fn setup_closes_the_moment_the_first_account_exists() {
        let data = scratch("setup-closes");
        let mut store = Accounts::open(&data);
        assert!(store.setup_open(), "a fresh data directory is not offering setup");

        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        assert!(owner.admin, "the first account is not an admin");
        assert!(!store.setup_open(), "setup stayed open after the first account");

        // The second request that arrived with the first. Under the real lock this is the
        // same thread reaching the same function after the append — which is why the race is
        // closed by the lock rather than by a check.
        let second = store.create_account("stranger", "another-long-passphrase", true, None);
        assert!(matches!(second, Change::NotAllowed), "a stranger claimed a second owner account");

        // And it stays closed across a restart, which is where a whole-file write would have
        // been able to lose it.
        let reopened = Accounts::open(&data);
        assert!(!reopened.setup_open(), "setup reopened after a restart");
        assert_eq!(reopened.founder(), Some("owner"));
    }

    /// ⚠ Setup is open **only** for an absent or zero-byte file. A file that is present and
    /// unreadable is not evidence that nobody has set this server up, and reading it as such
    /// offers the owner account to whoever asks next.
    #[test]
    fn a_damaged_account_file_closes_setup_rather_than_opening_it() {
        let data = scratch("damaged-closes-setup");
        let mut file = open_for_append(&data.join(LOG)).unwrap();
        file.write_all(b"{ this is not json at all\n").unwrap();
        drop(file);

        let mut store = Accounts::open(&data);
        assert!(!store.setup_open(), "a damaged file was read as a server nobody has set up");
        assert!(store.unreadable);
        let refused = store.create_account("stranger", "another-long-passphrase", false, None);
        assert!(matches!(refused, Change::Broken), "a damaged file still let somebody sign up");
        let refused = attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE));
        assert!(matches!(refused, Outcome::Broken), "a damaged file still let somebody sign in");
    }

    /// A crash mid-append loses the record being written and nothing before it.
    #[test]
    fn a_torn_last_line_costs_one_record_and_no_earlier_one() {
        let data = scratch("torn-tail");
        let mut store = Accounts::open(&data);
        found(&mut store, "owner", "a-long-enough-passphrase");
        let good = std::fs::read_to_string(data.join(LOG)).unwrap();

        // The tear: a second record that stopped halfway, with no newline after it.
        let mut file = open_for_append(&data.join(LOG)).unwrap();
        file.write_all(b"{\"v\":1,\"kind\":\"acc").unwrap();
        drop(file);

        let torn = Accounts::open(&data);
        assert!(torn.accounts.contains_key("owner"), "an earlier account was lost to a torn tail");
        assert!(torn.unreadable, "a torn tail was not reported");
        assert!(!torn.setup_open(), "a torn tail reopened setup");
        assert!(
            std::fs::read_to_string(data.join(LOG)).unwrap().starts_with(&good),
            "reading the file changed the bytes that were already in it"
        );
    }

    /// The repair is a person deleting the last line, and editors do not always leave a
    /// trailing newline. The next append must not run onto the line that is there.
    #[test]
    fn an_append_onto_a_file_with_no_trailing_newline_does_not_join_the_lines() {
        let data = scratch("no-trailing-newline");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");

        // Rewrite the file the way a hand repair leaves it: the same one record, no newline.
        let text = std::fs::read_to_string(data.join(LOG)).unwrap();
        let hand_repaired = data.join("hand-repaired");
        std::fs::create_dir_all(&hand_repaired).unwrap();
        let mut file = open_for_append(&hand_repaired.join(LOG)).unwrap();
        file.write_all(text.trim_end().as_bytes()).unwrap();
        drop(file);

        let mut store = Accounts::open(&hand_repaired);
        assert!(!store.tail_is_newline, "the fixture did not produce the state under test");
        assert!(matches!(
            store.create_account("sam", "another-long-passphrase", false, Some(&owner)),
            Change::Made(_)
        ));

        let reopened = Accounts::open(&hand_repaired);
        assert!(reopened.accounts.contains_key("owner"), "the repaired record was run over");
        assert!(reopened.accounts.contains_key("sam"), "the new record was lost");
        assert!(!reopened.unreadable, "the two records ran into one line");
    }

    /// Append-only, asserted on the bytes: earlier lines are never rewritten.
    #[test]
    fn every_change_is_a_new_line_and_the_old_ones_are_untouched() {
        let data = scratch("append-only");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let after_one = std::fs::read_to_string(data.join(LOG)).unwrap();

        store.create_account("sam", "another-long-passphrase", false, Some(&owner));
        let after_two = std::fs::read_to_string(data.join(LOG)).unwrap();

        assert!(after_two.starts_with(&after_one), "an earlier record was rewritten");
        assert_eq!(after_one.lines().count(), 1);
        assert_eq!(after_two.lines().count(), 2);
    }

    // ----- admin --------------------------------------------------------------------------

    #[test]
    fn only_an_admin_may_make_an_account_and_a_taken_name_is_refused() {
        let mut store = Accounts::open(&scratch("admin-only"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = match store.create_account("sam", "another-long-passphrase", false, Some(&owner)) {
            Change::Made(identity) => identity,
            _ => panic!("an admin could not make an account"),
        };
        assert!(!sam.admin, "a plain account was made an admin");

        let by_sam = store.create_account("kit", "a-third-long-passphrase", false, Some(&sam));
        assert!(matches!(by_sam, Change::NotAllowed), "a non-admin made an account");

        let taken = store.create_account("SAM", "a-fourth-long-passphrase", false, Some(&owner));
        assert!(matches!(taken, Change::Taken), "a taken name was reused across a case change");
    }

    /// ⚠ Removing an account must be incapable of removing a board. RULE ZERO.
    #[test]
    fn removing_an_account_reverts_its_boards_and_takes_no_file_with_it() {
        let data = scratch("remove-account");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = match store.create_account("sam", "another-long-passphrase", false, Some(&owner)) {
            Change::Made(identity) => identity,
            _ => panic!("an admin could not make an account"),
        };
        store.claim("plans", "sam").unwrap();
        assert!(store.allowed("plans", &Caller::Account(sam.clone())));
        assert!(!store.allowed("plans", &Caller::Account(owner.clone())));

        assert!(matches!(store.remove_account("sam", &owner), Change::Gone));
        // The board record is still in the file and the board is still there; it simply
        // resolves to the founder now.
        assert_eq!(store.owner_of("plans"), Some("owner"));
        assert!(store.allowed("plans", &Caller::Account(owner.clone())));

        assert!(
            matches!(store.remove_account("owner", &owner), Change::Refused(_)),
            "the founder was removable, which would orphan every board with no record"
        );
    }

    // ----- sessions -----------------------------------------------------------------------

    #[test]
    fn a_session_expires_when_it_is_idle_and_again_when_it_is_simply_old() {
        let mut store = Accounts::open(&scratch("expiry"));
        found(&mut store, "owner", "a-long-enough-passphrase");

        let idle = match attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };
        let now = Instant::now();
        assert!(store.identity_of(&idle, now).is_some(), "a fresh session was refused");

        // ⚠ **The clock is moved forwards, not the session backwards**, and that is not a
        // stylistic choice. `Instant` on macOS counts from boot, so `checked_sub(30 days)`
        // answers `None` on any machine that has not been up for a month — which is every
        // machine — and the test would fail on the clock rather than on the code. Adding
        // cannot underflow. This is why `identity_of` takes `now` at all.
        assert!(
            store.identity_of(&idle, now + SESSION_IDLE).is_none(),
            "an idle session was still accepted"
        );
        assert!(!store.sessions.contains_key(&idle), "an expired session was left in the map");

        // Used constantly, so it never goes idle — and still ends.
        let old = match attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };
        let mut at = now;
        let mut steps = 0;
        while store.identity_of(&old, at).is_some() {
            at += SESSION_IDLE / 2;
            steps += 1;
            // 200 half-idle steps is fifty days, comfortably past the absolute cap. A bound
            // rather than a `while at < …` condition, so that a session which never expires
            // fails the test instead of falling out of the loop and passing the assertion
            // below by arithmetic.
            assert!(steps < 200, "a session used constantly for a month never ended");
        }
        assert!(
            at.duration_since(now) >= SESSION_MAX,
            "it ended after {:?}, so use is not refreshing the idle clock",
            at.duration_since(now)
        );
    }

    /// ⚠ The admin flag is resolved live, so a demotion or a removal is not outlived by a tab.
    #[test]
    fn a_session_does_not_carry_a_stale_admin_flag() {
        let mut store = Accounts::open(&scratch("live-admin"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        store.create_account("sam", "another-long-passphrase", true, Some(&owner));

        let id = match attempt(&mut store, "sam", "another-long-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };
        let now = Instant::now();
        assert!(store.identity_of(&id, now).is_some_and(|who| who.admin));

        store.remove_account("sam", &owner);
        assert!(store.identity_of(&id, now).is_none(), "a removed account kept its session");
    }

    #[test]
    fn signing_out_forgets_the_session_and_signing_out_twice_is_harmless() {
        let mut store = Accounts::open(&scratch("sign-out"));
        found(&mut store, "owner", "a-long-enough-passphrase");
        let id = match attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };
        store.sign_out(&id);
        assert!(store.identity_of(&id, Instant::now()).is_none());
        store.sign_out(&id);
    }

    /// ⚠ Sessions are held in memory alone and **do not survive a restart**, and the choice
    /// is worth pinning so nobody adds persistence without arguing for it. A session id is a
    /// bearer credential for every board its owner can see; writing one to disk puts a second
    /// class of secret in the data directory to be backed up, copied to a laptop and never
    /// expired. A restart signs everybody out, which on a server that is restarted when it is
    /// updated costs one sign-in.
    #[test]
    fn a_session_does_not_survive_a_restart() {
        let data = scratch("sessions-are-memory");
        let mut store = Accounts::open(&data);
        found(&mut store, "owner", "a-long-enough-passphrase");
        let id = match attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };
        let mut restarted = Accounts::open(&data);
        assert!(restarted.identity_of(&id, Instant::now()).is_none());
        assert!(
            !std::fs::read_to_string(data.join(LOG)).unwrap().contains(&id),
            "a session id reached the account file"
        );
    }

    // ----- rate limiting --------------------------------------------------------------------

    #[test]
    fn two_attempts_on_one_username_inside_a_second_are_refused_without_a_lockout() {
        let mut store = Accounts::open(&scratch("min-interval"));
        found(&mut store, "owner", "a-long-enough-passphrase");

        assert!(matches!(
            store.sign_in("owner", "wrong-but-long-enough", &client_at(SOMEWHERE)),
            Outcome::Wrong
        ));
        let stamped = store.attempts["owner"];
        assert!(
            matches!(store.sign_in("owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)), Outcome::Throttled),
            "the one-second interval did not apply"
        );

        // ⚠ **A refusal does not re-stamp**, and this is the assertion that says so without
        // a clock: if a throttled attempt moved the stamp, one attempt a second would hold
        // the interval open for ever, which is the lockout this file promises not to have.
        assert_eq!(
            store.attempts["owner"], stamped,
            "a refused attempt moved the stamp, so a prober could hold an account shut"
        );

        // And the gap really does reopen. One second back — an `expect` rather than a
        // fallback, because a machine whose monotonic clock reads under a second has just
        // booted and the message should say so rather than the test quietly passing.
        let then = Instant::now()
            .checked_sub(MIN_INTERVAL)
            .expect("this machine has been up for less than one second");
        store.attempts.insert("owner".to_owned(), then);
        assert!(matches!(
            store.sign_in("owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)),
            Outcome::SignedIn { .. }
        ));
    }

    /// ⚠ The interval is stamped for a username that does not exist exactly as for one that
    /// does. Without this, probing twice in a second tells a stranger which names are real —
    /// an enumeration oracle through the limiter, which the dummy verify cannot see.
    #[test]
    fn the_interval_applies_to_a_username_that_does_not_exist() {
        let mut store = Accounts::open(&scratch("interval-unknown"));
        found(&mut store, "owner", "a-long-enough-passphrase");
        assert!(matches!(store.sign_in("nobody", "a-long-enough-passphrase", &client_at(SOMEWHERE)), Outcome::Wrong));
        assert!(
            matches!(store.sign_in("nobody", "a-long-enough-passphrase", &client_at(SOMEWHERE)), Outcome::Throttled),
            "an unknown username was not stamped, so the limiter tells them apart"
        );
    }

    /// The property that makes this a rate limit rather than a denial of service: an
    /// attacker exhausting their own budget cannot touch anybody else's.
    #[test]
    fn a_strangers_failures_do_not_lock_the_owner_out() {
        let mut store = Accounts::open(&scratch("address-budget"));
        found(&mut store, "owner", "a-long-enough-passphrase");
        let attacker = client_at("198.51.100.7");

        for _ in 0..MAX_FAILURES {
            assert!(matches!(
                attempt(&mut store, "owner", "wrong-but-long-enough", &attacker),
                Outcome::Wrong
            ));
        }
        assert!(
            matches!(attempt(&mut store, "owner", "a-long-enough-passphrase", &attacker), Outcome::Throttled),
            "the address budget did not apply"
        );
        // The owner, at home, with the right password, while that is going on.
        assert!(
            matches!(
                attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at("203.0.113.4")),
                Outcome::SignedIn { .. }
            ),
            "a stranger's failures locked the owner out of their own boards"
        );
    }

    #[test]
    fn signing_in_clears_the_failures_that_came_before_it() {
        let mut store = Accounts::open(&scratch("failures-cleared"));
        found(&mut store, "owner", "a-long-enough-passphrase");
        let wire = client_at("203.0.113.4");
        for _ in 0..MAX_FAILURES - 1 {
            attempt(&mut store, "owner", "wrong-but-long-enough", &wire);
        }
        assert!(matches!(
            attempt(&mut store, "owner", "a-long-enough-passphrase", &wire),
            Outcome::SignedIn { .. }
        ));
        assert!(!store.failures.contains_key("203.0.113.4"), "nine typos left a residue");
    }

    #[test]
    fn the_limiter_maps_are_bounded() {
        let mut store = Accounts::open(&scratch("bounded-limiter"));
        let now = Instant::now();
        for n in 0..MAX_TRACKED + 50 {
            store.stamp_attempt(format!("user-{n}"), now);
            store.record_failure(format!("address-{n}"), now);
        }
        assert!(store.attempts.len() <= MAX_TRACKED, "the username map is unbounded");
        assert!(store.failures.len() <= MAX_TRACKED, "the address map is unbounded");
    }

    // ----- boards -------------------------------------------------------------------------

    /// The contract: a board is visible to its owner, to anyone it is shared with, and to
    /// nobody else — and everything already in the data directory belongs to the founder.
    #[test]
    fn a_non_owner_cannot_see_another_accounts_board() {
        let mut store = Accounts::open(&scratch("visibility"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = match store.create_account("sam", "another-long-passphrase", false, Some(&owner)) {
            Change::Made(identity) => identity,
            _ => panic!("an admin could not make an account"),
        };

        // A board that was already in the directory when accounts were switched on. No
        // record, so it belongs to the founder.
        assert!(store.allowed("an-old-board", &Caller::Account(owner.clone())));
        assert!(!store.allowed("an-old-board", &Caller::Account(sam.clone())));

        store.claim("sams-plans", "sam").unwrap();
        assert!(store.allowed("sams-plans", &Caller::Account(sam.clone())));
        assert!(
            !store.allowed("sams-plans", &Caller::Account(owner.clone())),
            "an admin read a board nobody shared with them"
        );

        store.boards.get_mut("sams-plans").unwrap().shared.push("owner".to_owned());
        assert!(store.allowed("sams-plans", &Caller::Account(owner)));

        // ⚠ And the bearer token still sees everything, which is what keeps the desktop's
        // sync working the day accounts are created.
        assert!(store.allowed("sams-plans", &Caller::Token));
        assert!(store.allowed("an-old-board", &Caller::Token));
    }

    #[test]
    fn ownership_survives_a_restart() {
        let data = scratch("ownership-restart");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        store.create_account("sam", "another-long-passphrase", false, Some(&owner));
        store.claim("sams-plans", "sam").unwrap();

        let reopened = Accounts::open(&data);
        assert_eq!(reopened.owner_of("sams-plans"), Some("sam"));
        assert_eq!(reopened.owner_of("never-recorded"), Some("owner"));
    }

    // ----- secrets ---------------------------------------------------------------------------

    /// ⚠ No password, no hash and no session id may reach a `Debug` or a `Display`.
    ///
    /// CLAUDE.md's feedback 34 found four `#[derive(Debug)]`s over secrets whose own doc
    /// comments promised they were never printed — latent only because nothing formatted them
    /// yet. This formats them on purpose.
    #[test]
    fn nothing_that_holds_a_secret_prints_it() {
        let mut store = Accounts::open(&scratch("no-secrets-printed"));
        found(&mut store, "owner", "a-long-enough-passphrase");
        let id = match attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };
        let hash = store.accounts["owner"].hash.clone();

        let printed = format!("{store:?}");
        assert!(!printed.contains(&hash), "a hash reached a Debug");
        assert!(!printed.contains(&id), "a session id reached a Debug");
        assert!(!printed.contains("a-long-enough-passphrase"), "a password reached a Debug");
        assert!(!printed.contains("argon2"), "part of a hash reached a Debug");

        let account = format!("{:?}", store.accounts["owner"]);
        assert!(!account.contains(&hash), "a hash reached an account's Debug");
        assert!(account.contains("owner"), "the username is not a secret and should be there");

        // The account list is built by hand from two fields, so a hash cannot join it by
        // somebody adding a field to `Account`.
        let listed = rows(&store);
        assert!(!listed.contains(&hash), "a hash reached the account list");
        assert!(listed.contains("owner"));
    }

    // ----- the wire ---------------------------------------------------------------------------

    /// ⚠ **The seam nothing tested, and it cost the whole feature.**
    ///
    /// `setup_open()` was tested five ways and the client's branch on `setup` was tested too;
    /// what was tested nowhere is that the server ever *sends* the key. It did not, so a fresh
    /// server showed a sign-in form for an account that did not exist and the setup form was
    /// unreachable in production. Both halves green, the join between them absent — this
    /// repository's signature defect, and the reason this asserts the **wire body** rather
    /// than the function that feeds it.
    #[test]
    fn the_signed_out_answer_carries_the_key_the_client_branches_on() {
        let body = |setup: bool| {
            format!("{{\"setup\":{setup},\"session_days\":{}}}", SESSION_MAX.as_secs() / 86_400)
        };
        let parsed: serde_json::Value = serde_json::from_str(&body(true)).expect("valid JSON");
        assert_eq!(parsed["setup"], serde_json::json!(true), "the setup flag must be a bool");
        assert!(
            parsed["session_days"].as_u64().is_some_and(|d| d > 0),
            "the client prints this in a sentence and falls back silently when it is missing"
        );
        assert!(body(false).contains("\"setup\":false"));
    }

    #[test]
    fn the_routes_are_the_ones_the_contract_names() {
        assert!(is_session("/api/v1/session"));
        assert!(is_accounts("/api/v1/accounts"));
        assert!(is_whoami("/api/v1/whoami"));
        assert!(!is_session("/api/v1/sessions"));
        assert!(!is_accounts("/api/v1/accounts/sam"));

        // ⚠ Exactly two routes may answer without a bearer token, and no board route may.
        assert!(is_exempt_from_the_bearer_gate("/api/v1/session"));
        assert!(is_exempt_from_the_bearer_gate("/api/v1/whoami"));
        // ⚠ `/api/v1/accounts` is **not** exempt by path, and that is still right: once a
        // server is set up, only an admin may add an account. Its first-run carve-out is in
        // `serve.rs`, conditioned on there being no accounts yet, so it cannot outlive setup.
        for gated in
            ["/api/v1/accounts", "/api/v1/boards", "/api/v1/library", "/api/v1/blobs/abc"]
        {
            assert!(!is_exempt_from_the_bearer_gate(gated), "{gated} escaped the bearer gate");
        }
    }

    /// ⚠ The wrong answer here locks the owner out of their own development server, with a
    /// symptom that reads as a session bug rather than as a cookie flag.
    #[test]
    fn the_cookie_is_secure_everywhere_but_a_genuinely_local_connection() {
        assert!(!cookie(Some("abc"), false).contains("Secure"));
        assert!(cookie(Some("abc"), true).contains("; Secure"));
        for line in [cookie(Some("abc"), true), cookie(None, true)] {
            assert!(line.contains("HttpOnly"), "the cookie is readable by script");
            assert!(line.contains("SameSite=Strict"), "the cookie rides cross-site requests");
            assert!(line.contains("Path=/"), "sign-out cannot clear a cookie on another path");
        }
        assert!(cookie(None, true).contains("Max-Age=0"), "signing out does not clear the cookie");

        // ⚠ **The cookie must outlive the longest session, not the idle window.** A
        // `Max-Age` counts from the moment it is set and no request refreshes it, so
        // `SESSION_IDLE` here would delete the cookie twelve hours after signing in however
        // much it was used — the idle window would never once slide, while the server went on
        // refreshing a value the browser had thrown away. The assertion is on the number
        // because both are plausible-looking constants and only one of them works.
        assert!(
            cookie(Some("abc"), true).contains(&format!("Max-Age={}", SESSION_MAX.as_secs())),
            "the cookie expires on the idle clock, so a session in constant use still ends at 12h"
        );
    }

    #[test]
    fn a_session_cookie_is_found_beside_other_cookies() {
        let header = |raw: &str| {
            let mut headers = BTreeMap::new();
            headers.insert("cookie".to_owned(), raw.to_owned());
            headers
        };
        assert_eq!(session_cookie(&header("velm_session=abc")).as_deref(), Some("abc"));
        // ⚠ The one this got wrong first: a cookie with no `=` in it, before ours, must not
        // end the search. Browsers do send bare cookie names.
        assert_eq!(
            session_cookie(&header("consent; theme=dark; velm_session=abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(session_cookie(&header("theme=dark")), None);
        // ⚠ `read_head` keeps a header whose value would not decode as UTF-8 and empties the
        // value, so that `proxied()` cannot be switched off by sending invalid bytes. Here
        // that has to mean "no cookie", not a panic and not an empty session id.
        assert_eq!(session_cookie(&header("")), None);
        assert_eq!(session_cookie(&header("velm_session=")), None);
        assert_eq!(session_cookie(&BTreeMap::new()), None);
    }

    #[test]
    fn a_body_that_is_not_credentials_is_refused_rather_than_guessed_at() {
        assert!(credentials(b"not json").is_none());
        // ⚠ Missing fields default to empty rather than failing to parse — `serde(default)`,
        // for forward compatibility — so the emptiness has to be caught by the rules, and it
        // is: an empty username does not fold and an empty password is too short.
        let empty = credentials(b"{}").expect("an empty object should still parse");
        assert!(fold_username(&empty.username).is_err());
        assert!(check_password(&empty.password).is_err());
        let extra = credentials(br#"{"username":"sam","password":"another-long-passphrase","future":1}"#);
        assert_eq!(extra.expect("an unknown key should be ignored").username, "sam");
    }

    #[test]
    fn a_session_id_is_never_the_same_twice() {
        let ids: std::collections::BTreeSet<String> = (0..64).map(|_| new_session_id()).collect();
        assert_eq!(ids.len(), 64, "the session id generator repeats itself");
    }
}
