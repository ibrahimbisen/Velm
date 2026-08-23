//! Making a board, and naming one — the two verbs the browser was missing.
//!
//! ```text
//! POST /api/v1/boards               body: the name        -> {"id":…,"title":…}
//! POST /api/v1/boards/{id}/rename   body: the new name    -> {"id":…,"title":…}
//! ```
//!
//! The body is the name, as plain UTF-8 text, and nothing else. There is no JSON to parse on
//! the way in: a name is one string, and a request format with exactly one field does not
//! need a grammar. It comes back as JSON because the answer has two fields — the id the
//! client must use from now on, and the title as it was actually stored after trimming.
//!
//! # 🛑 RULE ZERO — this is the first route in `velmd` that creates a file
//!
//! Every other route in this program either reads, or merges into a board that already
//! exists. This one calls a file into being, and the whole design of the create path is
//! about the one way that could go wrong: **landing on a board that is already there.**
//!
//! [`vellum_store::BoardDb::open`] does not truncate an existing file — it opens it as the
//! SQLite database it is. That sounds like the safe direction and it is worse than
//! truncation would be: a `save` into somebody else's board appends a chunk of a *different*
//! document to their chunk chain, and the next `load` replays both. The board is not blanked,
//! it is adulterated, and there is no `rm` anywhere in it for `tests/rule_zero.rs` to catch.
//!
//! So "create cannot overwrite" is made **structural** rather than checked:
//!
//! 1. [`free_stem`] walks past every name that is taken, exactly as the desktop app's
//!    `Library::free_path` does — appending `-2`, `-3` — because four of this user's boards
//!    are called *Untitled* and a create that reused a stem would be the fault above.
//! 2. The file is then claimed with `create_new`, which is `O_CREAT | O_EXCL`: it **fails**
//!    if anything is already at that path rather than opening or truncating it. That is the
//!    guarantee, and it is one the kernel makes rather than one this module has to remember.
//!
//! The loop is the good name; the exclusive create is the promise. Either alone would work
//! nearly always, and "nearly always" is not the standard for a directory holding ~58 boards
//! that cannot be re-imported.
//!
//! # There is no delete route, and there must not be one
//!
//! Not an omission and not a to-do. Deletion lives in the desktop app's `Library::purge`,
//! which is reachable from Recently deleted alone and is confirmed; a browser tab that can
//! be closed by the operating system with no chance to flush is the last place to put an
//! irreversible verb. `tests/rule_zero.rs` greps this crate for the calls that would be
//! needed, so adding one is not a thing that can be done quietly.
//!
//! # Renaming names the board, not the file
//!
//! See [`rename_board`]. The short version, because it is the decision most likely to be
//! second-guessed: the file keeps its name, and only the title inside the document changes.

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use vellum_doc::Board;
use vellum_store::{BOARD_EXTENSION, BoardDb};

use crate::serve::{Server, board_by_id, json_string, printable, respond};
use crate::sync::{Refusal, before_the_web};

/// The largest request body either of these routes will read.
///
/// ⚠ **Its own cap, not [`crate::sync::MAX_BODY`], and refused before a byte is read.** Sync's
/// eight megabytes is sized for a CRDT delta — a whole document's worth of geometry, text and
/// style. A board's name is a name. Sharing sync's number would let a create request reserve
/// eight megabytes sixteen times over to deliver a word, which is the shape of a memory
/// exhaustion that costs an attacker one `Content-Length` header.
///
/// A kilobyte is far past [`MAX_NAME`] at four bytes per character, so nothing a person could
/// type is refused here — the length check that a client sees is [`clean_name`]'s, in
/// characters, with a sentence saying so.
pub const MAX_BODY: usize = 1024;

/// The longest board name this will store, in characters.
///
/// Characters rather than bytes, because that is what the person typing it counts. Generous
/// on purpose: the point of the cap is that a name is a name — it goes in a JSON response, a
/// log line and the board's own index row — not to be an opinion about how somebody titles
/// their work.
const MAX_NAME: usize = 200;

/// How many `-2`, `-3` suffixes [`free_stem`] will try before giving up.
///
/// A bound rather than a `loop`, because the loop's exit condition is a filesystem answer and
/// a filesystem that answers "taken" for every name — a full disk, a directory that has hit a
/// limit, a permissions fault — would spin this thread forever inside the board lock, taking
/// every other board request with it. Refusing after a thousand is a bad answer to a
/// pathological directory; not answering at all is a worse one.
const MAX_SUFFIX: u32 = 1000;

const PATH_BOARDS: &str = "/api/v1/boards";
const PATH_PREFIX: &str = "/api/v1/boards/";
const PATH_SUFFIX: &str = "/rename";

