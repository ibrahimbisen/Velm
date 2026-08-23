//! The board library, with the desktop app's own filing on it.
//!
//! ```text
//! GET /api/v1/library  ->  {"boards":[…],"spaces":[…]}
//! ```
//!
//! `/api/v1/boards` answers which boards exist and nothing about how they are filed. This
//! route adds the three things the desktop's start screen is built from — which boards are
//! starred, which folder each is in, and which are in Recently deleted — so a browser can
//! draw the same page.
//!
//! **A second route rather than four more keys on the first one.** `/api/v1/boards` returns a
//! bare array and this returns an object; a tab somebody already has open is parsing the
//! array, and widening a shape in place breaks it the moment the server is updated under it.
//! Two routes cost one `match` arm. Older clients keep the route they were written against,
//! and it is not deprecated — it is the cheaper answer when all a caller wants is the list.
//!
//! Boards come back in [`list_boards`] order, which is most recently modified first. The
//! banding into Recently opened / Pinned / All boards is the client's; this is the data.
//!
//! # 🛑 No path ever crosses the wire
//!
//! This is the one rule in this module and everything else follows from it.
//!
//! `library.json` records the user's filing as **absolute paths on the machine that wrote
//! it** — every star, every folder membership and every trash record is a full path under
//! their home directory. Sending one would put their account name and their directory layout
//! into a browser, into a URL bar, and quite possibly into a screenshot of a board they meant
//! to share. So a path is turned into a **file stem** by [`stem_of`] the moment it is read,
//! and the path is dropped there.
//!
//! A stem is not a compromise, it is already this API's name for a board: `/api/v1/boards`
//! hands out stems, `serve::board_by_id` matches against stems, and the rename route
//! addresses a board by its stem. There is nothing new on the wire here.
//!
//! # ⚠ The filing is matched by stem, never by directory
//!
//! `--data` is a *copy*, somewhere else, quite possibly on another machine. Every path in the
//! sidecar names the desktop's own directory — so "is this filed board in my data
//! directory?", answered by comparing an entry's parent against `--data`, is **false for
//! every entry on every server**. The route would come back with no stars and no folders
//! while looking perfectly healthy, which is the worst way for this to fail: nothing to
//! notice, nothing logged, and a start screen that quietly says the user has filed nothing.
//!
//! What is compared instead is the stem, against the stems [`list_boards`] just produced.
//! That is also what drops a stale entry — a board deleted on the Mac, or filing carried
//! across from another machine — with no separate liveness check to keep in step. The
//! desktop's `Library::rescan` prunes the same way and for the reason it states: a folder
//! listing a board that is not there shows a count that never matches its contents.
//!
//! The honest cost, stated rather than hidden: a stale entry whose stem happens to equal a
//! *different* board here will lend that board its star or its folder. Accepted, because a
//! board's identity in this API **is** its file stem everywhere else too, and because the
//! filing is decoration on a list nobody can edit through this route.
//!
//! # 🛑 RULE ZERO — nothing here writes
//!
//! `std::fs::read` and `serde_json::from_slice`, and that is the whole of the I/O besides
//! [`list_boards`]. The sidecar is never written back: velmd does not own the user's filing,
//! it reports it. Nothing in this module removes, moves or truncates a file, so there is no
//! call here for `tests/rule_zero.rs` to catch and none of its `RULE ZERO:` acknowledgements
//! are needed.
//!
//! ⚠ [`list_boards`] does open every board with SQLite, which is not a passive act — see
//! `serve.rs`'s own header. That is why this takes the same board lock `/api/v1/boards` does.

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use vellum_store::{BoardIndex, list_boards};

use crate::serve::{Server, json_string, printable, respond};

// ----- the sidecar, reduced to the three keys a board list cares about ---------------------

/// The desktop's `library.json`, as much of it as this route reads.
///
/// ⚠ **`#[serde(default)]` on the container and no `deny_unknown_fields`, and both halves are
/// load-bearing.** The real file carries about twenty other keys — the theme, the accent, the
/// grid colour, the agent layer's settings, the attached archives — and it gains more with
/// every release of the desktop app. Unknown keys being ignored is what lets a sidecar
/// written by a *newer* build parse here; the container default is what lets one written by
/// an *older* build parse, from before `trashed` existed at all. Either one alone turns a
/// version skew between the Mac and the server into a start screen with no folders on it.
///
/// ⚠ **The key is `spaces` and it must stay `spaces`.** The user calls them Folders and every
/// visible string in the desktop says Folder, but the serialised name deliberately did not
/// move — the desktop's own `Filing` spells out why at length, and the short version is that
/// serde reads a renamed field as *absent*. Here that would silently answer "no folders" on a
/// machine that has four; on the desktop, which writes the file back, it erased the filing.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Filing {
    spaces: Vec<SpaceRecord>,
    starred: Vec<PathBuf>,
    trashed: Vec<TrashedBoard>,
}

