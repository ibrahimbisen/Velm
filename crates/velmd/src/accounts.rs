//! Accounts: who may sign in, and which boards they may see.
//!
//! ```text
//! GET    /api/v1/whoami       {"username":…,"admin":…}  or 401
//! POST   /api/v1/session      {"username","password"}   -> 204 + Set-Cookie
//! DELETE /api/v1/session                                -> 204 + a cleared cookie
//! POST   /api/v1/accounts     admin only, except the very first one and an invite code
//! GET    /api/v1/accounts     admin only: usernames, never hashes
//! POST   /api/v1/invites      admin only  -> {"code","expires","days"}
//! GET    /api/v1/invites      admin only: every code and the state it is in
//! DELETE /api/v1/invites/{code}                         -> 204
//! POST   /api/v1/boards/{id}/share             {"username"}  -> 204
//! DELETE /api/v1/boards/{id}/share/{username}                -> 204
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

use crate::serve::{
    Server, board_by_id, json_string, printable, proxied, respond_with, same_secret,
};
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

pub const PATH_INVITES: &str = "/api/v1/invites";

/// The prefix one invite code hangs off, for `DELETE /api/v1/invites/{code}`.
const PATH_INVITE_PREFIX: &str = "/api/v1/invites/";

/// The prefix and the two tails that spell the two share routes.
///
/// Sharing lives here rather than in [`crate::manage`] because everything it changes lives
/// here: the shared list is a field on [`Ownership`], the `shared` key is a field on
/// [`Record`], and [`Accounts::allowed`] is what reads it. `manage.rs` owns a board's *file*
/// and its *name*; this owns who may see it. The **path shape** is copied from
/// [`crate::manage::rename_target`], which is the closest precedent for a route hanging off
/// one board.
const PATH_BOARD_PREFIX: &str = "/api/v1/boards/";
const PATH_SHARE_SUFFIX: &str = "/share";
const PATH_SHARE_INFIX: &str = "/share/";

/// Whether this path is the invite route — `POST` to mint one, `GET` to list them.
pub fn is_invites(path: &str) -> bool {
    path == PATH_INVITES
}

/// The invite code in a revoke path, or `None` if this is not one.
///
/// Stricter than [`crate::manage::rename_target`], which does not refuse an id holding a `/`:
/// a code has a fixed alphabet, so a segment with a slash in it is not one. That is defence
/// in depth rather than the security boundary — [`fold_code`] refuses a `/` anyway, because
/// it is not in [`CODE_ALPHABET`] — and it keeps `DELETE /api/v1/invites/a/b` from being read
/// as a code called `a/b`.
pub fn revoke_target(path: &str) -> Option<&str> {
    let code = path.strip_prefix(PATH_INVITE_PREFIX)?;
    (!code.is_empty() && !code.contains('/')).then_some(code)
}

/// The board id in `POST /api/v1/boards/{id}/share`, or `None` if this is not one.
///
/// The id is compared against file stems that came out of a directory listing, never joined
/// onto a directory — the same rule [`crate::manage::rename_target`] states. A stem cannot
/// hold a `/`, so one that does is refused here rather than left to match nothing later.
pub fn share_target(path: &str) -> Option<&str> {
    let id = path.strip_prefix(PATH_BOARD_PREFIX)?.strip_suffix(PATH_SHARE_SUFFIX)?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

/// The board id and the username in `DELETE /api/v1/boards/{id}/share/{username}`.
///
/// ⚠ **The username is in the path rather than in a body, and that is a decision.** A
/// `DELETE` in this server carries no body — signing out does not, and neither does revoking
/// an invite code — so a body here would mean a second `content_length` and a second body
/// read inside `serve.rs`'s `DELETE` block, which exists precisely because those two routes
/// need neither. It is also the safe direction for CSRF: an HTML form can send only `GET` and
/// `POST`, so nothing forgeable by a form can reach a `DELETE` at all.
///
/// A username is letters, digits and `. - _` ([`fold_username`]), so it never needs escaping
/// in a path segment and a segment holding a `/` is not one.
pub fn unshare_target(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(PATH_BOARD_PREFIX)?;
    let (id, username) = rest.split_once(PATH_SHARE_INFIX)?;
    let usable = !id.is_empty()
        && !id.contains('/')
        && !username.is_empty()
        && !username.contains('/');
    usable.then_some((id, username))
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
/// ⚠ **`serve::respond_inner`'s reason table has to hold every status this file sends**, and
/// for two releases it did not. It maps a status to its reason phrase from a fixed list and
/// an unlisted number falls through to *"Internal Server Error"*, so a 403 went out as
/// `HTTP/1.1 403 Internal Server Error` — a status line that contradicts itself, from a
/// server telling a client what to fix. This doc used to say *"`respond`'s match gains three
/// arms"*; **only 415 and 429 landed**, and 403 and 409 were live on the wire, mislabelled,
/// the whole time. All four arms are in that table now.
///
/// The codes themselves were never the negotiable part. 400 does not mean *"you are not an
/// admin"*, 401 means *"authenticate"* to somebody who already has, and a client that
/// branches on the code — which is every client, since nobody parses a reason phrase — would
/// be told the wrong thing.
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
    /// Whether *Remember me* was ticked when this session was minted.
    ///
    /// It switches off the idle rule and nothing else. [`SESSION_MAX`] still applies, so a
    /// remembered session ends after thirty days however much it is used — which is what
    /// makes "you will sign in again eventually" true for every session rather than most of
    /// them.
    ///
    /// ⚠ **Stored on the session rather than read from the request each time.** The choice
    /// belongs to the sign-in it was made at: a later request from the same browser must not
    /// be able to extend a session that was not remembered, or the flag would be a thing an
    /// attacker holding a stolen cookie could simply assert.
    remember: bool,
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

/// How long a session survives without being used, when *Remember me* was not ticked.
///
/// Forty-eight hours: a person who signed in on a tablet on Friday is still signed in on
/// Sunday, and one who put the tablet on a shelf is not signed in next month. Sliding,
/// because the alternative — a hard expiry — signs somebody out in the middle of using the
/// thing, which is the moment they are least able to see why.
///
/// ⚠ **It was twelve hours, and twelve was wrong for the audience.** A household server is
/// used in bursts: somebody opens a board on Saturday, does not touch it on Sunday, and comes
/// back on Monday. At twelve hours that is a password prompt every single time, which trains
/// the household to pick a short password. Two days spans a weekend, which is the gap that
/// actually occurs.
const SESSION_IDLE: Duration = Duration::from_secs(48 * 60 * 60);

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

// ----- invite codes -------------------------------------------------------------------------

/// Crockford base32: the digits and the 22 letters that are not I, L, O or U.
///
/// The four letters left out are the four a person confuses with a digit when they read a
/// code down a telephone or type it on a phone keyboard. [`fold_code`] puts three of them
/// back where they can only have meant a digit.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// How many characters a code is.
///
/// Twelve from 32 symbols is **60 bits**, shown as `K7QM-3XPT-9WNZ`. With the cap of
/// [`MAX_OPEN_INVITES`] open at once, a guesser expects 2^60 / (2 × 32) ≈ 1.8 × 10^16
/// attempts — and every attempt is refused inside [`Accounts`] under the one accounts
/// `Mutex`, so guesses are serialised across the whole process. There is no offline attack on
/// this: the verifier is the file that already holds the boards.
const CODE_CHARS: usize = 12;

/// How many characters between the hyphens a person reads a code by.
const CODE_GROUP: usize = 4;

/// How long a code stays open. Fourteen days.
///
/// A code sitting unspent for ever in a file that gets backed up is a standing door. Two
/// weeks is longer than it takes somebody to answer a message and shorter than it takes them
/// to forget the message existed.
///
/// ⚠ **This runs on the wall clock, not on an [`Instant`].** A session dies with the process,
/// so a monotonic clock is right for it; an invite outlives a restart, so its age has to
/// survive one. The consequence: a clock stepped backwards makes a code live longer and one
/// stepped forwards kills it early. On a machine with NTP that is a step of seconds against a
/// window of two weeks.
const INVITE_LIFE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

// ⚠ **These two constants spell the same number and they are next to each other for that
// reason.** [`Change::Refused`] carries a `&'static str`, which cannot hold a formatted
// value, so the cap's number is typed twice. Kept adjacent so that changing one is an edit on
// the next line, and pinned by `the_cap_and_its_refusal_agree` so that a change to one alone
// fails the suite rather than shipping a sentence that lies about the rule.
/// How many codes may be open at once.
const MAX_OPEN_INVITES: usize = 32;

/// What an admin is told at that cap. Names [`MAX_OPEN_INVITES`]'s number.
const TOO_MANY_INVITES: &str =
    "there are already 32 codes waiting; revoke one before you make another\n";

/// The one sentence for every code this server will not accept.
///
/// ⚠ **Unknown, spent, revoked, expired and malformed all get this**, for the reason
/// [`Accounts::sign_in`] gives one sentence for a wrong password and a username that does not
/// exist. Telling somebody that a code *was* real but has expired is an oracle that says they
/// hit a live value.
///
/// ⚠ **It says to wait, and that is not politeness.** A wrong code spends this address's
/// failure budget, which is the **same** budget sign-in uses ([`MAX_FAILURES`] in
/// [`WINDOW`]), so ten wrong codes stop sign-in from that address for fifteen minutes too.
/// Telling the person to ask for another code would send them back to a server that is going
/// to refuse the new one as well.
const BAD_CODE: &str =
    "that invite code is not one this server is waiting for; check it and wait a moment \
     before you try again\n";

/// One invite code, as replayed from the log.
///
/// ⚠ **No `#[derive(Debug)]`, and the redaction below is written by hand.** The code is a
/// bearer credential: whoever holds it gets an account on this server. It joins [`Account`]'s
/// pattern for the same reason — see the module header on feedback 34's four latent derives.
///
/// The code is stored **in the clear**, and the argument is worth writing down because a
/// reviewer will challenge it. `accounts.json` lives inside `--data`, beside the `.vellum`
/// files, so **anybody who can read that file already has the boards**. Hashing the code
/// would defend against a reader of the file who does not have the directory, and there is no
/// such reader; the same file already holds the Argon2 hashes, which are what an offline
/// cracker actually wants, and [`open_for_append`] sets `0o600` for exactly that reason. What
/// plaintext buys is real for a household: an admin can re-read a code they minted and lost,
/// and revoke can name the code itself rather than needing a second, non-secret id beside it.
/// `blake3` is already a dependency of this crate and is **deliberately unused here**, named
/// so the next reader does not conclude it was overlooked.
#[derive(Clone)]
struct Invite {
    /// Canonical: uppercase, ungrouped, [`CODE_CHARS`] long. The hyphens are display only.
    code: String,
    /// The username of the admin who minted it.
    by: String,
    /// Seconds since the epoch. Expiry is measured from here.
    at: u64,
    /// The account this code made, once it has made one. A code is spent once.
    used_by: Option<String>,
    used_at: Option<u64>,
    revoked_at: Option<u64>,
}

impl std::fmt::Debug for Invite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Everything except the code, which is the whole secret. Replaced by a fixed word
        // rather than by its length, which is itself a fact somebody chose.
        f.debug_struct("Invite")
            .field("code", &"<redacted>")
            .field("by", &self.by)
            .field("at", &self.at)
            .field("used_by", &self.used_by)
            .field("used_at", &self.used_at)
            .field("revoked_at", &self.revoked_at)
            .finish()
    }
}