/// Whether this path is the create route.
///
/// The routing rule lives here rather than in `serve.rs`, following [`crate::sync::board_id`]
/// exactly: it can then be tested without a socket, and the one fact `serve.rs` has to get
/// right — that this is under `/api/v1/` and therefore behind `needs_token` — is asserted in
/// the same file that spells the path.
///
/// ⚠ Note what this is *not*: `GET /api/v1/boards` is the board list and is a different verb
/// on the same path. `serve.rs` answers `POST` before `route` ever runs, so the two cannot
/// collide — and this function is deliberately about the path alone, so it cannot start
/// disagreeing with that dispatch about which method it describes.
pub fn is_create(path: &str) -> bool {
    path == PATH_BOARDS
}

/// The board id in a rename path, or `None` if this is not one.
///
/// The id is compared against file stems that came out of a directory listing, never joined
/// onto a directory — see [`crate::serve::board_by_id`], where that shape *is* the traversal
/// defence. So an id spelt `../../etc` is a stem that matches nothing and answers 404 rather
/// than a way out of the data directory, and there is no check here that has to be right.
pub fn rename_target(path: &str) -> Option<&str> {
    let id = path.strip_prefix(PATH_PREFIX)?.strip_suffix(PATH_SUFFIX)?;
    (!id.is_empty()).then_some(id)
}

/// How many bytes of body the request head promises, or why it will not be read.
///
/// ⚠ **Its own function rather than [`crate::sync::content_length`], for the sentences.** The
/// parsing is the same three checks and the cap is the only difference that matters — but
/// sync's refusals say *"a sync request needs a Content-Length"* and *"that sync request is
/// too large"*, and an operator who reads that after a failed **create** goes looking in the
/// wrong file. The refusal text is the entire diagnostic a client gets; it is worth twenty
/// lines to have it name the route the person was actually using.
///
/// [`Refusal`] itself is shared, so the *shape* — refuse before reading, return the decision
/// rather than writing it, let the one caller speak to the socket — is stated once.
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
        return Err(Refusal { status: 400, message: "a board's name needs a Content-Length\n" });
    };
    let Ok(length) = raw.trim().parse::<u64>() else {
        return Err(Refusal { status: 400, message: "that Content-Length is not a number\n" });
    };
    if length > MAX_BODY as u64 {
        return Err(Refusal {
            status: 413,
            message: "that is too long to be a board's name\n",
        });
    }
    // Infallible once the cap above has passed on every target this builds for, and written
    // as a conversion anyway: `usize` is not guaranteed to be 64 bits, and a silent truncation
    // here would read a short body and call it complete.
    let Ok(length) = usize::try_from(length) else {
        return Err(Refusal {
            status: 413,
            message: "that is too long to be a board's name\n",
        });
    };
    Ok(length)
}

/// What the locked section decided, so that the response is written after the lock is gone.
///
/// The same shape as [`crate::sync`]'s `Outcome` and for the same reason: every exit from the
/// board lock is a value rather than a written reply, so no path can hold the lock across a
/// socket write and hand one slow reader the ability to stall every board request for the
/// length of the write timeout.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The board, as the client should see it from now on.
    Board { id: String, title: String },
    /// No board on this server answers to that id.
    NoSuchBoard,
    /// The file is there and holds no document yet.
    Unsaved,
    /// The request was refused, with the sentence to send back.
    Refused(&'static str),
}

/// `POST /api/v1/boards` — make an empty board and say what it is called.
///
/// ⚠ **Deliberately not idempotent, and this is the one surprise worth stating.** A retried
/// create makes a *second* board called `notes-2`, because there is no way to tell a retry
/// from a person who wanted two boards with the same name — and four of this user's own
/// boards are called *Untitled*, so "a name already exists, therefore this is a duplicate
/// request" is a rule that would have refused four legitimate boards. Sync can be idempotent
/// because Loro makes it so; this cannot, so it does not pretend to be. A client that cares
/// should not retry a create it did not see fail.
pub fn create(
    server: &Server,
    body: &[u8],
    caller: Option<&crate::accounts::Caller>,
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let name = match clean_name(body) {
        Ok(name) => name,
        Err(reason) => return respond(stream, 400, "text/plain", reason.as_bytes(), origin),
    };

    let outcome = {
        let _guard = server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        create_one(&server.config.data, &name)
    };
    // ⚠ The guard is gone by this line and the `?` is deliberately out here, exactly as
    // `sync::handle` does it: raising inside the block would hold the board lock across the
    // 500 that `serve.rs` writes, for the length of the write timeout.
    let made = outcome?;
    // ⚠ **Whoever made it owns it**, recorded after the board exists and outside the lock the
    // creation held. A failure here is logged and not fatal: an unrecorded board belongs to
    // the founder, so the cost is "the owner owns it" rather than "nobody can open it".
    if let (Outcome::Board { id, .. }, Some(caller)) = (&made, caller) {
        crate::accounts::claim_board(server, id, caller);
    }
    answer(stream, made, origin)
}

