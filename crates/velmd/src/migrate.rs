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
    anyhow::ensure!(
        from != data,
        "--from and --data are the same directory; import copies between two places"
    );
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
        std::fs::copy(&source, &target)
            .map_err(|e| anyhow::anyhow!("copying {}: {e}", entry.path))?;
        copied += 1;
    }
    println!("copied {copied} file(s), {skipped} already present and identical\n");

    check_boards(data)
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

    // ⚠ Zero boards is reported as a failure, not as a quiet success.
    //
    // An import that found nothing prints "0 mismatches" and every other reassuring number,
    // and reads exactly like a clean run. That is the vacuous pass this repository keeps
    // paying for -- `--demo zoom-flicker` fails on `queued == 0` for the same reason, because
    // a sweep that never crossed an edge would otherwise report zero blinks and look like
    // evidence. A migration is the worst possible place to learn that lesson twice.
    anyhow::ensure!(
        !rows.is_empty(),
        "no boards were found in {} -- nothing was imported. \
         Check that --from pointed at a Velm data directory (the one holding boards/ and \
         blobs/), and that the copy in step 8 actually carried the .vellum files across",
        boards_dir.display()
    );

    if !unreadable.is_empty() {
        println!("\n{} board(s) could NOT be read:", unreadable.len());
        for line in &unreadable {
            println!("  {line}");
        }
        anyhow::bail!("some boards did not load — stop and ask before going further");
    }
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