/// One folder: its name, the boards filed under it, and whether it is pinned.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpaceRecord {
    name: String,
    boards: Vec<PathBuf>,
    pinned: bool,
}

/// One board in Recently deleted.
///
/// The desktop's trash moves nothing: deleting records a path and a time here and the
/// `.vellum` stays byte for byte where it is. That is why a trashed board is still in
/// [`list_boards`]' answer and still gets a row below — it is a board this server holds,
/// carrying a timestamp that says which band the client should draw it in.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TrashedBoard {
    path: PathBuf,
    /// ⚠ **Seconds since the epoch, not milliseconds.**
    ///
    /// That is what the desktop writes, and it is sent on **as seconds** so the wire agrees
    /// with the file rather than converting quietly in one direction only. `modified` on the
    /// same row is **milliseconds**, because that is what `/api/v1/boards` has always sent
    /// and what the client already parses.
    ///
    /// Two units in one object is a thing to say loudly rather than leave for a reader to
    /// discover. A seconds value read as milliseconds lands in 1970; a milliseconds value
    /// read as seconds lands fifty thousand years out. CLAUDE.md records the same shape
    /// costing a message pulse its whole animation — `unix_now() * 1_000` is a clock that
    /// ticks once a second, and nothing in a type system says so.
    at: u64,
}

/// A filed path reduced to the id this API uses, or nothing.
///
/// The path is dropped here and nowhere else, which is what makes *"no path crosses the
/// wire"* checkable by reading one function instead of the whole module.
fn stem_of(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|s| s.to_str())
}

/// The desktop's filing, or none of it — never an error.
///
/// ⚠ **Three failures, one answer, and it is always 200.** `--data` may be a bare folder of
/// `.vellum` files with no sidecar in it at all — that is what `velmd snapshot`, a
/// hand-assembled directory and a migration rooted somewhere else all produce. The file may
/// be half-written. It may be JSON this build cannot make sense of. In every one of those
/// cases the boards are still there and still readable, and a board list that refuses because
/// a *preference file* would not parse is this program failing at the one job it has.
///
/// The desktop's `Library::open` takes exactly this line and says why: the filing is a
/// convenience and the boards are the data. A sidecar that will not parse is logged and
/// replaced by the defaults, which is recoverable; refusing to start is not.
fn read_filing(data: &Path) -> Filing {
    let sidecar = data.join("library.json");
    let Ok(bytes) = std::fs::read(&sidecar) else {
        // Not logged. A directory with no sidecar is the ordinary case for every `--data`
        // that was not a whole-directory migration, and a warning printed on every request
        // for a state that is completely normal is a warning people stop reading.
        return Filing::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        // Said out loud rather than swallowed: an operator whose folders are mysteriously
        // missing has no other way to learn that the sidecar did not parse.
        //
        // Through `printable`, like every other line this server logs. The reasoning that
        // this particular string cannot carry a control character is exactly the reasoning
        // that shipped a log injection once — a doc comment argued a terminal escape could
        // not reach a logger, and it was true about the upstream parser and false about the
        // function two hops later. Sanitising costs one call and needs no argument.
        eprintln!(
            "velmd: {} could not be read: {}",
            printable(&sidecar.display().to_string()),
            printable(&error.to_string())
        );
        Filing::default()
    })
}

// ----- rendering --------------------------------------------------------------------------