/// `POST /api/v1/boards/{id}/rename` — change what a board is called.
///
/// # ⚠ The title inside the file, never the file's own name
///
/// Two reasons, and the second is the one that decided it.
///
/// **The extension.** `.vellum` is the board extension and [`vellum_store::BoardDb::open`]
/// refuses anything else, so any spelling of a filename change that drops or alters it
/// orphans the board — it stops being listed, stops being openable, and looks deleted while
/// sitting perfectly intact on disk. RULE ZERO names this as the sharp edge by name.
///
/// **The id.** A board's id in this API *is* its file stem — that is what
/// [`crate::serve::board_by_id`] matches and what `boards_json` hands out. So renaming the
/// file renames the board's identity: every tab already open on it 404s on its next sync, and
/// a client with unsent edits has nowhere to send them. A rename is the one gesture a person
/// makes *while looking at* a board, which is the worst possible moment to invalidate its
/// address.
///
/// So this does what the desktop app's `Library::rename` does, and `library.rs`'s own header
/// argues it from a third direction: Miro's rename changes the name on the row and moves no
/// file, and the desktop's folders refer to boards by path, so renaming a file would silently
/// empty every folder the board was in.
///
/// The title is what the library row reads — [`vellum_store::BoardDb::save`] writes it into
/// the board index — so the new name appears in `GET /api/v1/boards` with no extra
/// bookkeeping, on the desktop as well as in the browser.
pub fn rename_board(
    server: &Server,
    id: &str,
    body: &[u8],
    stream: &TcpStream,
) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let title = match clean_name(body) {
        Ok(title) => title,
        Err(reason) => return respond(stream, 400, "text/plain", reason.as_bytes(), origin),
    };

    let outcome = {
        let _guard = server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        set_board_title(&server.config.data, id, &title)
    };
    answer(stream, outcome?, origin)
}

/// Write the response for a finished [`Outcome`]. Called with the board lock already dropped.
fn answer(stream: &TcpStream, outcome: Outcome, origin: Option<&str>) -> anyhow::Result<()> {
    match outcome {
        Outcome::Board { id, title } => {
            let body = format!("{{\"id\":{},\"title\":{}}}", json_string(&id), json_string(&title));
            // ⚠ **200, not 201.** `serve::respond` maps a status to its reason phrase from a
            // fixed list, and an unlisted number falls through to *"Internal Server Error"* —
            // so answering 201 would put `HTTP/1.1 201 Internal Server Error` on the wire,
            // which is valid HTTP saying two different things and is exactly the sort of line
            // that costs somebody an afternoon. The right fix is a 201 arm in that match; the
            // right thing to do from a module that does not own that file is to use a status
            // it already speaks.
            respond(stream, 200, "application/json", body.as_bytes(), origin)
        }
        Outcome::NoSuchBoard => respond(stream, 404, "text/plain", b"no such board\n", origin),
        // Deliberately not "then save one into it". A `.vellum` with no document in it is a
        // board the desktop app has made and not yet written, and writing a fresh `Board` into
        // it would be this server inventing content in a file somebody else is using — the
        // same argument `sync.rs` makes at its own `Unsaved` arm, where the cost of getting it
        // wrong is a stranger's history merged into an empty board.
        Outcome::Unsaved => {
            respond(stream, 404, "text/plain", b"that board holds no snapshot\n", origin)
        }
        Outcome::Refused(reason) => respond(stream, 400, "text/plain", reason.as_bytes(), origin),
    }
}