/// A fresh code: [`CODE_CHARS`] symbols from the operating system's own generator.
fn new_invite_code() -> String {
    let mut bytes = [0u8; CODE_CHARS];
    // ⚠ `OsRng` and nothing else, for the reason [`new_session_id`] gives: a code is a bearer
    // credential, and a userspace PRNG is a seed somebody has to have got right.
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(CODE_CHARS);
    for byte in bytes {
        // 256 divides by 32 exactly, so masking to five bits is uniform. No rejection loop
        // and no modulo bias: `byte % 26` would make some symbols 1.2 times as likely.
        out.push(CODE_ALPHABET[(byte & 0x1F) as usize] as char);
    }
    out
}

/// A code as a person typed it, reduced to the one spelling this file stores and compares.
///
/// Returns `None` for anything that is not a code, with no reason attached: every refusal on
/// this route is [`BAD_CODE`], so a reason string here would be a second sentence nobody ever
/// sends. Grouping, case and spaces are all forgiven, because a person reads a code off a
/// message and types it into a phone.
fn fold_code(raw: &str) -> Option<String> {
    let mut folded = String::with_capacity(CODE_CHARS);
    for ch in raw.chars() {
        if ch == '-' || ch.is_ascii_whitespace() {
            continue;
        }
        // ⚠ Before any `as u8`. A non-ASCII character truncates to a byte that can land
        // inside the alphabet: `'İ' as u8` is not `'İ'`.
        if !ch.is_ascii() {
            return None;
        }
        let ch = match ch.to_ascii_uppercase() {
            // Crockford's own read aliases. The alphabet leaves these out; a person who typed
            // one meant the digit beside it, and refusing costs a support message.
            //
            // `U` is **not** aliased. It is out of the alphabet on purpose and there is no
            // digit it resembles.
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        };
        if !CODE_ALPHABET.contains(&(ch as u8)) {
            return None;
        }
        if folded.len() == CODE_CHARS {
            return None;
        }
        folded.push(ch);
    }
    (folded.len() == CODE_CHARS).then_some(folded)
}

/// A stored code, hyphenated for reading aloud: `K7QM-3XPT-9WNZ`.
fn grouped(code: &str) -> String {
    let mut out = String::with_capacity(code.len() + code.len() / CODE_GROUP);
    for (index, ch) in code.chars().enumerate() {
        if index > 0 && index % CODE_GROUP == 0 {
            out.push('-');
        }
        out.push(ch);
    }
    out
}

/// Whether this code can still be spent. See [`INVITE_LIFE`] on the clock it uses.
fn invite_is_open(invite: &Invite, now: u64) -> bool {
    invite.used_by.is_none()
        && invite.revoked_at.is_none()
        && now < invite.at.saturating_add(INVITE_LIFE.as_secs())
}

/// The word the admin list shows for one code.
///
/// ⚠ Expiry is evaluated **here and at redemption, never at replay**: "expired" is a function
/// of now, and replay happens at boot.
fn invite_state(invite: &Invite, now: u64) -> &'static str {
    if invite.used_by.is_some() {
        "used"
    } else if invite.revoked_at.is_some() {
        "revoked"
    } else if invite_is_open(invite, now) {
        "open"
    } else {
        "expired"
    }
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
    /// Canonical invite code to invite, spent and revoked ones included.
    ///
    /// ⚠ **This is the only unbounded map on this struct, and it is said out loud rather
    /// than discovered.** `sessions` is capped at [`MAX_SESSIONS`], and `failures` and
    /// `attempts` at [`MAX_TRACKED`]. Spent and expired invites are never compacted out of an
    /// append-only file, so this grows by one entry for every code ever minted here and is
    /// rebuilt in full on every restart. [`MAX_OPEN_INVITES`] caps how many may be *open*, not
    /// how many may exist. Only an admin can add to it, and a household mints a few dozen
    /// codes in a lifetime, so the bound is the admin rather than a number.
    invites: BTreeMap<String, Invite>,
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
            // A count, never the contents: the map's **key** is an invite code.
            .field("invites", &self.invites.len())
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
/// ⚠ No `#[derive(Debug)]`: `hash` is on it, and `code` joined it.
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
    /// An invite code, canonical and ungrouped.
    ///
    /// On an `invite` or an `invite-revoked` record it is the code itself. On an `account`
    /// record it is the code that was **spent** to make that account — see
    /// [`Accounts::apply`] on why spending has no record of its own.
    #[serde(skip_serializing_if = "String::is_empty")]
    code: String,
    /// The username of the admin who minted an invite code.
    #[serde(skip_serializing_if = "String::is_empty")]
    by: String,
    /// Seconds since the epoch, for a person reading the file. ⚠ **`at` on an `invite`
    /// record is the one exception to the line below: [`INVITE_LIFE`] is measured from it.**
    ///
    /// ⚠ Seconds, and said out loud: `library_api::TrashedBoard::at` carries the same unit
    /// beside a sibling in milliseconds, and CLAUDE.md records `unix_now() * 1_000` — a clock
    /// that ticks once a second — costing an animation its whole existence. It used to decide
    /// nothing at all; an invite's expiry now reads it, so a wrong unit here would make every
    /// code either immortal or born dead.
    at: u64,
}

const KIND_ACCOUNT: &str = "account";
const KIND_ACCOUNT_REMOVED: &str = "account-removed";
const KIND_BOARD: &str = "board";
const KIND_INVITE: &str = "invite";
const KIND_INVITE_REVOKED: &str = "invite-revoked";

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
            invites: BTreeMap::new(),
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
                let username = record.username;
                let at = record.at;
                self.accounts.insert(
                    username.clone(),
                    Account {
                        username: username.clone(),
                        hash: record.hash,
                        admin: record.admin,
                    },
                );
                // ⚠ **There is no `invite-used` record. Spending is carried by the `account`
                // record itself**, and that is the decision that matters most in this format:
                // redemption is one append, one line, one `sync_all`. Two records would have
                // a window between them, and a crash in that window would leave either an
                // account with a live code — single use broken — or a spent code with no
                // account. A torn write loses both together, which is the recoverable state.
                if !record.code.is_empty() {
                    let code = record.code;
                    // `or_insert_with`, not `get_mut`. An account record naming a code whose
                    // `invite` line has not been seen yet — a reordered or hand-edited file —
                    // would otherwise leave the code open, which is the unsafe direction. A
                    // placeholder marked spent closes it, and the real `invite` line arriving
                    // later hits the `or_insert_with` below and is ignored.
                    let invite = self.invites.entry(code.clone()).or_insert_with(|| Invite {
                        code,
                        by: String::new(),
                        at,
                        used_by: None,
                        used_at: None,
                        revoked_at: None,
                    });
                    // First write wins here, unlike every other key in this replay: a code is
                    // spent once, and a second account naming it is a hand-edit rather than a
                    // state change.
                    if invite.used_by.is_none() {
                        invite.used_by = Some(username);
                        invite.used_at = Some(at);
                    }
                }
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
            KIND_INVITE => {
                if record.code.is_empty() {
                    return;
                }
                let code = record.code;
                let (by, at) = (record.by, record.at);
                // `or_insert_with`, not `insert`: a duplicate `invite` line — a hand-edit, two
                // files concatenated — must not wipe the used or revoked state a later line
                // already recorded.
                self.invites.entry(code.clone()).or_insert_with(|| Invite {
                    code,
                    by,
                    at,
                    used_by: None,
                    used_at: None,
                    revoked_at: None,
                });
            }
            KIND_INVITE_REVOKED => {
                if let Some(invite) = self.invites.get_mut(&record.code) {
                    invite.revoked_at = Some(record.at);
                }
            }
            // ⚠ **`KIND_ACCOUNT_REMOVED` touches no invite, and that is deliberate.** It is
            // handled above and is named again here because this is where somebody will
            // reach to "tidy up": removing the account a code created must not bring the code
            // back. `removing_an_invited_account_does_not_bring_its_code_back` pins it.
            //
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
    fn sign_in(
        &mut self,
        raw_username: &str,
        password: &str,
        wire: &Wire,
        remember: bool,
    ) -> Outcome {
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
            remember,
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

    /// The stored code equal to the presented one **and still spendable**, or nothing.
    ///
    /// ⚠ **Compared with [`same_secret`], never with `BTreeMap::get`**, for the reason
    /// [`Accounts::identity_of`] gives: a map lookup on a client-presented secret compares
    /// keys byte by byte with an early return, which is the timing oracle spelled with a
    /// container instead of a `==`. Every code is [`CODE_CHARS`] long, so `same_secret`'s
    /// honest length check leaks nothing. A miss scans the whole map; a hit stops at the
    /// matching key, which leaks that code's sort position to somebody who already holds it.
    ///
    /// `presented` must already be folded. `now` is wall-clock seconds — see [`INVITE_LIFE`].
    fn open_invite(&self, presented: &str, now: u64) -> Option<String> {
        let held = self.invites.keys().find(|held| same_secret(held.as_str(), presented)).cloned()?;
        let invite = self.invites.get(&held)?;
        invite_is_open(invite, now).then_some(held)
    }

    /// How many codes are still spendable, for [`MAX_OPEN_INVITES`].
    fn open_invites(&self, now: u64) -> usize {
        self.invites.values().filter(|invite| invite_is_open(invite, now)).count()
    }
}