/// The whole answer, as JSON, from a board list and a filing.
///
/// Pure and separate from [`handle`] so it can be tested without a socket, a data directory
/// or SQLite — every field of [`BoardIndex`] is public, so a fixture is a struct literal.
/// That matters more than usual here: the two things most worth asserting are that a stale
/// entry is dropped and that no path appears anywhere in the output, and neither of those
/// needs a real board to be true.
fn render(indexes: &[BoardIndex], filing: &Filing) -> String {
    // Every id this server can actually serve. Intersecting against it is the liveness check,
    // the staleness check and the traversal defence at once — a stem that came out of a
    // directory listing cannot name anything outside that directory.
    let live: BTreeSet<&str> = indexes.iter().filter_map(|index| stem_of(&index.path)).collect();

    let starred: BTreeSet<&str> = filing
        .starred
        .iter()
        .filter_map(|path| stem_of(path))
        .filter(|id| live.contains(id))
        .collect();

    let trashed: BTreeMap<&str, u64> = filing
        .trashed
        .iter()
        .filter_map(|entry| stem_of(&entry.path).map(|id| (id, entry.at)))
        .filter(|(id, _)| live.contains(id))
        .collect();

    // A board is in at most one folder — the desktop's `move_to_space` takes it out of every
    // other one first, because the user's real folders are a filing scheme and not tags. A
    // hand-edited sidecar could still list one twice, so **the first record wins** rather than
    // the last: one answer, arrived at the same way every time. The folder counts below are
    // taken from this map rather than from `record.boards`, which is what stops a row's count
    // disagreeing with the rows that name it.
    let mut space_of: BTreeMap<&str, &str> = BTreeMap::new();
    for record in &filing.spaces {
        let name = record.name.trim();
        // An unnamed folder is skipped. `create_space` refuses an empty name, so this only
        // ever fires on a hand-edited file — and a nameless row in a sidebar is something the
        // user cannot click, cannot identify and cannot get rid of from the browser.
        if name.is_empty() {
            continue;
        }
        for path in &record.boards {
            let Some(id) = stem_of(path) else { continue };
            if !live.contains(id) {
                continue;
            }
            space_of.entry(id).or_insert(name);
        }
    }

    let mut rows = Vec::new();
    for index in indexes {
        let Some(id) = stem_of(&index.path) else { continue };
        // Milliseconds — see the note on `TrashedBoard::at` for why the two clocks on this
        // row differ. Copied from `serve::boards_json` so the two routes cannot come to
        // disagree about a board's date. `0` for a clock the file predates, which the picker
        // renders as no date at all rather than as 1970.
        let modified = index
            .modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let space = match space_of.get(id) {
            Some(name) => json_string(name),
            None => "null".to_owned(),
        };
        let trashed_at = match trashed.get(id) {
            Some(at) => at.to_string(),
            None => "null".to_owned(),
        };
        // Every string goes through `json_string`. A title is arbitrary text off a board file
        // — this user has boards with quotes in the name — and a hand-rolled quote is how one
        // becomes a parse error in a browser that then shows an empty library. The folder name
        // is the user's typing too, and the id is a file stem, which on macOS may hold very
        // nearly anything.
        // ⚠ **The board's own picture, as a blob hash — and it was very nearly left out.** A
        // reviewer found the consumer already written on the client and unreachable, because
        // nothing here emitted the key: `thumbnailUrl`, the `thumbnail` field and the whole
        // `<img>` branch of a card's picture well were tested code with no data, so every card
        // drew the placeholder for ever. This repository's signature defect, arrived at by two
        // agents who were each individually right.
        //
        // A hash, not a URL: `/api/v1/blobs/{hash}` already exists and is already behind the
        // same token, so building the address on the client keeps one definition of where a
        // blob lives. `None` for a board that has never been open long enough to be captured,
        // which the card draws as the placeholder it drew before.
        let thumbnail = index
            .thumbnail
            .as_ref()
            .map_or_else(|| "null".to_owned(), |hash| json_string(&hash.to_string()));
        rows.push(format!(
            "{{\"id\":{},\"title\":{},\"items\":{},\"modified\":{modified},\
             \"starred\":{},\"space\":{space},\"trashed\":{trashed_at},\
             \"thumbnail\":{thumbnail}}}",
            json_string(id),
            json_string(&index.title),
            index.item_count,
            starred.contains(id),
        ));
    }

    // ⚠ **A folder's count is of boards this server holds, that are filed here, and that are
    // *not* in Recently deleted.** All three, and the third is the one easy to miss: the
    // desktop hides a deleted board from every scope but the trash, so a count including one
    // would name a number the client cannot show — the exact "a count that never matches its
    // contents" the desktop's own prune exists to prevent.
    //
    // An empty folder is still listed. It is a folder the user made, and omitting it would
    // make the browser's sidebar disagree with the desktop's for no reason a user could work
    // out — and it is where they would drag a board, if this route were ever writable.
    let mut folders = Vec::new();
    let mut already: BTreeSet<&str> = BTreeSet::new();
    for record in &filing.spaces {
        let name = record.name.trim();
        // Same skip as above, plus a duplicate-name guard. `create_space` refuses a name that
        // is already taken, so two records sharing one is another hand-edited file — and
        // because the count below matches by *name*, an ungated duplicate would draw the
        // folder twice with the same total in both, which reads as the boards existing twice.
        if name.is_empty() || !already.insert(name) {
            continue;
        }
        let count = space_of
            .iter()
            .filter(|&(id, folder)| *folder == name && !trashed.contains_key(id))
            .count();
        folders.push(format!(
            "{{\"name\":{},\"pinned\":{},\"boards\":{count}}}",
            json_string(name),
            record.pinned,
        ));
    }

    format!("{{\"boards\":[{}],\"spaces\":[{}]}}", rows.join(","), folders.join(","))
}