/// Make the board. Runs with the board lock held and touches no socket.
fn create_one(data: &Path, name: &str) -> anyhow::Result<Outcome> {
    let Some(path) = free_stem(data, &slug(name)) else {
        return Ok(Outcome::Refused(
            "there are already too many boards with names like that one\n",
        ));
    };

    // This is the only write in `velmd` that brings a file into being, and `free_stem` above
    // has already picked a name nothing is using. What follows is what makes that impossible
    // to be wrong about.
    //
    // RULE ZERO: `create_new` is `O_CREAT | O_EXCL` — if anything at all is at this path (a
    // board, a stale sidecar, a dangling symlink) it **fails** rather than opening or
    // truncating it. It is the opposite of the writes this scan exists to catch, and the
    // guarantee is the kernel's rather than this module's to remember.
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => drop(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // The guarantee firing. Under the board lock this should be unreachable, so it is
            // reported rather than retried: something outside this process is writing into the
            // data directory, and quietly picking another name would hide the one fact worth
            // knowing — most likely that `--data` is pointed somewhere it should not be.
            anyhow::bail!(
                "{} appeared between choosing the name and claiming it — \
                 something else is writing into this data directory",
                printable(&path.display().to_string())
            );
        }
        Err(error) => {
            anyhow::bail!("cannot create {}: {error}", printable(&path.display().to_string()))
        }
    }
    // A zero-byte file is a valid empty SQLite database, so `BoardDb::open` initialises it
    // from here exactly as it would a path that did not exist.
    //
    // ⚠ If anything below fails, that empty file **stays**, and it is not litter this program
    // is allowed to sweep up: removing a `.vellum` is the one thing `velmd` must never do,
    // and a rule with an exception for files it believes it created is a rule with a hole in
    // it. The cost is visible and small — `list_boards` skips a board with no index row, so
    // it appears in no listing, and the next create simply steps past it to `-2`.
    let mut board = Board::new();
    board.set_title(name)?;
    let mut db = BoardDb::open(&path)?;
    db.save(&board)?;
    // Explicit rather than left to `Drop`, which discards the result — `sync.rs`'s reasoning:
    // a failure to record the session as closed means the next open reports a crash on a board
    // that is perfectly fine.
    db.close()?;

    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow::anyhow!("the new board's name is not valid UTF-8"))?;
    Ok(Outcome::Board { id: id.to_owned(), title: name.to_owned() })
}

/// Set the board's title. Runs with the board lock held and touches no socket.
fn set_board_title(data: &Path, id: &str, title: &str) -> anyhow::Result<Outcome> {
    let Some(path) = board_by_id(data, id) else {
        return Ok(Outcome::NoSuchBoard);
    };
    let mut db = BoardDb::open(&path)?;
    let Some(mut board) = db.load()? else {
        return Ok(Outcome::Unsaved);
    };

    // ⚠ **A rename is a change from the web, so it takes the same restore point sync does.**
    // `serve.rs`'s header promises "a labelled restore point before the first change a board
    // ever receives from the web", and a board whose *first* web change is a rename would
    // otherwise have that sentence be false about it — the `locked: false` trap, where a true
    // statement is invalidated by a change in a different function and breaks no test. It is
    // taken once per board ever, so a board that has already synced pays nothing here.
    before_the_web(&mut db, &board)?;
    board.set_title(title)?;

    // Deliberately **not** `sync`'s `readable_afterwards`. That check exists for bytes a
    // stranger merged in — specifically a foreign write of the schema marker, which would make
    // the board refuse to open days later. This writes one key, from this process, with a
    // string that has already been validated; there is no path from a title to an unreadable
    // document, and a full serialise-and-parse of the whole board to rename it would be the
    // most expensive thing in the route by an order of magnitude.
    db.save(&board)?;
    db.close()?;

    Ok(Outcome::Board { id: id.to_owned(), title: title.to_owned() })
}

/// A board name from a request body, or the sentence to refuse it with.
///
/// This runs before anything is opened or created, and it is the only place a name off the
/// wire is judged. What it is guarding is worth being precise about, because three of the four
/// sinks are already safe by construction and one is not:
///
/// - **The filename** cannot be reached from here at all: [`slug`] reduces a name to ASCII
///   letters, digits and dashes, so there is no spelling of `..`, `/` or a NUL that survives
///   into a path. The refusals below are therefore a *second* answer to that hazard rather
///   than the only one — deliberately, because a guard at the door does not depend on `slug`
///   staying the way it is, and this is the door.
/// - **JSON** is escaped by `serve::json_string`.
/// - **The log** goes through `serve::printable`, which is what handles the bidirectional and
///   zero-width characters that `char::is_control` says nothing about.
/// - **The board's own title** is the one with no downstream escape, and it is why control
///   characters are refused here rather than merely rendered safely: a title with a newline in
///   it is stored, listed, drawn on the desktop and exported, and no sink can undo it.
///
/// ⚠ **Stricter than the desktop's `Library::create`, in one way worth knowing.** That one
/// accepts `Engine bay / wiring` — its own doc uses that example — because `slug` flattens the
/// slash. This refuses it. The trade is deliberate: a name that arrives over a network reaches
/// a filename, a log and a response, and refusing the separators outright costs one exotic
/// title against a guard that cannot be undone by a refactor two crates away. Someone who
/// wants that title can still set it in the desktop app, and the board is not harmed by
/// arriving as `Engine bay - wiring`.
fn clean_name(body: &[u8]) -> Result<String, &'static str> {
    let Ok(text) = std::str::from_utf8(body) else {
        return Err("a board's name must be UTF-8 text\n");
    };
    // ⚠ **Trimmed first, and it is not cosmetic.** `curl --data-binary` and every shell
    // here-string put a trailing newline on the body, so validating before trimming would
    // refuse a perfectly good request as containing a control character — a 400 whose message
    // describes a problem the caller cannot see. The desktop's `Library::create` trims too, so
    // a board named the same way from either place lands on the same stored string.
    let name = text.trim();
    if name.is_empty() {
        // ⚠ Refused rather than defaulted. `Library::create` substitutes *"Untitled board"* for
        // an empty title, which is right for a button somebody pressed by accident; this is an
        // API, and a client that sent an empty body has a bug that a silent default would hide
        // behind four boards called *Untitled*.
        return Err("a board needs a name\n");
    }
    if name.chars().count() > MAX_NAME {
        return Err("that name is too long for a board\n");
    }
    if name.chars().any(char::is_control) {
        return Err("a board's name cannot contain control characters\n");
    }
    if name.contains('/') || name.contains('\\') {
        return Err("a board's name cannot contain a path separator\n");
    }
    if name.contains("..") {
        return Err("a board's name cannot contain '..'\n");
    }
    Ok(name.to_owned())
}