/// Whether a session has run out either way. See [`SESSION_IDLE`] and [`SESSION_MAX`].
///
/// ⚠ **`Instant`, not `SystemTime`.** Sessions live in memory and die with the process, so
/// there is nothing for a wall clock to be right about — and a wall clock that jumps (NTP
/// stepping a server that has just booted, a laptop waking in another timezone) would sign
/// everybody out for a reason nobody could ever reconstruct. `Instant` is monotonic and
/// cannot.
/// ⚠ **[`SESSION_MAX`] is checked for every session, remembered or not, and that ordering is
/// the whole safety of the feature.** *Remember me* switches off the idle rule alone. A
/// session that never expired at all would be a password that never changes, held in a cookie
/// on a device that can be lost, and thirty days is the backstop that stops it becoming one.
fn expired(session: &Session, now: Instant) -> bool {
    if now.duration_since(session.created) >= SESSION_MAX {
        return true;
    }
    !session.remember && now.duration_since(session.last_used) >= SESSION_IDLE
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
/// ⚠ **No `#[derive(Debug)]`, and that stopped being a nicety the day [`Change::Minted`]
/// landed.** This doc used to say *"nothing here carries a secret today"*; `Minted` carries a
/// live invite code, which is a bearer credential for an account on this server. The missing
/// derive is now the mechanism rather than the precaution, and adding one would put a code
/// into the first `{change:?}` somebody writes while chasing a 403.
enum Change {
    Made(Identity),
    /// The account is not there, or the code is not open. See [`Accounts::revoke_invite`] on
    /// why revoking answers this whatever the code was.
    Gone,
    /// A board's shared list is now what the request asked for, whether or not a record had
    /// to be written for it.
    Recorded,
    /// ⚠ Carries a live invite code. See the note on this enum.
    Minted {
        /// Grouped for reading: `K7QM-3XPT-9WNZ`.
        code: String,
        /// Wall-clock seconds. See [`INVITE_LIFE`].
        expires: u64,
    },
    Taken,
    /// Signed in, and not allowed to do this. ⚠ **It carries its own sentence**, because
    /// four different rules answer 403 now — only an admin may create an account on a server
    /// that has one, may mint a code, may revoke one, and only a board's owner or an admin
    /// may share it — and one shared sentence would tell three of them the wrong thing.
    NotAllowed(&'static str),
    /// The invite code offered is not one this server will accept. See [`BAD_CODE`].
    BadCode,
    /// This address has spent its failure budget. See [`MAX_FAILURES`].
    Throttled,
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
    /// Once one account exists this needs an admin, for ever, **or an invite code**.
    ///
    /// # The three ways this succeeds, checked in this order
    ///
    /// 1. **Setup.** [`Accounts::setup_open`] is true. No caller and no code are needed, any
    ///    `code` in the body is ignored and never spent, and the account is forced `admin`.
    /// 2. **An admin.** `by` is an admin. `admin` comes from the body. A code is ignored.
    /// 3. **A code.** No admin caller, and `code` names an invite that is open. The account
    ///    is forced **`admin: false`** whatever the body asked for.
    ///
    /// ⚠ **An invite cannot exist while setup is open, and that is provable rather than
    /// checked.** `setup_open` is true only when the file was absent or zero bytes; minting
    /// appends a record, which makes it non-empty; and minting needs an [`Identity`], which
    /// needs an account, which needs an `account` record. So case 1 can never be silently
    /// consuming a real code. It is written as an *ordering* rather than as an assertion so
    /// that a future change which does let an invite exist here degrades to "the code was
    /// ignored" rather than to "the code was silently spent".
    ///
    /// ⚠ **A file that cannot be read refuses the whole request, code or no code.** An
    /// unreadable log is a log whose invites cannot be verified, and a gate degraded to its
    /// default is a gate that is open. That is the first line of this function and it is the
    /// same rule `sign_in` follows.
    ///
    /// # Where the expensive work sits
    ///
    /// The 19 MiB Argon2 hash is **behind a valid code**. A wrong-code flood costs a body
    /// parse, a lock and a constant-time scan. A valid code with a too-short password is
    /// refused before the hash and **the code is not spent**, so the person retries with a
    /// longer one; the same is true of a username already taken.
    fn create_account(
        &mut self,
        raw_username: &str,
        password: &str,
        admin: bool,
        code: &str,
        by: Option<&Identity>,
        wire: &Wire,
    ) -> Change {
        // ⚠ Seven parameters counting `self`, which is **exactly** clippy's
        // `too_many_arguments` threshold: the lint fires above seven, so this passes with
        // zero headroom. An eighth needs a struct, not another parameter.
        if self.unreadable {
            return Change::Broken;
        }
        let first = self.setup_open();
        // The canonical code this request spent, once it is known to be spendable.
        let mut redeemed: Option<String> = None;
        if !first && !by.is_some_and(|caller| caller.admin) {
            if code.is_empty() {
                return Change::NotAllowed(
                    "only an admin may create an account on a server that has one\n",
                );
            }
            // ⚠ **Malformed is refused before the limiter is touched**, the rule `sign_in`
            // follows for a username that will not fold: a code with the wrong number of
            // characters is a client bug or a typo rather than a guess, and spending the
            // address's budget on it would let a fumbling family member lock out sign-in.
            let Some(folded) = fold_code(code) else {
                return Change::BadCode;
            };
            let now = Instant::now();
            if let Some(address) = &wire.address
                && self.address_is_over_budget(address, now)
            {
                return Change::Throttled;
            }
            let Some(held) = self.open_invite(&folded, now_seconds()) else {
                if let Some(address) = &wire.address {
                    self.record_failure(address.clone(), now);
                }
                return Change::BadCode;
            };
            // ⚠ **A successful redemption does not clear the address's record**, unlike a
            // successful sign-in. Clearing on success would let somebody holding one valid
            // code reset their guessing budget at will.
            redeemed = Some(held);
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
        // is what the admin making it asked for; and an account made with a code never is.
        //
        // ⚠ `self.accounts.is_empty()` rather than `first` alone. They are the same today,
        // and they come apart the moment anything other than a signed-in admin can write to
        // this log — a file holding only board records, or only records from a newer build,
        // loads with no accounts and setup closed. An account made there must still be an
        // admin, or the server has accounts and nobody who can manage them, and setup does
        // not reopen.
        let admin = if redeemed.is_some() { false } else { self.accounts.is_empty() || admin };

        let record = Record {
            v: VERSION,
            kind: KIND_ACCOUNT.to_owned(),
            username: username.clone(),
            hash: hash.clone(),
            admin,
            // Empty unless a code was spent, and `skip_serializing_if` keeps it out of the
            // line entirely when it is. This one field is what makes redemption one record.
            code: redeemed.clone().unwrap_or_default(),
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
        // The same fold `apply` performs on replay, so a running server and a restarted one
        // agree about which codes are spent without the fact being stored twice.
        if let Some(held) = &redeemed {
            let at = record.at;
            let invite = self.invites.entry(held.clone()).or_insert_with(|| Invite {
                code: held.clone(),
                by: String::new(),
                at,
                used_by: None,
                used_at: None,
                revoked_at: None,
            });
            if invite.used_by.is_none() {
                invite.used_by = Some(username.clone());
                invite.used_at = Some(at);
            }
        }
        Change::Made(Identity { username, admin })
    }

    /// Mint one invite code.
    ///
    /// ⚠ **The bearer token cannot reach this, and that is a decision rather than an
    /// oversight.** `Caller::Token` is not an [`Identity`], and the handler resolves its
    /// caller through [`identity`], which reads the session cookie alone. Letting the token
    /// mint would let an invite record exist on a server with no accounts, which closes setup
    /// with `accounts` empty — and the first account made after that would be whatever the
    /// body asked for, would become the founder, and would own every board with no ownership
    /// record. What it costs is stated: an operator who has the token and has lost the admin
    /// password cannot make accounts. That is already true today and is not a regression.
    fn create_invite(&mut self, by: &Identity) -> Change {
        if self.unreadable {
            return Change::Broken;
        }
        if !by.admin {
            return Change::NotAllowed("only an admin may make an invite code\n");
        }
        let at = now_seconds();
        if at == 0 {
            // ⚠ [`now_seconds`] answers 0 when the clock is before the epoch, and
            // `0 + INVITE_LIFE` is a date in 1970 — so every code would be born expired with
            // no sign of why. A 400 blaming the client is slightly wrong for a server fault
            // and is cheaper than a status arm for a state that needs a clock set before 1970.
            return Change::Refused("this server's clock is not set, so a code cannot be dated\n");
        }
        if self.open_invites(at) >= MAX_OPEN_INVITES {
            return Change::Refused(TOO_MANY_INVITES);
        }
        // A plain map lookup, and that is correct here: the value is one this process just
        // generated rather than one a client presented, so there is no remote channel to
        // leak through. Four tries against a 2^60 space is a formality; refusing after them
        // is better than a loop whose exit depends on a generator.
        let mut code = String::new();
        for _ in 0..4 {
            let candidate = new_invite_code();
            if !self.invites.contains_key(&candidate) {
                code = candidate;
                break;
            }
        }
        if code.is_empty() {
            return Change::Broken;
        }
        let record = Record {
            v: VERSION,
            kind: KIND_INVITE.to_owned(),
            code: code.clone(),
            by: by.username.clone(),
            at,
            ..Record::default()
        };
        if let Err(error) = self.append(&record) {
            // The path and the reason, never the record: it holds the code.
            eprintln!(
                "velmd: could not record an invite code in {}: {}",
                printable(&self.log.display().to_string()),
                printable(&error.to_string())
            );
            return Change::Broken;
        }
        self.invites.insert(code.clone(), Invite {
            code: code.clone(),
            by: by.username.clone(),
            at,
            used_by: None,
            used_at: None,
            revoked_at: None,
        });
        Change::Minted {
            code: grouped(&code),
            expires: at.saturating_add(INVITE_LIFE.as_secs()),
        }
    }

    /// Revoke one invite code.
    ///
    /// ⚠ **[`Change::Gone`] in every reachable case, like signing out.** The desired state is
    /// reached whether the code was open, spent, revoked already or never a code at all, and
    /// the admin surface re-fetches the list, which is where the truth shows. A spent code is
    /// **not** revoked and writes nothing: spent is spent, and a revoke record after a use
    /// would be noise in a file that is never compacted.
    fn revoke_invite(&mut self, raw_code: &str, by: &Identity) -> Change {
        if self.unreadable {
            return Change::Broken;
        }
        if !by.admin {
            return Change::NotAllowed("only an admin may revoke an invite code\n");
        }
        let Some(code) = fold_code(raw_code) else {
            return Change::Gone;
        };
        let at = now_seconds();
        let Some(held) = self.open_invite(&code, at) else {
            return Change::Gone;
        };
        let record = Record {
            v: VERSION,
            kind: KIND_INVITE_REVOKED.to_owned(),
            code: held.clone(),
            at,
            ..Record::default()
        };
        if let Err(error) = self.append(&record) {
            eprintln!(
                "velmd: could not record an invite change in {}: {}",
                printable(&self.log.display().to_string()),
                printable(&error.to_string())
            );
            return Change::Broken;
        }
        if let Some(invite) = self.invites.get_mut(&held) {
            invite.revoked_at = Some(at);
        }
        Change::Gone
    }

    /// Add somebody to a board's shared list, or take them off it.
    ///
    /// # ⚠ Why this exists at all
    ///
    /// [`Accounts::allowed`] has read a per-board shared list since accounts landed,
    /// [`Ownership`] has held one, and [`Record`] has serialised one — and **no route ever
    /// put a name in it**. So an invited family member signed in and saw an empty board list,
    /// which makes an invite code useless on its own. This is that route's rule engine.
    ///
    /// # The rules
    ///
    /// - **Only the board's owner, or an admin.** Not every signed-in account, and *not*
    ///   somebody the board was merely shared with: sharing is not a transferable power, or
    ///   one share puts the board one hop from everybody.
    /// - **The name must be an account here.** Refused with a sentence rather than accepted
    ///   silently, because a share that names nobody looks exactly like a share that worked.
    ///   It tells the caller that a username exists; they are the board's owner or an admin,
    ///   and an admin can already list every account.
    /// - **A board with no ownership record belongs to the founder**, which is
    ///   [`Accounts::owner_of`]'s existing rule and covers every board that was in the data
    ///   directory before accounts were switched on. The founder can therefore share those.
    /// - ⚠ **The record keeps the owner it already named**, resolved only when there is none
    ///   to keep. `owner_of`'s fall back to the founder is *derived*, never written down: an
    ///   admin sharing a board whose owner has been removed must not materialise themselves
    ///   as its owner, or an account made again with that name never gets the board back.
    /// - **Append-only, and the last record wins on replay**, which is how [`Accounts::apply`]
    ///   already reads `KIND_BOARD`. Nothing is rewritten.
    /// - **Nothing is written when the list is already what was asked for**, so sharing twice
    ///   costs one line rather than two, and unsharing a name that is not there costs none.
    ///
    /// ⚠ The board's **file** is not checked to exist, and that is the lock rule rather than
    /// laziness: finding a board needs `Server::boards`, and this runs under the accounts
    /// lock, which must never be held at the same time. `serve.rs` calls
    /// [`may_see`] before this, which answers 404 for a board this caller cannot see and is
    /// the check a stranger meets.
    ///
    /// ⚠ The bearer token cannot reach this either, for [`Accounts::create_invite`]'s reason:
    /// the handler resolves an [`Identity`] from the session cookie, and `Caller::Token` is
    /// not one.
    fn set_share(&mut self, board: &str, raw_username: &str, by: &Identity, add: bool) -> Change {
        if self.unreadable {
            return Change::Broken;
        }
        if board.is_empty() {
            return Change::Refused("that is not a board on this server\n");
        }
        // Resolved, so that the founder's fallback applies to a board with no record.
        let resolved = self.owner_of(board).map(str::to_owned);
        if resolved.as_deref() != Some(by.username.as_str()) && !by.admin {
            return Change::NotAllowed("only a board's owner, or an admin, may share it\n");
        }
        let Ok(username) = fold_username(raw_username) else {
            return Change::Refused("that is not a username on this server\n");
        };
        if !self.accounts.contains_key(&username) {
            return Change::Refused("there is no account with that name on this server\n");
        }
        let recorded = self.boards.get(board).cloned();
        // ⚠ The stored owner verbatim when there is one. See the note above.
        let owner = match &recorded {
            Some(record) => record.owner.clone(),
            None => match resolved {
                Some(founder) => founder,
                // No accounts and no founder. Unreachable through a session, since a session
                // needs an account; refused rather than written as a record with no owner,
                // which `apply` would skip on the next restart.
                None => return Change::Refused("that is not a board on this server\n"),
            },
        };
        let mut shared = recorded.map(|record| record.shared).unwrap_or_default();
        if username == owner {
            // The owner already sees it. Nothing to record either way.
            return Change::Recorded;
        }
        if shared.contains(&username) == add {
            return Change::Recorded;
        }
        if add {
            shared.push(username);
            // Sorted so that the same set of people is the same line whatever order they
            // were added in, which is what makes a file diff readable.
            shared.sort();
        } else {
            shared.retain(|held| held != &username);
        }
        let record = Record {
            v: VERSION,
            kind: KIND_BOARD.to_owned(),
            board: board.to_owned(),
            owner: owner.clone(),
            shared: shared.clone(),
            at: now_seconds(),
            ..Record::default()
        };
        if let Err(error) = self.append(&record) {
            eprintln!(
                "velmd: could not record who may see {} in {}: {}",
                printable(board),
                printable(&self.log.display().to_string()),
                printable(&error.to_string())
            );
            return Change::Broken;
        }
        self.boards.insert(board.to_owned(), Ownership { owner, shared });
        Change::Recorded
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
            return Change::NotAllowed("only an admin may remove an account\n");
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
    /// Who owns this board and who it is shared with, if this account is entitled to know.
    ///
    /// See [`sharing_of`], the route-facing wrapper, for why this is not [`Self::allowed`]
    /// and why it is not folded together with [`Self::set_share`]'s identical-looking rule.
    fn sharing_seen_by(&self, board: &str, by: &Identity) -> Option<(String, Vec<String>)> {
        let owner = self.owner_of(board)?.to_owned();
        if owner != by.username && !by.admin {
            return None;
        }
        Some((owner, self.boards.get(board).map(|r| r.shared.clone()).unwrap_or_default()))
    }

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

/// Who owns this board and who it is shared with, for a caller entitled to know.
///
/// `None` means *"do not tell this caller"*, and it is the answer in three different cases
/// that all deserve the same silence: the caller is neither the owner nor an admin, the
/// caller is the bearer token rather than an account, or there is no owner to name at all.
/// The board list simply omits both keys, and `web/boards.js` already treats an absent
/// `shared` as "this server does not say" rather than as "nobody" — the two readings differ
/// and the client was written for the distinction before this function existed.
///
/// ⚠ **The rule is [`Accounts::set_share`]'s rule, restated deliberately rather than shared.**
/// Both are "the owner, or an admin", and it is tempting to factor them into one predicate.
/// They answer different questions: this one guards *knowing who has access*, that one guards
/// *changing it*, and a later decision to let anyone a board is shared with see the other
/// names would move this one and must not move that one. Two call sites, two sentences.
///
/// ⚠ **Not [`Accounts::allowed`], which is a weaker gate.** Everyone a board is shared with
/// passes `allowed`, so building the list from it would tell each of them the names of all the
/// others. That is a disclosure the contract never promised.
/// The rule itself lives on [`Accounts`] so a test can reach it without an HTTP server.
pub fn sharing_of(server: &Server, board: &str, caller: Option<&Caller>) -> Option<(String, Vec<String>)> {
    let Some(Caller::Account(identity)) = caller else { return None };
    server.accounts.lock().ok()?.sharing_seen_by(board, identity)
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
    /// Only read when an admin is creating somebody else. The first account forces it true,
    /// and an account made with an invite code forces it false.
    admin: bool,
    /// The invite code this request offers, or `""`.
    ///
    /// Optional through the container's `#[serde(default)]`, so a client written before
    /// invite codes existed sends the same body it always did.
    code: String,
    /// Whether *Remember me* was ticked. Only read by [`Accounts::sign_in`].
    ///
    /// Defaults to `false` through the container's `#[serde(default)]`, so a client written
    /// before this existed gets the ordinary idle rule rather than the long one. That is the
    /// right direction for a default: forgetting to send it shortens a session, never
    /// lengthens one.
    remember: bool,
}

/// The body of a share request: the person to add or to take off.
///
/// ⚠ No `#[derive(Debug)]` by reflex here either. It holds no secret today, and that
/// sentence is exactly what [`Change`]'s doc used to say.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Sharing {
    username: String,
}

/// Read a share body, or say it is not one.
fn sharing(body: &[u8]) -> Option<Sharing> {
    serde_json::from_slice(body).ok()
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
        accounts.sign_in(&creds.username, &creds.password, &wire, creds.remember)
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
            let body = signed_out_body(setup_is_open(server));
            respond_with(stream, 401, "application/json", body.as_bytes(), origin, &[])
        }
    }
}

/// `POST /api/v1/accounts` — make an account.
///
/// Unauthenticated in two cases and no others: while this server has never been set up, in
/// which case the account it makes is the owner, and when the body carries an invite code,
/// which **is** the credential the request presents. See [`Accounts::create_account`] for how
/// the race between two setup requests is closed and for the order the three ways are checked
/// in.
///
/// ⚠ **`Content-Type: application/json` is required, and it is checked first.** That is the
/// whole CSRF defence on this route — see [`sent_as_json`], where the `enctype="text/plain"`
/// form that founds a server from any page the owner visits is written out. It matters more
/// now, not less: a forged request that could carry a code would be a forged account.
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
    let wire = wire_of(headers, stream, server.config.behind_https);

    let change = {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.create_account(
            &creds.username,
            &creds.password,
            creds.admin,
            &creds.code,
            caller.as_ref(),
            &wire,
        )
    };
    answer_change(stream, change, origin)
}

/// `POST /api/v1/invites` — an admin mints one code.
///
/// ⚠ **The body is required to be JSON and is then not read.** It may be `{}`. There is no
/// `days` knob and no `admin` knob: a code always makes an ordinary account with a fixed
/// life, and an admin who wants a second admin creates that account directly through
/// `POST /api/v1/accounts`, which already honours `admin: true`. The content type is checked
/// because it is the CSRF defence — see [`sent_as_json`] — and a route that mints a
/// credential is exactly one worth forging.
pub fn create_invite(
    server: &Server,
    headers: &BTreeMap<String, String>,
    _body: &[u8],
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    if !sent_as_json(headers) {
        return respond_with(stream, 415, "text/plain", b"send application/json\n", origin, &[]);
    }
    // Before the lock, and `Caller::Token` cannot produce one — see
    // [`Accounts::create_invite`] on why the bearer token may not mint.
    let Some(caller) = identity(server, headers) else {
        return respond_with(stream, 401, "text/plain", b"not signed in\n", origin, &[]);
    };
    let change = {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.create_invite(&caller)
    };
    if matches!(change, Change::Minted { .. }) {
        // ⚠ The minter's username, and **never the code**. A code in a log line is a code in
        // whatever collects that log.
        eprintln!("velmd: {} made an invite code", printable(&caller.username));
    }
    answer_change(stream, change, origin)
}

/// `GET /api/v1/invites` — an admin lists them.
pub fn list_invites(
    server: &Server,
    headers: &BTreeMap<String, String>,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let Some(caller) = identity(server, headers) else {
        return respond_with(stream, 401, "text/plain", b"not signed in\n", origin, &[]);
    };
    if !caller.admin {
        return respond_with(
            stream,
            FORBIDDEN,
            "text/plain",
            b"only an admin may list invite codes\n",
            origin,
            &[],
        );
    }
    let body = {
        let accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        invite_rows(&accounts, now_seconds())
    };
    respond_with(stream, 200, "application/json", body.as_bytes(), origin, &[])
}

/// `DELETE /api/v1/invites/{code}` — an admin revokes one.
///
/// ⚠ **The code lands in the request line, so a reverse proxy's access log records it.** That
/// is acceptable and is worth saying out loud: the effect of the request is to kill the value
/// it names, so what the proxy writes is dead by the time the line is written. No other route
/// carries a code in a URL.
///
/// No [`sent_as_json`] check, because a `DELETE` carries no body — the same reasoning
/// [`sign_out`] gives. An HTML form can send only `GET` and `POST`, so nothing a page can
/// forge reaches a `DELETE` at all, and `SameSite=Strict` is the second layer.
pub fn revoke_invite(
    server: &Server,
    headers: &BTreeMap<String, String>,
    code: &str,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let Some(caller) = identity(server, headers) else {
        return respond_with(stream, 401, "text/plain", b"not signed in\n", origin, &[]);
    };
    let change = {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.revoke_invite(code, &caller)
    };
    answer_change(stream, change, origin)
}

/// `POST /api/v1/boards/{id}/share` — let one more account see this board.
///
/// The board id arrives already percent-decoded from `serve.rs`, for the reason the snapshot
/// route gives: a board id is a file stem and real ones have spaces in them.
pub fn share_board(
    server: &Server,
    headers: &BTreeMap<String, String>,
    board: &str,
    body: &[u8],
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    if !sent_as_json(headers) {
        return respond_with(stream, 415, "text/plain", b"send application/json\n", origin, &[]);
    }
    let Some(request) = sharing(body) else {
        return respond_with(stream, 400, "text/plain", b"send {\"username\"}\n", origin, &[]);
    };
    let Some(caller) = identity(server, headers) else {
        return respond_with(stream, 401, "text/plain", b"not signed in\n", origin, &[]);
    };
    // ⚠ **Sharing asks whether the board exists; unsharing does not, and the asymmetry is
    // deliberate.** `may_see` in `serve.rs` answers a question about *permission*, and
    // `owner_of` gives every board with no ownership record to the founder — so the founder
    // "may see" a board that was never created, and a typed board id that matches nothing
    // reached this function and appended a perfectly good record for a board nobody can open.
    // The person who typed it was told 204 and the account they meant to invite saw nothing.
    //
    // 404 with the same sentence every read route uses, so a stranger cannot learn that a
    // board exists by being told they may not share it.
    if board_by_id(&server.config.data, board).is_none() {
        return respond_with(stream, 404, "text/plain", b"no such board\n", origin, &[]);
    }
    let change = {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.set_share(board, &request.username, &caller, true)
    };
    answer_change(stream, change, origin)
}

/// `DELETE /api/v1/boards/{id}/share/{username}` — take one account off a board.
///
/// The username is a path segment rather than a body — see [`unshare_target`] for why. No
/// [`sent_as_json`] check, for [`revoke_invite`]'s reason.
pub fn unshare_board(
    server: &Server,
    headers: &BTreeMap<String, String>,
    board: &str,
    username: &str,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let Some(caller) = identity(server, headers) else {
        return respond_with(stream, 401, "text/plain", b"not signed in\n", origin, &[]);
    };
    let change = {
        let mut accounts =
            server.accounts.lock().map_err(|_| anyhow::anyhow!("accounts lock poisoned"))?;
        accounts.set_share(board, username, &caller, false)
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

/// The invite list as JSON. Pure, so a test can assert the wire shape without a socket.
///
/// ⚠ Built by hand from named fields, exactly like [`rows`], so a field added to [`Invite`]
/// cannot join the answer by accident. `state` is one of `open`, `used`, `revoked` or
/// `expired`; `used_by` and `used_at` appear on a `used` row alone.
///
/// Newest first, then by code, so the code the admin has just minted is at the top and the
/// order is total.
fn invite_rows(accounts: &Accounts, now: u64) -> String {
    let mut invites: Vec<&Invite> = accounts.invites.values().collect();
    invites.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.code.cmp(&b.code)));
    let rows: Vec<String> = invites
        .iter()
        .map(|invite| {
            let mut fields = vec![
                format!("\"code\":{}", json_string(&grouped(&invite.code))),
                format!("\"by\":{}", json_string(&invite.by)),
                format!("\"at\":{}", invite.at),
                format!("\"expires\":{}", invite.at.saturating_add(INVITE_LIFE.as_secs())),
                format!("\"state\":{}", json_string(invite_state(invite, now))),
            ];
            if let (Some(used_by), Some(used_at)) = (&invite.used_by, invite.used_at) {
                fields.push(format!("\"used_by\":{}", json_string(used_by)));
                fields.push(format!("\"used_at\":{used_at}"));
            }
            format!("{{{}}}", fields.join(","))
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// The body a signed-out `whoami` answers with.
///
/// ⚠ **A function, and it exists because the test used to build its own copy of this string.**
/// `the_signed_out_answer_carries_the_key_the_client_branches_on` re-spelled the `format!` in
/// its own body, so the two could drift and one of them did: `idle_hours` was added to the
/// route and the test went on asserting a two-key object and passing. `minted_body` is the
/// precedent, and its note says the same thing: assert the bytes the client actually reads.
///
/// **Two numbers, because there are two session lifetimes and a page that names one of them is
/// lying about the other.** `session_days` is [`SESSION_MAX`], which is what *Remember me*
/// buys and is the cap on every session either way. `idle_hours` is [`SESSION_IDLE`], which is
/// what an ordinary session gets. The sign-in page prints whichever matches the state of its
/// checkbox and does no arithmetic to get either, so changing a number here cannot leave a
/// client quietly stating the old one.
fn signed_out_body(setup: bool) -> String {
    format!(
        "{{\"setup\":{setup},\"session_days\":{},\"idle_hours\":{}}}",
        SESSION_MAX.as_secs() / 86_400,
        SESSION_IDLE.as_secs() / 3_600
    )
}

/// The body a mint answers with.
///
/// ⚠ **A function rather than a `format!` inside [`answer_change`]**, so the test asserts the
/// bytes the client actually reads. `the_signed_out_answer_carries_the_key_the_client_branches_on`
/// records what a tested half and an untested seam cost last time.
///
/// `days` is sent for the reason `whoami` sends `session_days`: the client prints it in a
/// sentence and should not do date arithmetic to get it.
fn minted_body(code: &str, expires: u64) -> String {
    format!(
        "{{\"code\":{},\"expires\":{expires},\"days\":{}}}",
        json_string(code),
        INVITE_LIFE.as_secs() / 86_400
    )
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
        Change::Gone | Change::Recorded => {
            respond_with(stream, 204, "text/plain", b"", origin, &[])
        }
        Change::Minted { code, expires } => {
            let body = minted_body(&code, expires);
            respond_with(stream, 200, "application/json", body.as_bytes(), origin, &[])
        }
        Change::Taken => {
            respond_with(stream, CONFLICT, "text/plain", b"there is already an account with that name\n", origin, &[])
        }
        Change::NotAllowed(reason) => {
            respond_with(stream, FORBIDDEN, "text/plain", reason.as_bytes(), origin, &[])
        }
        // ⚠ 403 rather than 401. The request was not missing a credential; the credential it
        // presented is not one this server will take. See [`BAD_CODE`] on why unknown, spent,
        // revoked and expired all read the same.
        Change::BadCode => {
            respond_with(stream, FORBIDDEN, "text/plain", BAD_CODE.as_bytes(), origin, &[])
        }
        // The same sentence sign-in gives, because it is the same budget.
        Change::Throttled => respond_with(
            stream,
            TOO_MANY,
            "text/plain",
            b"too many attempts just now; wait a moment and try again\n",
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
        store.sign_in(user, password, wire, false)
    }

    /// [`attempt`], with *Remember me* ticked.
    ///
    /// Separate rather than a fifth parameter on `attempt`, because every existing caller
    /// wants the ordinary session and a bool at the end of four arguments reads as noise at
    /// each of them.
    fn attempt_remembered(store: &mut Accounts, user: &str, password: &str, wire: &Wire) -> Outcome {
        store.attempts.clear();
        store.sign_in(user, password, wire, true)
    }

    fn found(store: &mut Accounts, user: &str, password: &str) -> Identity {
        match store.create_account(user, password, false, "", None, &client_at(SOMEWHERE)) {
            Change::Made(identity) => identity,
            _ => panic!("the first account was refused"),
        }
    }

    /// Make an account the way a setup request or an admin does: no code, and a wire that is
    /// nobody in particular.
    ///
    /// It exists to absorb the two parameters [`Accounts::create_account`] gained with invite
    /// codes, so that the fourteen call sites in this module stay one line each.
    fn add(
        store: &mut Accounts,
        user: &str,
        password: &str,
        admin: bool,
        by: Option<&Identity>,
    ) -> Change {
        store.create_account(user, password, admin, "", by, &client_at(SOMEWHERE))
    }

    /// Make an account the way an invited person does: no caller, and the code they typed.
    fn redeem(store: &mut Accounts, user: &str, password: &str, code: &str, wire: &Wire) -> Change {
        store.create_account(user, password, false, code, None, wire)
    }

    /// Mint one code, or panic naming what stopped it. Returns the **grouped** spelling, the
    /// one the client is given; `fold_code` turns it back into the map's key.
    fn mint(store: &mut Accounts, by: &Identity) -> String {
        match store.create_invite(by) {
            Change::Minted { code, .. } => code,
            _ => panic!("an admin could not mint an invite code"),
        }
    }

    /// The identity a [`Change::Made`] carries, or a panic.
    fn made(change: Change) -> Identity {
        match change {
            Change::Made(identity) => identity,
            _ => panic!("that account was refused"),
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
        let short = add(&mut store, "sam", "short", false, Some(&owner));
        assert!(matches!(short, Change::Refused(_)), "a five-character password was accepted");
        let confusable =
            add(&mut store, "\u{43e}wner", "a-long-enough-passphrase", false, Some(&owner));
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
        let second = add(&mut store, "stranger", "another-long-passphrase", true, None);
        assert!(
            matches!(second, Change::NotAllowed(_)),
            "a stranger claimed a second owner account"
        );

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
        let refused = add(&mut store, "stranger", "another-long-passphrase", false, None);
        assert!(matches!(refused, Change::Broken), "a damaged file still let somebody sign up");
        // ⚠ And a code cannot rescue it either: an unreadable log is a log whose invites
        // cannot be verified, and the safe answer to "cannot prove it" is no.
        let with_code = redeem(
            &mut store,
            "stranger",
            "another-long-passphrase",
            "K7QM-3XPT-9WNZ",
            &client_at(SOMEWHERE),
        );
        assert!(
            matches!(with_code, Change::Broken),
            "an unverifiable code was treated as a valid one"
        );
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
            add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)),
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

        add(&mut store, "sam", "another-long-passphrase", false, Some(&owner));
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
        let sam = match add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)) {
            Change::Made(identity) => identity,
            _ => panic!("an admin could not make an account"),
        };
        assert!(!sam.admin, "a plain account was made an admin");

        let by_sam = add(&mut store, "kit", "a-third-long-passphrase", false, Some(&sam));
        assert!(matches!(by_sam, Change::NotAllowed(_)), "a non-admin made an account");

        let taken = add(&mut store, "SAM", "a-fourth-long-passphrase", false, Some(&owner));
        assert!(matches!(taken, Change::Taken), "a taken name was reused across a case change");
    }

    /// ⚠ Removing an account must be incapable of removing a board. RULE ZERO.
    #[test]
    fn removing_an_account_reverts_its_boards_and_takes_no_file_with_it() {
        let data = scratch("remove-account");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = match add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)) {
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

    // ----- invite codes ---------------------------------------------------------------------

    #[test]
    fn an_invite_code_is_never_the_same_twice() {
        let codes: std::collections::BTreeSet<String> =
            (0..64).map(|_| new_invite_code()).collect();
        assert_eq!(codes.len(), 64, "the invite code generator repeats itself");
        for code in &codes {
            assert_eq!(code.len(), CODE_CHARS, "a code is not {CODE_CHARS} characters");
            assert!(
                code.bytes().all(|byte| CODE_ALPHABET.contains(&byte)),
                "a code held a symbol outside the alphabet"
            );
            // The four Crockford leaves out, because a person confuses each with a digit.
            assert!(!code.contains(['I', 'L', 'O', 'U']), "a code held a confusable letter");
        }
    }

    #[test]
    fn a_code_is_read_the_way_a_person_types_it() {
        let canonical = "K7QM3XPT9WNZ";
        for spelling in [
            "K7QM3XPT9WNZ",
            "K7QM-3XPT-9WNZ",
            "k7qm-3xpt-9wnz",
            " K7QM 3XPT 9WNZ ",
            "K7QM-3xpt 9WNZ",
        ] {
            assert_eq!(fold_code(spelling).as_deref(), Some(canonical), "{spelling}");
        }
        // Crockford's read aliases: the letters the alphabet leaves out, put back where they
        // can only have meant the digit beside them.
        assert_eq!(fold_code("IL0000000000").as_deref(), Some("110000000000"));
        assert_eq!(fold_code("O00000000000").as_deref(), Some("000000000000"));
        // ⚠ `U` is refused rather than aliased. It is out of the alphabet on purpose and
        // there is no digit it resembles.
        assert!(fold_code("U00000000000").is_none(), "U was aliased to something");
        assert!(fold_code("K7QM3XPT9WN").is_none(), "eleven characters were accepted");
        assert!(fold_code("K7QM3XPT9WNZ1").is_none(), "thirteen characters were accepted");
        // ⚠ Refused **before** any `as u8`: `'İ' as u8` is not `'İ'`, and the truncated byte
        // can land inside the alphabet.
        assert!(fold_code("K7QM3XPT9WN\u{130}").is_none(), "a non-ASCII character was folded");
        assert!(fold_code("").is_none());
        assert!(fold_code("nonsense").is_none());
        assert_eq!(grouped(canonical), "K7QM-3XPT-9WNZ", "the display form is not grouped");
    }

    /// ⚠ `Caller::Token` cannot reach [`Accounts::create_invite`] at all — it takes an
    /// [`Identity`], and only `identity_of` and `create_account` mint one. That is the second
    /// half of the rule this test pins; it cannot be written as an assertion because the
    /// unwanted call does not typecheck, which is the stronger form.
    #[test]
    fn only_an_admin_may_mint_a_code() {
        let mut store = Accounts::open(&scratch("mint-admin-only"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = made(add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)));

        let Change::NotAllowed(reason) = store.create_invite(&sam) else {
            panic!("a non-admin minted an invite code");
        };
        assert!(reason.contains("admin"), "the refusal does not say who may do this");
        assert!(store.invites.is_empty(), "a refused mint still recorded a code");
    }

    #[test]
    fn a_minted_code_survives_a_restart() {
        let data = scratch("code-restart");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let code = mint(&mut store, &owner);

        let mut reopened = Accounts::open(&data);
        assert_eq!(reopened.invites.len(), 1, "the invite record did not replay");
        let made_it = redeem(
            &mut reopened,
            "sam",
            "another-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        );
        assert!(matches!(made_it, Change::Made(_)), "a code did not survive a restart");
    }

    #[test]
    fn a_code_creates_one_account_and_is_then_spent() {
        let data = scratch("code-spent-once");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let code = mint(&mut store, &owner);

        let sam = made(redeem(
            &mut store,
            "sam",
            "another-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        ));
        assert_eq!(sam.username, "sam");

        let again = redeem(
            &mut store,
            "kit",
            "a-third-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        );
        assert!(matches!(again, Change::BadCode), "one code made two accounts");

        // And the spend is in the file, not only in memory: it is carried by the `account`
        // record's own `code` field, which is what makes redemption one line.
        let mut reopened = Accounts::open(&data);
        let after_restart = redeem(
            &mut reopened,
            "kit",
            "a-third-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        );
        assert!(matches!(after_restart, Change::BadCode), "a restart reopened a spent code");
    }

    #[test]
    fn an_account_made_with_a_code_is_never_an_admin() {
        let mut store = Accounts::open(&scratch("code-never-admin"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let code = mint(&mut store, &owner);

        // The body asks for an admin. The code decides, not the body.
        let asked_for_admin = store.create_account(
            "sam",
            "another-long-passphrase",
            true,
            &code,
            None,
            &client_at(SOMEWHERE),
        );
        let sam = made(asked_for_admin);
        assert!(!sam.admin, "an invited account made itself an admin");
    }

    /// ⚠ **The replay bug this format predicts.** Removing the account a code created must
    /// not bring the code back: `KIND_ACCOUNT_REMOVED` writes nothing about invites and
    /// `apply` touches none, so the spend outlives the account.
    #[test]
    fn removing_an_invited_account_does_not_bring_its_code_back() {
        let data = scratch("removed-account-code");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let code = mint(&mut store, &owner);
        made(redeem(&mut store, "sam", "another-long-passphrase", &code, &client_at(SOMEWHERE)));

        assert!(matches!(store.remove_account("sam", &owner), Change::Gone));

        let mut reopened = Accounts::open(&data);
        let reused = redeem(
            &mut reopened,
            "sam",
            "a-third-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        );
        assert!(matches!(reused, Change::BadCode), "removing an account reopened its code");
    }

    #[test]
    fn a_revoked_code_is_refused_and_stays_refused_after_a_restart() {
        let data = scratch("code-revoked");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let code = mint(&mut store, &owner);

        assert!(matches!(store.revoke_invite(&code, &owner), Change::Gone));
        // Revoking twice is harmless and writes nothing the second time — the desired state
        // is reached either way, exactly as for signing out.
        assert!(matches!(store.revoke_invite(&code, &owner), Change::Gone));
        let refused = redeem(
            &mut store,
            "sam",
            "another-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        );
        assert!(matches!(refused, Change::BadCode), "a revoked code still made an account");

        let mut reopened = Accounts::open(&data);
        let still = redeem(
            &mut reopened,
            "sam",
            "another-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        );
        assert!(matches!(still, Change::BadCode), "a restart un-revoked a code");
    }

    /// ⚠ The clock is a `u64` of wall-clock seconds, so this is arithmetic rather than
    /// `Instant` gymnastics: the invite's own `at` is moved to 1970 and no `checked_sub` is
    /// needed. `INVITE_LIFE` is wall-clock precisely because a code outlives a restart.
    #[test]
    fn a_code_stops_working_after_its_life() {
        let mut store = Accounts::open(&scratch("code-expiry"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let code = mint(&mut store, &owner);
        let key = fold_code(&code).expect("a minted code folds");

        store.invites.get_mut(&key).expect("the code is in the map").at = 1;
        assert_eq!(store.open_invites(now_seconds()), 0, "an expired code still counts as open");

        let refused = redeem(
            &mut store,
            "sam",
            "another-long-passphrase",
            &code,
            &client_at(SOMEWHERE),
        );
        assert!(matches!(refused, Change::BadCode), "a code outlived its life");
    }

    /// ⚠ The budget a wrong code spends is the **same** one sign-in uses, which is why
    /// [`BAD_CODE`] tells the person to wait rather than to ask for another code.
    #[test]
    fn a_wrong_code_costs_the_address_its_budget() {
        let mut store = Accounts::open(&scratch("code-budget"));
        found(&mut store, "owner", "a-long-enough-passphrase");
        let guesser = client_at("198.51.100.7");

        for n in 0..MAX_FAILURES {
            let refused =
                redeem(&mut store, &format!("sam{n}"), "another-long-passphrase", "K7QM3XPT9WNZ", &guesser);
            assert!(matches!(refused, Change::BadCode), "guess {n} was not refused");
        }
        let over = redeem(&mut store, "sam", "another-long-passphrase", "K7QM3XPT9WNZ", &guesser);
        assert!(matches!(over, Change::Throttled), "the address budget did not apply to codes");

        // And it is that address's budget alone, which is what makes this a rate limit
        // rather than a way to shut the household out.
        let elsewhere =
            redeem(&mut store, "sam", "another-long-passphrase", "K7QM3XPT9WNZ", &client_at("203.0.113.4"));
        assert!(matches!(elsewhere, Change::BadCode), "one guesser spent everybody's budget");
    }

    /// ⚠ Malformed is refused **before** the limiter is touched, the rule `sign_in` follows
    /// for a username that will not fold. A family member fumbling a code should not be able
    /// to lock sign-in out for their own house.
    #[test]
    fn a_malformed_code_is_refused_before_the_limiter_is_touched() {
        let mut store = Accounts::open(&scratch("code-malformed"));
        found(&mut store, "owner", "a-long-enough-passphrase");

        for _ in 0..MAX_FAILURES + 5 {
            let refused = redeem(
                &mut store,
                "sam",
                "another-long-passphrase",
                "nonsense",
                &client_at(SOMEWHERE),
            );
            assert!(matches!(refused, Change::BadCode));
        }
        assert!(store.failures.is_empty(), "a code that is not a code spent the address budget");
    }

    /// ⚠ Setup wins outright and the code is not read, not validated and **not spent** —
    /// written as an ordering rather than as an assertion, so that a future change which does
    /// let an invite exist here degrades to "the code was ignored".
    #[test]
    fn setting_up_ignores_any_code_in_the_body() {
        let data = scratch("setup-ignores-code");
        let mut store = Accounts::open(&data);

        let owner = made(redeem(
            &mut store,
            "owner",
            "a-long-enough-passphrase",
            "K7QM-3XPT-9WNZ",
            &client_at(SOMEWHERE),
        ));
        assert!(owner.admin, "the first account is not an admin");
        assert!(store.invites.is_empty(), "a code the server never minted was recorded as spent");

        let text = std::fs::read_to_string(data.join(LOG)).unwrap();
        assert!(!text.contains("K7QM"), "an ignored code was written into the log");
    }

    /// The invariant of the founder rule: an invite cannot exist while setup is open.
    #[test]
    fn an_invite_cannot_exist_while_setup_is_open() {
        let data = scratch("invite-and-setup");
        let mut store = Accounts::open(&data);
        assert!(store.setup_open(), "a fresh data directory is not offering setup");
        assert!(store.invites.is_empty());

        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        mint(&mut store, &owner);
        assert!(!store.setup_open(), "setup stayed open after a code was minted");

        // And in the other direction: any `invite` record makes the file non-empty, so a
        // store that loads one has setup closed before it reads a single account.
        let reopened = Accounts::open(&data);
        assert!(!reopened.setup_open(), "a file holding an invite reopened setup");
        assert_eq!(reopened.invites.len(), 1);
    }

    /// Append-only, asserted on the bytes, for the invite records too.
    #[test]
    fn the_log_only_ever_grows_when_a_code_is_made_and_spent() {
        let data = scratch("invite-append-only");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let after_founding = std::fs::read_to_string(data.join(LOG)).unwrap();

        let code = mint(&mut store, &owner);
        let after_mint = std::fs::read_to_string(data.join(LOG)).unwrap();
        assert!(after_mint.starts_with(&after_founding), "minting rewrote an earlier record");
        assert_eq!(after_mint.lines().count(), after_founding.lines().count() + 1);

        made(redeem(&mut store, "sam", "another-long-passphrase", &code, &client_at(SOMEWHERE)));
        let after_redeem = std::fs::read_to_string(data.join(LOG)).unwrap();
        assert!(after_redeem.starts_with(&after_mint), "redeeming rewrote an earlier record");
        assert_eq!(
            after_redeem.lines().count(),
            after_mint.lines().count() + 1,
            "redeeming wrote more than one record, so a crash can split an account from its code"
        );
    }

    /// ⚠ **The wire body, not the function that feeds it** — the seam
    /// `the_signed_out_answer_carries_the_key_the_client_branches_on` exists to record.
    #[test]
    fn the_mint_answer_carries_the_keys_the_client_branches_on() {
        let mut store = Accounts::open(&scratch("mint-answer"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let at = now_seconds();

        let Change::Minted { code, expires } = store.create_invite(&owner) else {
            panic!("an admin could not mint an invite code");
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&minted_body(&code, expires)).expect("valid JSON");

        let shown = parsed["code"].as_str().expect("the code must be a string");
        assert_eq!(shown.len(), CODE_CHARS + 2, "the code is not grouped for reading");
        assert_eq!(shown.matches('-').count(), 2);
        let ungrouped = shown.replace('-', "");
        assert_eq!(
            fold_code(shown).as_deref(),
            Some(ungrouped.as_str()),
            "the code that goes out does not fold back to the one that is stored"
        );
        assert!(
            parsed["expires"].as_u64().is_some_and(|when| when > at),
            "the expiry is not in the future"
        );
        assert_eq!(
            parsed["days"],
            serde_json::json!(INVITE_LIFE.as_secs() / 86_400),
            "the client prints this in a sentence and should not do date arithmetic"
        );
    }

    #[test]
    fn the_invite_list_says_which_codes_are_still_open() {
        let mut store = Accounts::open(&scratch("invite-list"));
        let now = 1_750_000_000u64;
        let life = INVITE_LIFE.as_secs();
        let mut put = |code: &str, at: u64, used: Option<&str>, revoked: Option<u64>| {
            store.invites.insert(code.to_owned(), Invite {
                code: code.to_owned(),
                by: "owner".to_owned(),
                at,
                used_by: used.map(str::to_owned),
                used_at: used.map(|_| at + 10),
                revoked_at: revoked,
            });
        };
        put("AAAAAAAAAAAA", now - 10, None, None);
        put("BBBBBBBBBBBB", now - 20, Some("sam"), None);
        put("CCCCCCCCCCCC", now - 30, None, Some(now - 25));
        put("DDDDDDDDDDDD", now - life - 1, None, None);

        let parsed: serde_json::Value =
            serde_json::from_str(&invite_rows(&store, now)).expect("valid JSON");
        let rows = parsed.as_array().expect("an array");
        assert_eq!(rows.len(), 4);

        let states: Vec<&str> = rows.iter().map(|row| row["state"].as_str().unwrap()).collect();
        assert_eq!(states, ["open", "used", "revoked", "expired"], "newest first, by state");

        let used = &rows[1];
        assert_eq!(used["used_by"], serde_json::json!("sam"));
        assert!(used["used_at"].as_u64().is_some(), "a used row does not say when");
        assert!(rows[0]["used_by"].is_null(), "an open row claimed it was used");
        assert_eq!(
            rows[0]["code"],
            serde_json::json!("AAAA-AAAA-AAAA"),
            "the list does not group the code the way a person reads it"
        );
        assert_eq!(rows[0]["expires"], serde_json::json!(now - 10 + life));
    }

    #[test]
    fn a_code_cannot_be_minted_past_the_cap() {
        let mut store = Accounts::open(&scratch("invite-cap"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");

        let first = mint(&mut store, &owner);
        for _ in 1..MAX_OPEN_INVITES {
            mint(&mut store, &owner);
        }
        let Change::Refused(reason) = store.create_invite(&owner) else {
            panic!("the cap on open codes did not apply");
        };
        assert_eq!(reason, TOO_MANY_INVITES);

        // Revoking one makes room, because the cap counts open codes rather than every code
        // that has ever existed.
        assert!(matches!(store.revoke_invite(&first, &owner), Change::Gone));
        assert!(matches!(store.create_invite(&owner), Change::Minted { .. }));
    }

    /// ⚠ A `&'static str` cannot hold a formatted number, so the cap is typed twice. This is
    /// what stops the two from drifting apart into a sentence that lies about the rule.
    #[test]
    fn the_cap_and_its_refusal_agree() {
        assert!(
            TOO_MANY_INVITES.contains(&MAX_OPEN_INVITES.to_string()),
            "the refusal at the cap does not name the cap's number: {TOO_MANY_INVITES}"
        );
    }

    // ----- sharing a board --------------------------------------------------------------------

    /// The route that was missing: `Accounts::allowed` has read a shared list since accounts
    /// landed, and nothing could put a name in it.
    #[test]
    fn a_shared_board_is_visible_to_the_person_it_was_shared_with() {
        let mut store = Accounts::open(&scratch("share-visible"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = made(add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)));
        store.claim("plans", "owner").unwrap();
        assert!(!store.allowed("plans", &Caller::Account(sam.clone())));

        // Spelt the way the person typed it; folded the way every username is.
        assert!(matches!(store.set_share("plans", "Sam", &owner, true), Change::Recorded));
        assert!(
            store.allowed("plans", &Caller::Account(sam)),
            "the board was shared and is still invisible"
        );
        assert_eq!(store.boards["plans"].shared, vec!["sam".to_owned()]);
    }

    /// ⚠ Sharing is not a transferable power. One share must not put a board one hop from
    /// everybody on the server.
    #[test]
    fn only_the_owner_or_an_admin_may_share_a_board() {
        let mut store = Accounts::open(&scratch("share-authority"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = made(add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)));
        let kit = made(add(&mut store, "kit", "a-third-long-passphrase", false, Some(&owner)));
        store.claim("sams-plans", "sam").unwrap();

        // The owner may.
        assert!(matches!(store.set_share("sams-plans", "kit", &sam, true), Change::Recorded));

        // Somebody it was merely shared with may not, even though they can see it.
        assert!(store.allowed("sams-plans", &Caller::Account(kit.clone())));
        let Change::NotAllowed(reason) = store.set_share("sams-plans", "owner", &kit, true) else {
            panic!("a board was shared on by somebody it was only shared with");
        };
        assert!(reason.contains("owner"), "the refusal does not say who may do this");

        // An admin may, and doing so does not make the board theirs.
        assert!(matches!(store.set_share("sams-plans", "owner", &owner, true), Change::Recorded));
        assert_eq!(store.owner_of("sams-plans"), Some("sam"), "an admin sharing took the board");
    }

    #[test]
    fn sharing_with_a_name_that_has_no_account_is_refused_and_records_nothing() {
        let data = scratch("share-unknown-name");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        store.claim("plans", "owner").unwrap();
        let before = std::fs::read_to_string(data.join(LOG)).unwrap();

        let Change::Refused(reason) = store.set_share("plans", "nobody", &owner, true) else {
            panic!("a board was shared with an account that does not exist");
        };
        assert!(reason.contains("account"), "the refusal does not say what is wrong");
        assert_eq!(
            std::fs::read_to_string(data.join(LOG)).unwrap(),
            before,
            "a refused share still wrote a record"
        );
    }

    /// ⚠ **Being shared with a board must not tell you who else was.**
    ///
    /// `allowed` says yes to everybody a board is shared with, so building the list from it
    /// would hand each of them the names of all the others. The board list carries `owner` and
    /// `shared` only for the owner and for an admin, and the difference between "the server
    /// did not say" and "nobody" is what the absent keys mean on the wire.
    #[test]
    fn a_shared_account_is_not_told_who_else_the_board_is_shared_with() {
        let data = scratch("share-disclosure");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = made(add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)));
        let kim = made(add(&mut store, "kim", "a-third-long-passphrase", false, Some(&owner)));
        let boss = made(add(&mut store, "boss", "a-fourth-long-passphrase", true, Some(&owner)));
        store.claim("plans", "owner").unwrap();
        assert!(matches!(store.set_share("plans", "sam", &owner, true), Change::Recorded));
        assert!(matches!(store.set_share("plans", "kim", &owner, true), Change::Recorded));

        let (whose, names) = store.sharing_seen_by("plans", &owner).expect("the owner may know");
        assert_eq!(whose, "owner");
        // Sorted, not in the order they were added, so the list reads the same on every replay.
        assert_eq!(names, vec!["kim".to_owned(), "sam".to_owned()]);

        // An admin may know, because an admin may change it.
        assert!(store.sharing_seen_by("plans", &boss).is_some(), "an admin was refused the list");

        // Sam and Kim can both open the board, and neither may learn that the other can.
        for who in [&sam, &kim] {
            assert!(
                store.allowed("plans", &Caller::Account(who.clone())),
                "the fixture is wrong: {} cannot open the board", who.username
            );
            assert!(
                store.sharing_seen_by("plans", who).is_none(),
                "{} was told who else the board is shared with", who.username
            );
        }
    }

    #[test]
    fn unsharing_takes_the_board_back() {
        let data = scratch("share-undo");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = made(add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)));
        store.claim("plans", "owner").unwrap();

        assert!(matches!(store.set_share("plans", "sam", &owner, true), Change::Recorded));
        assert!(matches!(store.set_share("plans", "sam", &owner, false), Change::Recorded));
        assert!(!store.allowed("plans", &Caller::Account(sam.clone())), "unsharing did nothing");

        // The empty list is a record like any other, so the last one wins on replay.
        let reopened = Accounts::open(&data);
        assert!(
            !reopened.allowed("plans", &Caller::Account(sam)),
            "a restart brought a removed share back"
        );
    }

    #[test]
    fn sharing_survives_a_restart() {
        let data = scratch("share-restart");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = made(add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)));
        store.claim("plans", "owner").unwrap();
        assert!(matches!(store.set_share("plans", "sam", &owner, true), Change::Recorded));

        let reopened = Accounts::open(&data);
        assert!(reopened.allowed("plans", &Caller::Account(sam)), "a share did not replay");
        assert_eq!(reopened.owner_of("plans"), Some("owner"));
    }

    /// Every board that was in the data directory before accounts existed has no ownership
    /// record, and `owner_of` gives those to the founder. So the founder can share them —
    /// which is the whole point, since that is ~45 of them.
    #[test]
    fn the_founder_can_share_a_board_that_has_no_owner_record() {
        let mut store = Accounts::open(&scratch("share-unrecorded"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        let sam = made(add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)));
        assert!(!store.boards.contains_key("an-old-board"), "the fixture is not the state under test");

        assert!(matches!(store.set_share("an-old-board", "sam", &owner, true), Change::Recorded));
        assert!(store.allowed("an-old-board", &Caller::Account(sam)));
        assert_eq!(store.owner_of("an-old-board"), Some("owner"), "the founder did not stay owner");

        // And somebody else's board is still not theirs to share.
        store.claim("sams-plans", "sam").unwrap();
        let kit = made(add(&mut store, "kit", "a-third-long-passphrase", false, Some(&owner)));
        assert!(matches!(
            store.set_share("sams-plans", "owner", &kit, true),
            Change::NotAllowed(_)
        ));
    }

    /// The file is never compacted, so a route that writes a record for a state it is already
    /// in is a route that grows the file every time somebody presses the button twice.
    #[test]
    fn sharing_twice_records_nothing_the_second_time() {
        let data = scratch("share-idempotent");
        let mut store = Accounts::open(&data);
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        add(&mut store, "sam", "another-long-passphrase", false, Some(&owner));
        add(&mut store, "kit", "a-third-long-passphrase", false, Some(&owner));
        store.claim("plans", "owner").unwrap();

        assert!(matches!(store.set_share("plans", "sam", &owner, true), Change::Recorded));
        let after_one = std::fs::read_to_string(data.join(LOG)).unwrap();

        assert!(matches!(store.set_share("plans", "sam", &owner, true), Change::Recorded));
        // Taking off somebody who was never on, and sharing with the owner, are the same
        // no-op from the other two directions.
        assert!(matches!(store.set_share("plans", "kit", &owner, false), Change::Recorded));
        assert!(matches!(store.set_share("plans", "owner", &owner, true), Change::Recorded));

        assert_eq!(
            std::fs::read_to_string(data.join(LOG)).unwrap(),
            after_one,
            "a share that changed nothing still wrote a line"
        );
    }

    /// ⚠ **`owner_of`'s fall back to the founder is derived and must never be written down.**
    /// An admin sharing a board whose recorded owner has been removed must not materialise
    /// themselves as its owner, or an account made again with that name never gets it back.
    #[test]
    fn sharing_a_removed_accounts_board_does_not_take_it_over() {
        let mut store = Accounts::open(&scratch("share-keeps-owner"));
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
        add(&mut store, "sam", "another-long-passphrase", false, Some(&owner));
        let kit = made(add(&mut store, "kit", "a-third-long-passphrase", false, Some(&owner)));
        store.claim("sams-plans", "sam").unwrap();

        assert!(matches!(store.remove_account("sam", &owner), Change::Gone));
        assert_eq!(store.owner_of("sams-plans"), Some("owner"), "the fallback did not apply");

        assert!(matches!(store.set_share("sams-plans", "kit", &owner, true), Change::Recorded));
        assert_eq!(
            store.boards["sams-plans"].owner, "sam",
            "sharing rewrote the recorded owner, so the board can never revert"
        );

        // Sam comes back, and the board is theirs again — which is what the fallback is for.
        add(&mut store, "sam", "a-fourth-long-passphrase", false, Some(&owner));
        assert_eq!(store.owner_of("sams-plans"), Some("sam"));
        assert!(store.allowed("sams-plans", &Caller::Account(kit)));
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
        add(&mut store, "sam", "another-long-passphrase", true, Some(&owner));

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

    /// *Remember me* switches off the idle rule, and only the idle rule.
    ///
    /// Both halves are asserted against the same instant, because the thing that would break
    /// silently is the two rules being combined the wrong way round: an `&&` where there is an
    /// `||` would make a remembered session immortal, and nothing else in the suite would
    /// notice for thirty days.
    #[test]
    fn remembering_a_session_drops_the_idle_rule_and_keeps_the_thirty_day_cap() {
        let mut store = Accounts::open(&scratch("remember-me"));
        found(&mut store, "owner", "a-long-enough-passphrase");

        let ordinary = match attempt(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };
        let remembered = match attempt_remembered(&mut store, "owner", "a-long-enough-passphrase", &client_at(SOMEWHERE)) {
            Outcome::SignedIn { id, .. } => id,
            _ => panic!("the right password did not sign in"),
        };

        // Just past the idle window. The ordinary one is gone; the remembered one is not.
        let idled = Instant::now() + SESSION_IDLE + Duration::from_secs(60);
        assert!(store.identity_of(&ordinary, idled).is_none(), "the idle rule stopped applying");
        assert!(
            store.identity_of(&remembered, idled).is_some(),
            "Remember me did not survive the idle window"
        );

        // Past the hard cap. Neither survives, and that is the half that bounds a stolen cookie.
        let capped = Instant::now() + SESSION_MAX + Duration::from_secs(60);
        assert!(store.identity_of(&ordinary, capped).is_none());
        assert!(
            store.identity_of(&remembered, capped).is_none(),
            "a remembered session outlived SESSION_MAX, so it never ends"
        );
    }

    /// The default is the short session, and a client that has never heard of the flag gets it.
    ///
    /// Asserted on the wire type rather than through `sign_in`, because the direction of the
    /// default is the whole point: forgetting to send it must shorten a session, never
    /// lengthen one.
    #[test]
    fn a_body_with_no_remember_field_asks_for_the_ordinary_session() {
        let without = credentials(br#"{"username":"sam","password":"another-long-passphrase"}"#)
            .expect("the body did not parse");
        assert!(!without.remember, "a body with no remember field asked to be remembered");
        let with = credentials(
            br#"{"username":"sam","password":"another-long-passphrase","remember":true}"#,
        )
        .expect("the body did not parse");
        assert!(with.remember);
    }

    // ----- rate limiting --------------------------------------------------------------------

    #[test]
    fn two_attempts_on_one_username_inside_a_second_are_refused_without_a_lockout() {
        let mut store = Accounts::open(&scratch("min-interval"));
        found(&mut store, "owner", "a-long-enough-passphrase");

        assert!(matches!(
            store.sign_in("owner", "wrong-but-long-enough", &client_at(SOMEWHERE), false),
            Outcome::Wrong
        ));
        let stamped = store.attempts["owner"];
        assert!(
            matches!(store.sign_in("owner", "a-long-enough-passphrase", &client_at(SOMEWHERE), false), Outcome::Throttled),
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
            store.sign_in("owner", "a-long-enough-passphrase", &client_at(SOMEWHERE), false),
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
        assert!(matches!(store.sign_in("nobody", "a-long-enough-passphrase", &client_at(SOMEWHERE), false), Outcome::Wrong));
        assert!(
            matches!(store.sign_in("nobody", "a-long-enough-passphrase", &client_at(SOMEWHERE), false), Outcome::Throttled),
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
        let sam = match add(&mut store, "sam", "another-long-passphrase", false, Some(&owner)) {
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
        add(&mut store, "sam", "another-long-passphrase", false, Some(&owner));
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
        let owner = found(&mut store, "owner", "a-long-enough-passphrase");
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

        // ⚠ **An invite code is a bearer credential too**, and it is stored in the clear, so
        // the only thing keeping it out of a log is that no type carrying one derives `Debug`.
        // `Change` has no derive for exactly this reason; `Change::Minted` holds a live code.
        let code = mint(&mut store, &owner);
        let key = fold_code(&code).expect("a minted code folds");
        let printed = format!("{store:?}");
        assert!(!printed.contains(&key), "an invite code reached a Debug");
        assert!(!printed.contains(&code), "an invite code reached a Debug");
        let invite = format!("{:?}", store.invites[&key]);
        assert!(!invite.contains(&key), "an invite code reached its own Debug");
        assert!(invite.contains("owner"), "who minted it is not a secret and should be there");
        // The mint log line names the minter and never the code. `create_invite` writes
        // `velmd: {by} made an invite code`, and there is no other formatting of one.
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
        // ⚠ `signed_out_body`, not a second `format!` written here. This test used to spell the
        // string itself, so when `idle_hours` was added to the route the test kept asserting a
        // two-key object and kept passing. One function, one set of bytes, one assertion.
        let parsed: serde_json::Value =
            serde_json::from_str(&signed_out_body(true)).expect("valid JSON");
        assert_eq!(parsed["setup"], serde_json::json!(true), "the setup flag must be a bool");
        assert!(
            parsed["session_days"].as_u64().is_some_and(|d| d > 0),
            "the client prints this in a sentence and falls back silently when it is missing"
        );
        assert!(
            parsed["idle_hours"].as_u64().is_some_and(|h| h > 0),
            "the sign-in page prints this whenever Remember me is not ticked"
        );
        assert!(signed_out_body(false).contains("\"setup\":false"));
    }

    #[test]
    fn the_routes_are_the_ones_the_contract_names() {
        assert!(is_session("/api/v1/session"));
        assert!(is_accounts("/api/v1/accounts"));
        assert!(is_whoami("/api/v1/whoami"));
        assert!(is_invites("/api/v1/invites"));
        assert!(!is_session("/api/v1/sessions"));
        assert!(!is_accounts("/api/v1/accounts/sam"));
        assert!(!is_invites("/api/v1/invites/"));

        assert_eq!(revoke_target("/api/v1/invites/K7QM-3XPT-9WNZ"), Some("K7QM-3XPT-9WNZ"));
        assert_eq!(revoke_target("/api/v1/invites"), None);
        assert_eq!(revoke_target("/api/v1/invites/"), None);
        assert_eq!(revoke_target("/api/v1/invites/a/b"), None);

        assert_eq!(share_target("/api/v1/boards/plans/share"), Some("plans"));
        // Percent-encoded, because a board id is a file stem and real ones have spaces in them.
        assert_eq!(share_target("/api/v1/boards/last%20quarter/share"), Some("last%20quarter"));
        assert_eq!(share_target("/api/v1/boards//share"), None);
        assert_eq!(share_target("/api/v1/boards/plans"), None);
        assert_eq!(share_target("/api/v1/boards/a/b/share"), None);
        // ⚠ The two share routes must not read each other's paths.
        assert_eq!(share_target("/api/v1/boards/plans/share/sam"), None);

        assert_eq!(unshare_target("/api/v1/boards/plans/share/sam"), Some(("plans", "sam")));
        assert_eq!(unshare_target("/api/v1/boards/plans/share"), None);
        assert_eq!(unshare_target("/api/v1/boards/plans/share/"), None);
        assert_eq!(unshare_target("/api/v1/boards//share/sam"), None);
        assert_eq!(unshare_target("/api/v1/boards/plans/share/a/b"), None);

        // ⚠ Exactly two routes may answer without a bearer token, and no board route may.
        assert!(is_exempt_from_the_bearer_gate("/api/v1/session"));
        assert!(is_exempt_from_the_bearer_gate("/api/v1/whoami"));
        // ⚠ `/api/v1/accounts` is **not** exempt by path, and that is what keeps `GET` on it
        // admin-only. Its carve-out is a **condition** in `serve.rs` — `POST` alone — which is
        // wide enough for a browser founding a server or redeeming an invite code, and narrow
        // enough that the account listing stays behind the gate. Every invite and share route
        // needs an admin or an owner, so all of them are gated outright.
        for gated in [
            "/api/v1/accounts",
            "/api/v1/invites",
            "/api/v1/invites/K7QM3XPT9WNZ",
            "/api/v1/boards",
            "/api/v1/boards/plans/share",
            "/api/v1/boards/plans/share/sam",
            "/api/v1/library",
            "/api/v1/blobs/abc",
        ] {
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

        // ⚠ The invite code rides on the same body, and it is optional: a client written
        // before codes existed sends exactly what it always did and must still work.
        let with_code = credentials(
            br#"{"username":"sam","password":"another-long-passphrase","code":"K7QM-3XPT-9WNZ"}"#,
        );
        assert_eq!(with_code.expect("a body with a code parses").code, "K7QM-3XPT-9WNZ");
        let without = credentials(br#"{"username":"sam","password":"another-long-passphrase"}"#);
        assert_eq!(
            without.expect("a body with no code parses").code,
            "",
            "a missing code did not default to empty, so an old client cannot sign up"
        );

        // The share body is its own type, for the same reasons.
        assert!(sharing(b"not json").is_none());
        assert_eq!(sharing(br#"{"username":"sam"}"#).expect("valid").username, "sam");
        assert_eq!(sharing(b"{}").expect("an empty object parses").username, "");
    }

    #[test]
    fn a_session_id_is_never_the_same_twice() {
        let ids: std::collections::BTreeSet<String> = (0..64).map(|_| new_session_id()).collect();
        assert_eq!(ids.len(), 64, "the session id generator repeats itself");
    }
}
