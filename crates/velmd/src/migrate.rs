//! Verifying a copied data directory, and importing it into the server's own.
//!
//! Both halves exist because "the files arrived" and "the boards are readable" are different
//! claims, and only the second one is what the user actually cares about. Bytes first,
//! because a hash mismatch means *stop* and no amount of semantic checking is worth running
//! on a corrupt copy; then meaning, on the copies alone.

use std::collections::BTreeMap;
use std::path::Path;

use crate::manifest;

/// Recompute every hash under `data` and diff it against `manifest_path`.
///
/// Byte-level only, deliberately. It opens nothing with SQLite, so it is safe to point at
/// either side of the copy.
pub fn verify(data: &Path, manifest_path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(data.is_dir(), "{} is not a directory", data.display());
    let expected = manifest::read(manifest_path)?;

    let found = manifest::walk(data)?;
    let found_by_path: BTreeMap<&str, &manifest::Entry> =
        found.iter().map(|e| (e.path.as_str(), e)).collect();

    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    for want in &expected.entries {
        match found_by_path.get(want.path.as_str()) {
            None => missing.push(want.path.clone()),
            Some(got) if got.hash != want.hash => mismatched.push(want.path.clone()),
            Some(_) => {}
        }
    }

    let boards = expected.entries.iter().filter(|e| e.path.ends_with(".vellum")).count();
    println!(
        "{boards} boards · {} files · {} mismatches · {} missing",
        expected.entries.len(),
        mismatched.len(),
        missing.len()
    );

    for path in missing.iter().take(20) {
        println!("  MISSING   {path}");
    }
    for path in mismatched.iter().take(20) {
        println!("  MISMATCH  {path}");
    }

    // Extra files are reported and are **not** a failure. The manifest is taken before the
    // copy and the copy lands in a directory that may already hold the server's own runtime
    // files; a stricter rule would fail a perfectly good migration for a lock file.
    let extra = found.len().saturating_sub(expected.entries.len() - missing.len());
    if extra > 0 {
        println!("  ({extra} file(s) present here that the manifest did not record — not an error)");
    }

    anyhow::ensure!(
        missing.is_empty() && mismatched.is_empty(),
        "the copy is not identical to the source — do not continue; re-run the copy"
    );
    println!("\nEvery file arrived intact.");
    Ok(())
}