/// A file-name stem from a board title.
///
/// A deliberate copy of `vellum-app`'s `library::slug`, semantics for semantics, so a board
/// created from the browser is named on disk exactly as one created from the desktop. It is
/// copied rather than shared because this crate must never depend on `vellum-app` — that
/// would drag winit, wgpu and egui onto a headless server, which `Cargo.toml`'s own comment
/// settles.
///
/// Conservative on purpose: the result is a path on two operating systems with different
/// reserved characters. Anything that is not an ASCII letter or digit becomes a dash, runs
/// collapse, and an empty result falls back to a fixed name — so a board called `日本語` lands
/// as `board`, `board-2`, and is titled correctly inside the file, which is where the title
/// lives.
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
    if trimmed.is_empty() { "board".to_owned() } else { trimmed.chars().take(64).collect() }
}

/// The first path under `data` built from `stem` that nothing is using.
///
/// `notes.vellum`, then `notes-2.vellum`, then `notes-3.vellum` — the desktop app's
/// `Library::free_path` rule, and the reason it exists is in RULE ZERO: four of this user's
/// boards are called *Untitled*, so a create that reused a stem would open somebody's board
/// and append a foreign document to it.
///
/// ⚠ **Three files decide whether a stem is taken, not one.** A `-wal` or `-shm` left beside
/// a board that is gone is not inert: SQLite replays a write-ahead log into whatever database
/// it finds under the matching name, so creating `notes.vellum` next to an orphaned
/// `notes.vellum-wal` produces a "new" board carrying content from the old one — resurrected
/// or corrupt, and unexplainable from the outside. CLAUDE.md records the same trap from the
/// other side: *"reset a scratch board with `rm -f x.vellum*`, not `rm x.vellum`"*.
///
/// ⚠ **`symlink_metadata`, not `exists`.** `Path::exists` follows symlinks, so it answers
/// *false* for a dangling one — and then the create would follow that link and write wherever
/// it pointed. This asks whether anything is at the path at all. The exclusive create in
/// [`create_one`] would catch it too; a guard is worth having in the place that is *choosing*
/// the name as well as in the place that claims it, because only one of the two can offer a
/// different name instead of failing.
fn free_stem(data: &Path, stem: &str) -> Option<PathBuf> {
    let first = data.join(format!("{stem}.{BOARD_EXTENSION}"));
    if !taken(&first) {
        return Some(first);
    }
    (2..=MAX_SUFFIX)
        .map(|n| data.join(format!("{stem}-{n}.{BOARD_EXTENSION}")))
        .find(|candidate| !taken(candidate))
}