/// The one place this route's address is spelled.
///
/// `paste::PATH`'s precedent: a path written once in the dispatch and once in a doc comment is
/// a path that drifts, and the failure is a 404 nobody can explain because both copies look
/// right on their own.
pub(crate) const PATH: &str = "/api/v1/library";

/// `GET /api/v1/library`.
///
/// Behind the same token as every other `/api/v1/` route — `serve::needs_token` gates on the
/// prefix, and the check runs before `route` is reached at all, so there is nothing to
/// remember here. That is deliberate: a per-route allow-list is a list somebody forgets to
/// add to, and the one this route would be missing from serves the whole library.
pub(crate) fn handle(server: &Server, stream: &TcpStream) -> anyhow::Result<()> {
    let origin = server.config.app_origin.as_deref();
    let body = {
        // The same lock `/api/v1/boards` takes, for the same reason and not for throughput:
        // `list_boards` opens every board with SQLite, and two `BoardDb`s over one file in WAL
        // mode is the arrangement this codebase has already been burned by once. Scoped to the
        // database work and dropped before `respond`, or one slow reader stalls every board
        // request for the length of the write timeout.
        let _guard = server.boards.lock().map_err(|_| anyhow::anyhow!("board lock poisoned"))?;
        let indexes = list_boards(&server.config.data).unwrap_or_default();
        render(&indexes, &read_filing(&server.config.data))
    };
    respond(stream, 200, "application/json", body.as_bytes(), origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    /// A fabricated home directory, deliberately unlike any real one.
    ///
    /// Distinctive on purpose: the leak test asserts this substring is absent from the
    /// output, and a generic fixture path like `/tmp/x` could plausibly be absent for
    /// reasons unrelated to the rule being checked.
    const FILING_ROOT: &str = "/home/nobody/filing-test/boards";

    fn board_at(stem: &str, title: &str, modified_ms: u64) -> BoardIndex {
        BoardIndex {
            path: PathBuf::from(FILING_ROOT).join(format!("{stem}.vellum")),
            title: title.to_owned(),
            item_count: 7,
            thumbnail: None,
            modified: UNIX_EPOCH + Duration::from_millis(modified_ms),
        }
    }

    /// A path as the sidecar would spell it — absolute, on the machine that wrote it.
    fn filed(stem: &str) -> String {
        format!("{FILING_ROOT}/{stem}.vellum")
    }

    fn filing_from(text: &str) -> Filing {
        serde_json::from_str(text).expect("the fixture sidecar is not valid JSON")
    }

    /// The response, parsed.
    ///
    /// ⚠ **Every test goes through this rather than asserting on substrings alone.** A
    /// substring check passes on output with a missing comma or an unescaped quote in it —
    /// which is precisely the bug a hand-built JSON string has — and the client would then
    /// show an empty library against a route whose tests are green. Parsing catches the whole
    /// family at once.
    fn parsed(body: &str) -> serde_json::Value {
        serde_json::from_str(body)
            .unwrap_or_else(|e| panic!("the route emitted JSON no client can parse: {e}\n{body}"))
    }

    fn scratch(name: &str) -> PathBuf {
        // Named from an atomic counter, not the clock. CLAUDE.md records a fixture that
        // derived its scratch directory from a clock in *seconds*, so two tests starting in
        // the same second shared a directory and the first to finish destroyed the other's
        // data — presenting as two sidecar failures that looked like a bug in the sidecar.
        // `cargo test` is a parallel runner; a clock-derived path is a collision waiting.
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // Nothing is cleaned up, not even here. The counter makes each directory unique, so
        // there is no stale state to clear — which lets `tests/rule_zero.rs` grep this whole
        // crate for a removal and expect *zero* hits, test code included. A rule with an
        // exemption for tests is a rule with a hole in it.
        let dir = std::env::temp_dir()
            .join(format!("velmd-library-{name}-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The rule this module exists to keep: the filing crosses the wire, the paths do not.
    #[test]
    fn a_filed_path_never_reaches_the_wire() {
        let boards = [board_at("alpha", "Alpha", 4_000), board_at("beta", "Beta", 3_000)];
        let filing = filing_from(&format!(
            r#"{{"spaces":[{{"name":"Reference","boards":["{}"],"pinned":true}}],
                 "starred":["{}"],
                 "trashed":[{{"path":"{}","at":1785544454}}]}}"#,
            filed("alpha"),
            filed("alpha"),
            filed("beta"),
        ));

        let body = render(&boards, &filing);
        let value = parsed(&body);

        // The filing itself did arrive — without this the assertions below pass on a route
        // that answers with nothing at all, which is the failure they exist to distinguish.
        assert_eq!(value["boards"][0]["starred"].as_bool(), Some(true));
        assert_eq!(value["boards"][0]["space"], "Reference");
        assert_eq!(value["boards"][1]["trashed"].as_u64(), Some(1_785_544_454));

        assert!(!body.contains(FILING_ROOT), "a server path reached the wire:\n{body}");
        assert!(!body.contains("/home/"), "a home directory reached the wire:\n{body}");
        // The extension is the other half of a path and is just as identifying in a URL bar.
        assert!(!body.contains(".vellum"), "a board's file name reached the wire:\n{body}");
    }

    /// Filing that names a board this data directory does not hold is dropped.
    ///
    /// This is the case a `parent == --data` check gets wrong for *every* entry, and it is
    /// also how a board deleted on the Mac stops being counted here.
    #[test]
    fn filing_that_names_a_board_this_server_does_not_hold_is_dropped() {
        let boards = [board_at("alpha", "Alpha", 4_000)];
        // A stem obviously unlike anything in the listing, so the assertion cannot pass by
        // colliding with a live board.
        let filing = filing_from(&format!(
            r#"{{"spaces":[{{"name":"Reference","boards":["{}","{}"],"pinned":false}}],
                 "starred":["{}"],
                 "trashed":[{{"path":"{}","at":1785544454}}]}}"#,
            filed("not-on-this-server"),
            filed("gone-long-ago"),
            filed("not-on-this-server"),
            filed("gone-long-ago"),
        ));

        let value = parsed(&render(&boards, &filing));

        assert_eq!(value["boards"].as_array().map(|a| a.len()), Some(1), "the live board vanished");
        assert_eq!(value["boards"][0]["starred"].as_bool(), Some(false));
        assert!(value["boards"][0]["space"].is_null());
        assert!(value["boards"][0]["trashed"].is_null());
        // The folder is still listed — the user made it — and it counts nothing.
        assert_eq!(value["spaces"][0]["name"], "Reference");
        assert_eq!(value["spaces"][0]["boards"].as_u64(), Some(0));
    }

    /// No sidecar at all is the ordinary case, not an error.
    #[test]
    fn a_sidecar_that_is_absent_reads_as_no_filing_at_all() {
        let dir = scratch("absent");
        let filing = read_filing(&dir);
        assert!(filing.spaces.is_empty() && filing.starred.is_empty() && filing.trashed.is_empty());

        let value = parsed(&render(&[board_at("alpha", "Alpha", 1)], &filing));
        assert_eq!(value["boards"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(value["spaces"].as_array().map(|a| a.len()), Some(0));
    }

    /// A malformed sidecar loses the filing and never the boards.
    #[test]
    fn a_sidecar_that_will_not_parse_still_answers_with_every_board() {
        let dir = scratch("malformed");
        std::fs::write(dir.join("library.json"), b"{\"spaces\": [ truncated").unwrap();

        let filing = read_filing(&dir);
        assert!(filing.spaces.is_empty(), "a half-written sidecar must degrade, not carry junk");

        let boards = [board_at("alpha", "Alpha", 2), board_at("beta", "Beta", 1)];
        let value = parsed(&render(&boards, &filing));
        let count = value["boards"].as_array().map(|a| a.len());
        assert_eq!(count, Some(2), "boards were lost along with the filing");
        assert_eq!(value["spaces"].as_array().map(|a| a.len()), Some(0));
    }

    /// The twenty other keys in a real sidecar, and one no build has ever written.
    ///
    /// This is the test that fails the day somebody adds `deny_unknown_fields` for tidiness,
    /// which would make every future desktop release break this route.
    #[test]
    fn every_other_key_in_a_real_sidecar_is_ignored() {
        let filing = filing_from(&format!(
            r#"{{"theme":"light","accent":"Teal","glass_opacity":160,"minimap":true,
                 "speech":null,"link_previews":null,"attached_archives":[],"start_views":[],
                 "last_board":"{}","a_key_from_a_later_release":{{"nested":[1,2,3]}},
                 "spaces":[{{"name":"Reference","boards":["{}"],"pinned":true}}],
                 "starred":["{}"],
                 "trashed":[]}}"#,
            filed("alpha"),
            filed("alpha"),
            filed("alpha"),
        ));

        let value = parsed(&render(&[board_at("alpha", "Alpha", 1)], &filing));
        assert_eq!(value["boards"][0]["space"], "Reference");
        assert_eq!(value["boards"][0]["starred"].as_bool(), Some(true));
        assert_eq!(value["spaces"][0]["pinned"].as_bool(), Some(true));
    }

    /// A title is arbitrary user text and must survive as itself.
    #[test]
    fn a_title_with_a_quote_and_a_newline_stays_parseable() {
        let awkward = "He said \"go\"\nnow\\then\ttabbed";
        let boards = [board_at("alpha", awkward, 1)];
        // The folder name is user typing too, and it goes through the same escaper.
        let filing = filing_from(&format!(
            r#"{{"spaces":[{{"name":"Quote \" and \\ backslash",
                             "boards":["{}"],"pinned":false}}]}}"#,
            filed("alpha"),
        ));

        let value = parsed(&render(&boards, &filing));
        assert_eq!(value["boards"][0]["title"], awkward, "the title did not round-trip");
        assert_eq!(value["boards"][0]["space"], "Quote \" and \\ backslash");
    }

    /// A hand-edited sidecar can file one board twice; the answer must still be one answer.
    #[test]
    fn a_board_in_two_folders_is_reported_and_counted_once() {
        let boards = [board_at("alpha", "Alpha", 1)];
        let filing = filing_from(&format!(
            r#"{{"spaces":[{{"name":"First","boards":["{}"],"pinned":false}},
                           {{"name":"Second","boards":["{}"],"pinned":false}}]}}"#,
            filed("alpha"),
            filed("alpha"),
        ));

        let value = parsed(&render(&boards, &filing));
        assert_eq!(value["boards"][0]["space"], "First", "the first record must win");
        let counted: u64 = value["spaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|space| space["boards"].as_u64().unwrap())
            .sum();
        assert_eq!(counted, 1, "the board was counted in both folders");
    }

    /// Recently deleted hides a board from its folder's count and not from the list.
    #[test]
    fn a_deleted_board_is_not_counted_in_its_folder_but_is_still_listed() {
        let boards = [board_at("alpha", "Alpha", 2), board_at("beta", "Beta", 1)];
        let filing = filing_from(&format!(
            r#"{{"spaces":[{{"name":"Reference","boards":["{}","{}"],"pinned":false}}],
                 "trashed":[{{"path":"{}","at":1785544454}}]}}"#,
            filed("alpha"),
            filed("beta"),
            filed("beta"),
        ));

        let value = parsed(&render(&boards, &filing));
        // Still on the list, still filed, and carrying the time it was deleted — the client
        // needs all three to draw it in Recently deleted and to put it back.
        assert_eq!(value["boards"][1]["space"], "Reference");
        assert_eq!(value["boards"][1]["trashed"].as_u64(), Some(1_785_544_454));
        // But the folder says one, because one is what the client can show in it.
        assert_eq!(value["spaces"][0]["boards"].as_u64(), Some(1));
    }

    /// The client bands on the order, so the order is part of the answer.
    #[test]
    fn the_board_order_is_the_order_list_boards_gave() {
        let boards = [
            board_at("newest", "Newest", 9_000),
            board_at("middle", "Middle", 5_000),
            board_at("oldest", "Oldest", 1_000),
        ];
        let value = parsed(&render(&boards, &Filing::default()));
        let ids: Vec<&str> =
            value["boards"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["newest", "middle", "oldest"]);
        // And the clock is milliseconds, which is the half a client can silently get wrong.
        assert_eq!(value["boards"][0]["modified"].as_u64(), Some(9_000));
    }
}