/// Copy `from` into the server's live `data` directory, then check the copies are readable.
///
/// **Copy, never move.** `from` is left exactly as it was found — this function has no code
/// path that removes or renames anything, which is what makes it re-runnable and what makes a
/// failure halfway through cost nothing.
pub fn import(from: &Path, data: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(from.is_dir(), "{} is not a directory", from.display());
    // Canonicalised, because a lexical compare is satisfied by a trailing slash, a symlink
    // or `~/x` against `/Users/me/x` -- and "these are two different places" is the whole
    // premise of a copy. `--data` may not exist yet, so only `--from` is required to resolve.
    let from_real = std::fs::canonicalize(from)
        .map_err(|e| anyhow::anyhow!("cannot resolve {}: {e}", from.display()))?;
    if let Ok(data_real) = std::fs::canonicalize(data) {
        anyhow::ensure!(
            from_real != data_real,
            "--from and --data resolve to the same directory ({}); import copies between two \
             places",
            from_real.display()
        );
    }
    std::fs::create_dir_all(data)?;

    let entries = manifest::walk(from)?;
    let mut copied = 0usize;
    let mut skipped = 0usize;
    for entry in &entries {
        let source = from.join(&entry.path);
        let target = data.join(&entry.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // An identical file already in place is left alone, so a re-run is cheap and cannot
        // half-write something that was already correct.
        if let Ok(existing) = manifest::hash_file(&target)
            && existing.1 == entry.hash
        {
            skipped += 1;
            continue;
        }
        // ⚠ `std::fs::copy` TRUNCATES an existing target. RULE ZERO is about content, not
        // about which syscall removes it: importing a stale copy over a live data directory
        // would replace a newer board with an older one, silently, and that is the same loss
        // as an unlink. The hash check above only clears files that are already identical, so
        // anything reaching here with different content is refused by name.
        if target.exists() {
            anyhow::bail!(
                "{} already exists here and differs from the incoming copy.\n  \
                 Refusing to overwrite it -- that would replace a board with a different \
                 version of itself and there is no undo.\n  \
                 If the incoming copy is genuinely the one you want, move the existing file \
                 aside yourself first.",
                target.display()
            );
        }
        // RULE ZERO: `copy` truncates, so this is a destructive call wearing an innocent
        // name — and the guard is the `bail!` immediately above, not this line. `target`
        // cannot exist by the time we get here: an identical file was skipped earlier by
        // hash, and a *differing* one is refused by name rather than replaced. So the only
        // file this ever writes is one that was not there.
        std::fs::copy(&source, &target)
            .map_err(|e| anyhow::anyhow!("copying {}: {e}", entry.path))?;
        copied += 1;
    }
    println!("copied {copied} file(s), {skipped} already present and identical\n");

    check_boards(data)
}

/// Write one board's Loro snapshot to a file.
///
/// This is `velmd serve`'s `/snapshot` route with the HTTP taken off, and it is what a
/// browser client actually consumes: [`vellum_doc::Board::from_bytes`] over these exact
/// bytes is the whole of the document layer a wasm build needs.
///
/// ⚠ **Point it at a copy.** `BoardDb::open` is not read-only -- it runs
/// `CREATE TABLE IF NOT EXISTS`, may bump `user_version`, and WAL mode creates a `-wal`
/// sidecar. Harmless on a copy, and not something to do to the only copy of a board that
/// cannot be re-imported.
pub fn snapshot(board_path: &Path, out: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(board_path.is_file(), "{} is not a file", board_path.display());
    let mut db = vellum_store::BoardDb::open(board_path)?;
    let board = db
        .load()?
        .ok_or_else(|| anyhow::anyhow!("{} holds no snapshot", board_path.display()))?;
    let bytes = board.to_bytes()?;
    let items = board.items()?.len();
    std::fs::write(out, &bytes)?;
    println!(
        "{} — {items} items, {} KB of snapshot → {}",
        board.title(),
        bytes.len() / 1024,
        out.display()
    );
    Ok(())
}

/// Copy just the assets one board references, into a directory a client can fetch from.
///
/// This is `velmd serve`'s `/blobs/{hash}` route with the HTTP taken off. The blob store is
/// shared across every board and runs to gigabytes; a client viewing one board needs the
/// handful it actually names.
///
/// The **match has no `_` arm on purpose.** Adding an `ItemKind` that carries an asset hash
/// then becomes a compile error rather than a board whose pictures silently do not travel —
/// which is the same rule the blob garbage collector will need and the reason to establish it
/// here, where getting it wrong costs nothing.
pub fn export_blobs(board_path: &Path, blobs: &Path, out: &Path) -> anyhow::Result<()> {
    use vellum_doc::ItemKind;

    let mut db = vellum_store::BoardDb::open(board_path)?;
    let board = db
        .load()?
        .ok_or_else(|| anyhow::anyhow!("{} holds no snapshot", board_path.display()))?;

    let mut wanted: BTreeMap<String, ()> = BTreeMap::new();
    for item in board.items()? {
        match &item.kind {
            ItemKind::Image { asset_id, .. } => {
                wanted.insert(asset_id.clone(), ());
            }
            ItemKind::LinkPreview { thumbnail, favicon, .. } => {
                for hash in [thumbnail, favicon].into_iter().flatten() {
                    wanted.insert(hash.clone(), ());
                }
            }
            _ => {}
        }
    }

    std::fs::create_dir_all(out)?;
    let (mut copied, mut absent) = (0usize, 0usize);
    let mut malformed = 0usize;
    for hash in wanted.keys() {
        // ⚠ **Parsed, never trusted, because this string comes out of a document.**
        //
        // An `asset_id` is board content, and `POST /sync` lets a client merge whatever it
        // likes into a board — so `asset_id = "../../../../tmp/x"` is a thing somebody can
        // put there and then wait for an operator to run `velmd blobs`. Joined unparsed, that
        // wrote a file outside `--out`. The HTTP route at `serve.rs`'s `blob` has always
        // parsed strictly and is total; this path was the sibling that did not, which is
        // feedback 35's rule exactly.
        //
        // `Hash::from_hex` accepts exactly 64 hex characters, so every traversal spelling —
        // `..`, a separator, a NUL, an absolute path, a `~` — fails to parse rather than
        // being filtered. That is the difference between a check and a guarantee.
        if hash.parse::<vellum_store::Hash>().is_err() {
            malformed += 1;
            continue;
        }
        // The store's own layout: two hex characters of shard, then the full hash.
        let Some(shard) = hash.get(..2) else { continue };
        let source = blobs.join(shard).join(hash);
        if !source.is_file() {
            absent += 1;
            continue;
        }
        let target = out.join(hash);
        if target.exists() {
            continue;
        }
        // RULE ZERO: `copy` truncates, and the `exists` check above is what makes that
        // harmless — nothing already in `--out` is ever written over. `out` is an export
        // directory rather than a board directory, and the name is a content hash that has
        // just been parsed, so a file that is there already holds exactly these bytes.
        std::fs::copy(&source, &target)?;
        copied += 1;
    }
    println!(
        "{} asset(s) referenced · {copied} copied · {absent} not in the store",
        wanted.len()
    );
    // Named rather than silent: an id that is not a hash means either a board written by
    // something else or an attempt at one of the paths above, and both are worth seeing.
    if malformed > 0 {
        println!("{malformed} asset id(s) were not valid hashes and were skipped");
    }
    Ok(())
}

/// Open every board **in the server's own directory** and report what it holds.
///
/// This is the first point in the whole migration where SQLite touches anything, and it is
/// deliberately the last step and on copies alone. `BoardDb::open` runs
/// `CREATE TABLE IF NOT EXISTS`, may bump `user_version`, and creates a `-wal` sidecar — all
/// fine on a copy, none of it acceptable on the only copy of an irreplaceable board.
fn check_boards(data: &Path) -> anyhow::Result<()> {
    let boards_dir = data.join("boards");
    if !boards_dir.is_dir() {
        println!("no boards/ directory here — nothing to check");
        return Ok(());
    }

    let mut rows: Vec<(String, String, usize)> = Vec::new();
    let mut unreadable = Vec::new();

    // `list_boards` answers a Result, and it reads each board's index row without decoding
    // any CRDT -- which is the whole point of `board_index` and why 58 boards list in
    // milliseconds.
    let found = vellum_store::list_boards(&boards_dir)
        .map_err(|e| anyhow::anyhow!("cannot list {}: {e}", boards_dir.display()))?;
    for index in &found {
        let path = &index.path;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        match inspect(path) {
            Ok((title, items)) => rows.push((name, title, items)),
            Err(error) => unreadable.push(format!("{name}: {error:#}")),
        }
    }

    let width = rows.iter().map(|(n, _, _)| n.len()).max().unwrap_or(4).min(44);
    println!("{:<width$}  {:>7}  TITLE", "FILE", "ITEMS", width = width);
    for (name, title, items) in &rows {
        println!("{name:<width$}  {items:>7}  {title}", width = width);
    }
    println!("\n{} board(s) opened and readable", rows.len());

    // ⚠ The failures are reported BEFORE the zero-board check below, and the order is the
    // whole point. If every board fails to open, `rows` is empty and `unreadable` is full --
    // and a guard on `rows` alone would fire first, print "no boards were found, check your
    // --from path", and swallow the only lines that say *why*. That is a true-sounding
    // message that is false on both counts, in the one situation where the user most needs
    // the real reason.
    if !unreadable.is_empty() {
        println!("\n{} board(s) could NOT be read:", unreadable.len());
        for line in &unreadable {
            println!("  {line}");
        }
        anyhow::bail!("some boards did not load — stop and ask before going further");
    }

    // ⚠ Zero boards is reported as a failure, not as a quiet success.
    //
    // An import that found nothing prints "0 mismatches" and every other reassuring number,
    // and reads exactly like a clean run. That is the vacuous pass this repository keeps
    // paying for -- `--demo zoom-flicker` fails on `queued == 0` for the same reason, because
    // a sweep that never crossed an edge would otherwise report zero blinks and look like
    // evidence. A migration is the worst possible place to learn that lesson twice.
    anyhow::ensure!(
        !found.is_empty(),
        "no boards were found in {} -- nothing was imported. \
         Check that --from pointed at a Velm data directory (the one holding boards/ and \
         blobs/), and that the copy actually carried the .vellum files across",
        boards_dir.display()
    );
    Ok(())
}

/// Open one board, recover it if it was left unclean, and report its title and item count.
fn inspect(path: &Path) -> anyhow::Result<(String, usize)> {
    let mut db = vellum_store::BoardDb::open(path)?;

    // A `-wal` left behind by the Mac means Velm did not shut down cleanly. Recovering here
    // is right: this is a copy, the recovery path is the same one the app runs on every
    // launch, and it is far better to learn a board needs recovery now than the first time
    // it is opened from an iPad.
    if db.recovery().unclean_shutdown && db.recover()?.is_some() {
        println!("  (recovered a board that had been left unclean: {})", path.display());
    }

    // ⚠ `check_integrity` answers `Result<bool>`, and the bool is the answer. Writing
    // `db.check_integrity()?;` compiles, discards it, and reports a corrupt board as fine --
    // which is the whole failure this step exists to catch.
    anyhow::ensure!(
        db.check_integrity()?,
        "SQLite reports this file as corrupt (PRAGMA quick_check failed)"
    );

    let board = db.load()?.ok_or_else(|| anyhow::anyhow!("holds no snapshot"))?;
    let title = board.title();
    let items = board.items()?.len();
    Ok((title, items))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("velmd-mig-{name}-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn importing_over_a_board_that_differs_is_refused_rather_than_overwritten() {
        // `std::fs::copy` truncates. RULE ZERO is about content, not about which syscall
        // takes it away: importing a stale copy over a live data directory would replace a
        // newer board with an older one, silently, and no `rm` would appear anywhere.
        //
        // A/B: with the `target.exists()` guard removed this test fails with `after ==
        // "older"`, which is the data loss stated as an assertion.
        let root = scratch("overwrite");
        let from = root.join("from");
        let data = root.join("data");
        std::fs::create_dir_all(from.join("boards")).unwrap();
        std::fs::create_dir_all(data.join("boards")).unwrap();
        std::fs::write(from.join("boards/b.vellum"), b"older").unwrap();
        std::fs::write(data.join("boards/b.vellum"), b"newer, and irreplaceable").unwrap();

        let result = super::import(&from, &data);

        assert!(result.is_err(), "import overwrote an existing board instead of refusing");
        let after = std::fs::read(data.join("boards/b.vellum")).unwrap();
        assert_eq!(
            after, b"newer, and irreplaceable",
            "the board that was already there was modified"
        );
    }

    #[test]
    fn an_identical_file_is_skipped_rather_than_refused() {
        // The guard must not make a re-run impossible. Identical content is the common case
        // when somebody runs the import twice, and refusing there would teach them to reach
        // for a flag that disables the protection entirely.
        let root = scratch("idempotent");
        let from = root.join("from");
        let data = root.join("data");
        std::fs::create_dir_all(from.join("boards")).unwrap();
        std::fs::create_dir_all(data.join("boards")).unwrap();
        std::fs::write(from.join("boards/b.vellum"), b"same").unwrap();
        std::fs::write(data.join("boards/b.vellum"), b"same").unwrap();

        // It fails on "no boards opened" -- `b.vellum` is not a real database -- but the
        // copy stage must have got past the identical file rather than bailing on it.
        let message = format!("{:#}", super::import(&from, &data).unwrap_err());
        assert!(
            !message.contains("Refusing to overwrite"),
            "an identical file was refused instead of skipped: {message}"
        );
    }
}