/// Whether anything at all occupies this board's name — the file or either SQLite sidecar.
fn taken(path: &Path) -> bool {
    if path.symlink_metadata().is_ok() {
        return true;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        // A name this program built from `slug` is always valid UTF-8, so this is unreachable
        // — and it answers "taken", because the failure direction of a guard that picks a file
        // to create must be to pick a different one.
        return true;
    };
    ["-wal", "-shm"]
        .iter()
        .any(|suffix| path.with_file_name(format!("{name}{suffix}")).symlink_metadata().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory nothing else is using.
    ///
    /// `manifest.rs`'s helper, verbatim in shape and for both of its reasons. **Named from an
    /// atomic counter, not the clock** — CLAUDE.md records a fixture that derived its scratch
    /// path from `unix_now()` in *seconds*, so two tests starting in the same second shared a
    /// directory and the first to finish deleted the other's data. `cargo test` is a parallel
    /// runner. And **nothing is removed, not even here**: the counter makes each directory
    /// unique, so there is no stale state to clear, which is what lets `tests/rule_zero.rs`
    /// grep this whole crate for `remove_` and expect zero hits with no exemption for tests.
    ///
    /// It takes a name rather than nothing for a second reason worth knowing: a zero-argument
    /// `fn` at a test module's top level is indistinguishable from a test that has lost its
    /// `#[test]`, and `every_test_in_this_crate_still_has_its_attribute` reports one as an
    /// orphan. A helper takes arguments; that is the rule that tells them apart.
    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let unique = format!("velmd-manage-{name}-{n}-{}", std::process::id());
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_create_route_is_the_board_list_path_and_nothing_else() {
        assert!(is_create("/api/v1/boards"));
        assert!(!is_create("/api/v1/boards/"));
        assert!(!is_create("/api/v1/boards/notes/rename"));
        assert!(!is_create("/api/v1/health"));
        // ⚠ Under `/api/v1/`, which is what puts it behind `serve::needs_token`. Asserted in
        // the file that spells the path, so a change here cannot quietly publish the route.
        assert!(PATH_BOARDS.starts_with("/api/v1/"));
        assert!(PATH_PREFIX.starts_with("/api/v1/"));
    }

    #[test]
    fn a_rename_path_yields_the_id_between_its_ends() {
        assert_eq!(rename_target("/api/v1/boards/notes/rename"), Some("notes"));
        // Real board names have spaces in them and arrive percent-encoded; the decode happens
        // in `serve.rs` before this is called, so both spellings must survive the split.
        assert_eq!(rename_target("/api/v1/boards/BMW 2020/rename"), Some("BMW 2020"));
        assert_eq!(rename_target("/api/v1/boards/BMW%202020/rename"), Some("BMW%202020"));
        assert_eq!(rename_target("/api/v1/boards//rename"), None);
        assert_eq!(rename_target("/api/v1/boards/notes/sync"), None);
        assert_eq!(rename_target("/api/v1/boards"), None);
    }

    /// The traversal defence is `board_by_id`'s shape, not a check here — so what this asserts
    /// is that a hostile id is *carried through unchanged* rather than sanitised into something
    /// that might match. A `..` that arrived here as an id is a stem no directory listing
    /// contains, and answers 404.
    #[test]
    fn a_hostile_id_is_passed_through_rather_than_repaired() {
        let hostile = "/api/v1/boards/../../etc/passwd/rename";
        assert_eq!(rename_target(hostile), Some("../../etc/passwd"));
        let dir = scratch("hostile-id");
        assert!(board_by_id(&dir, "../../etc/passwd").is_none());
    }

    #[test]
    fn a_body_is_refused_before_it_is_read_when_it_is_too_long_to_be_a_name() {
        let with = |value: &str| {
            let mut headers = BTreeMap::new();
            headers.insert("content-length".to_owned(), value.to_owned());
            content_length(&headers)
        };
        assert_eq!(with("12").ok(), Some(12));
        assert_eq!(with(&(MAX_BODY + 1).to_string()).map_err(|r| r.status), Err(413));
        // ⚠ The cap that matters is *this* module's, not sync's. A body sized for a CRDT delta
        // must not be accepted by a route that reads a word — measured here rather than
        // asserted in prose, because the two constants live in different files.
        assert_eq!(with(&crate::sync::MAX_BODY.to_string()).map_err(|r| r.status), Err(413));
        assert_eq!(with("not a number").map_err(|r| r.status), Err(400));
        assert!(content_length(&BTreeMap::new()).is_err());

        let mut chunked = BTreeMap::new();
        chunked.insert("transfer-encoding".to_owned(), "chunked".to_owned());
        assert_eq!(content_length(&chunked).map_err(|r| r.status), Err(400));
    }

    #[test]
    fn a_name_is_trimmed_before_it_is_judged() {
        // The trailing newline every `curl --data-binary` and here-string adds. Validating
        // first would refuse this as a control character, which is a 400 describing a problem
        // the caller cannot see.
        assert_eq!(clean_name(b"Rear seats\n").as_deref(), Ok("Rear seats"));
        assert_eq!(clean_name(b"  spaced  ").as_deref(), Ok("spaced"));
    }

    #[test]
    fn a_name_that_could_reach_a_path_or_a_log_is_refused() {
        for hostile in [
            &b"a/b"[..],
            &b"a\\b"[..],
            &b".."[..],
            &b"../../etc/passwd"[..],
            &b"notes/../../etc"[..],
            &b"a\0b"[..],
            // A forged log line, and a terminal escape. Both survive `printable`, which is
            // what the log uses — this refuses them one layer earlier, because the board's
            // own stored title has no such filter in front of it.
            &b"ok\nvelmd: token accepted"[..],
            &b"ok\x1b[2Jcleared"[..],
        ] {
            assert!(
                clean_name(hostile).is_err(),
                "{:?} was accepted as a board name",
                String::from_utf8_lossy(hostile)
            );
        }
        assert!(clean_name(b"").is_err(), "an empty body must be refused, not defaulted");
        assert!(clean_name(b"   ").is_err());
        assert!(clean_name(&[0xC3, 0x28]).is_err(), "a name must be UTF-8");
        assert!(clean_name("x".repeat(MAX_NAME + 1).as_bytes()).is_err());
        // And the ordinary case still passes, or the guard above is just a refusal. An em
        // dash and an ampersand are neither control characters nor separators, and a board
        // named with them is exactly the sort of title this must not become an opinion about.
        let ordinary = "Rear seats — trim & fit";
        assert_eq!(clean_name(ordinary.as_bytes()).as_deref(), Ok(ordinary));
    }

    /// The desktop's own examples, so a board created from the browser lands on the same file
    /// name as one created beside it.
    #[test]
    fn a_stem_is_built_the_way_the_desktop_builds_one() {
        assert_eq!(slug("Rear seats"), "rear-seats");
        assert_eq!(slug("Engine bay / wiring"), "engine-bay-wiring");
        assert_eq!(slug("BMW 2020 530i g30"), "bmw-2020-530i-g30");
        assert_eq!(slug("日本語"), "board");
        assert_eq!(slug("   "), "board");
        assert_eq!(slug(&"x".repeat(200)).chars().count(), 64);
    }

    /// ⚠ The whole of RULE ZERO on this route, measured: a create that finds its first choice
    /// taken must step past it and must leave what is there byte for byte.
    #[test]
    fn a_create_never_lands_on_a_board_that_is_already_there() {
        let dir = scratch("free-stem");
        let existing = dir.join("notes.vellum");
        std::fs::write(&existing, b"pretend this is an irreplaceable board").unwrap();
        let before = std::fs::read(&existing).unwrap();

        let first = free_stem(&dir, "notes").expect("a free stem");
        assert_eq!(first.file_name().unwrap(), "notes-2.vellum");

        // And again, with the second name taken as well.
        std::fs::write(dir.join("notes-2.vellum"), b"another one").unwrap();
        let third = free_stem(&dir, "notes").expect("a free stem");
        assert_eq!(third.file_name().unwrap(), "notes-3.vellum");

        assert_eq!(std::fs::read(&existing).unwrap(), before, "the board that was there changed");
    }

    /// ⚠ A/B for the sidecar half. With only `path.exists()`, this passes and produces a board
    /// that SQLite will replay a stranger's write-ahead log into.
    #[test]
    fn a_stem_is_taken_when_only_its_write_ahead_log_is_there() {
        let dir = scratch("wal");
        std::fs::write(dir.join("notes.vellum-wal"), b"an orphaned log").unwrap();
        assert!(!dir.join("notes.vellum").exists(), "the board itself is deliberately absent");
        let chosen = free_stem(&dir, "notes").expect("a free stem");
        assert_eq!(chosen.file_name().unwrap(), "notes-2.vellum");

        let other = scratch("shm");
        std::fs::write(other.join("notes.vellum-shm"), b"an orphaned index").unwrap();
        assert_eq!(
            free_stem(&other, "notes").unwrap().file_name().unwrap(),
            "notes-2.vellum"
        );
    }

    /// The claim in [`create_one`]'s RULE ZERO note, asserted rather than argued: `create_new`
    /// refuses an occupied path instead of truncating it. This is the guarantee the whole
    /// create path rests on, and it is one line of `std` — so it is worth one test, because a
    /// flag dropped in a refactor would leave a build that still compiles and still passes
    /// every other test here.
    #[test]
    fn claiming_a_file_exclusively_refuses_rather_than_truncates() {
        let dir = scratch("exclusive");
        let occupied = dir.join("notes.vellum");
        std::fs::write(&occupied, b"irreplaceable").unwrap();

        let refused = std::fs::OpenOptions::new().write(true).create_new(true).open(&occupied);
        assert_eq!(refused.unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&occupied).unwrap(), b"irreplaceable");
    }

    #[test]
    fn a_created_board_is_empty_saved_and_carries_the_name_it_was_given() {
        let dir = scratch("create");
        let Outcome::Board { id, title } = create_one(&dir, "Rear seats").unwrap() else {
            panic!("the create did not answer with a board");
        };
        assert_eq!(id, "rear-seats");
        assert_eq!(title, "Rear seats");

        // Saved, or it is a file rather than a board: `list_boards` skips a `.vellum` with no
        // index row, so an unsaved create would answer with an id that 404s on the next
        // request. This is the assertion that makes the id honest.
        let path = board_by_id(&dir, "rear-seats").expect("the new board is listed");
        let mut db = BoardDb::open(&path).unwrap();
        let board = db.load().unwrap().expect("the new board holds a document");
        assert_eq!(board.title(), "Rear seats");
        assert!(board.items().unwrap().is_empty(), "a new board is empty");
    }

    #[test]
    fn two_boards_asked_for_the_same_name_are_two_boards() {
        let dir = scratch("duplicate-name");
        let first = create_one(&dir, "Untitled").unwrap();
        let second = create_one(&dir, "Untitled").unwrap();
        let (Outcome::Board { id: a, .. }, Outcome::Board { id: b, .. }) = (first, second) else {
            panic!("both creates should have produced a board");
        };
        assert_eq!((a.as_str(), b.as_str()), ("untitled", "untitled-2"));
        // Both are real, listed boards — four of this user's boards are called *Untitled*, so
        // the second one must not have been folded into the first.
        assert!(board_by_id(&dir, "untitled").is_some());
        assert!(board_by_id(&dir, "untitled-2").is_some());
    }

    #[test]
    fn renaming_changes_the_title_and_leaves_the_file_where_it_was() {
        let dir = scratch("rename-title");
        let Outcome::Board { id, .. } = create_one(&dir, "Rear seats").unwrap() else {
            panic!("the board was not created");
        };
        let before = board_by_id(&dir, &id).expect("the board is listed");

        let outcome = set_board_title(&dir, &id, "Front seats").unwrap();
        assert_eq!(
            outcome,
            Outcome::Board { id: id.clone(), title: "Front seats".to_owned() },
            "a rename must answer with the id the client already has"
        );

        // ⚠ The whole decision, in two assertions: the file did not move, so every open tab's
        // id still resolves — and the title did change, so `GET /api/v1/boards` shows it.
        let after = board_by_id(&dir, &id).expect("the board is still listed under its id");
        assert_eq!(after, before, "the file moved, which would orphan every open tab");
        let mut db = BoardDb::open(&after).unwrap();
        assert_eq!(db.load().unwrap().unwrap().title(), "Front seats");
    }

    /// ⚠ `serve.rs`'s header promises a labelled restore point before the *first* change a
    /// board ever receives from the web. A rename is such a change, so a board whose first web
    /// change is a rename must have one — otherwise that sentence is false about exactly the
    /// board nobody thought to check.
    #[test]
    fn the_first_rename_from_the_web_takes_the_restore_point_sync_promises() {
        let dir = scratch("restore-point");
        let Outcome::Board { id, .. } = create_one(&dir, "Notes").unwrap() else {
            panic!("the board was not created");
        };
        let path = board_by_id(&dir, &id).unwrap();
        assert!(BoardDb::open(&path).unwrap().restore_points().unwrap().is_empty());

        set_board_title(&dir, &id, "Notes, revised").unwrap();
        let points = BoardDb::open(&path).unwrap().restore_points().unwrap();
        assert_eq!(points.len(), 1, "the first change from the web left no restore point");
        assert!(points[0].label.is_some(), "an automatic point is prunable; this one must not be");

        // Once per board **ever**, not once per change: a labelled point is never pruned, so a
        // point per rename would grow an irreplaceable board's file without bound.
        set_board_title(&dir, &id, "Notes, revised twice").unwrap();
        assert_eq!(BoardDb::open(&path).unwrap().restore_points().unwrap().len(), 1);
    }

    #[test]
    fn renaming_a_board_that_is_not_there_creates_nothing() {
        let dir = scratch("missing-board");
        assert_eq!(set_board_title(&dir, "ghost", "Named").unwrap(), Outcome::NoSuchBoard);
        // The 404 must not be a create by another name — RULE ZERO's other direction.
        assert!(!dir.join("ghost.vellum").exists());
        assert!(!dir.join("named.vellum").exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// A `.vellum` the desktop app has made and not yet written holds no document. Writing a
    /// fresh one into it would be this server inventing content in somebody else's file.
    #[test]
    fn renaming_a_board_with_no_document_in_it_is_refused_rather_than_filled_in() {
        let dir = scratch("unsaved-board");
        let path = dir.join("blank.vellum");
        BoardDb::open(&path).unwrap().close().unwrap();

        // It is not listed, because `list_boards` reads the index row a save writes — so the
        // route answers 404 for the same reason it answers 404 for a board that is not there.
        assert!(board_by_id(&dir, "blank").is_none());
        assert_eq!(set_board_title(&dir, "blank", "Named").unwrap(), Outcome::NoSuchBoard);
        assert!(BoardDb::open(&path).unwrap().load().unwrap().is_none(), "it was written into");
    }
}
