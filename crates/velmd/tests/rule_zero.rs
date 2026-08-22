//! The one property this crate promises, enforced against its own source.
//!
//! `velmd` holds a copy of ~58 boards that cannot be re-imported: the `.rtb` backups are
//! encrypted and Miro's REST API returns no content for 46% of items. CLAUDE.md's RULE ZERO
//! puts it plainly — those boards *"must stay alive no matter what gets updated"*.
//!
//! A comment saying "this program never deletes anything" is worth very little. It is true on
//! the day it is written and nothing notices when it stops being true — which is exactly the
//! `locked: false` trap this repository has already paid for once, where a constant standing
//! in for a field that did not exist yet went stale in a different crate and broke no test.
//!
//! So the promise is a test. It reads this crate's own source and fails on any call that
//! could remove or displace a file.
//!
//! **The removal scan has no exemption for test code**, which is why `manifest.rs`'s scratch
//! helper builds a fresh directory per run rather than clearing one. An exemption is a hole,
//! and a hole in this particular rule is how an irreplaceable board goes missing.
//!
//! The *truncating*-write scan covers `src/` alone, and the reason is written out at that
//! test. The short version: a test that removes is as dangerous as production that removes,
//! and a test that writes a fixture into a `TempDir` is simply how a test is written.
//!
//! What is deliberately *not* forbidden: `std::fs::copy`, `create_dir_all` and `write`.
//! Importing is a copy, and writing into the server's own data directory is the job.

use std::path::{Path, PathBuf};

/// Calls that remove or displace a file. `rename` is here because a move is a delete from
/// wherever the file used to be, and the migration's whole promise is that the source is
/// left untouched.
const FORBIDDEN: [&str; 6] = [
    "remove_file",
    "remove_dir_all",
    "remove_dir",
    "std::fs::rename",
    "fs::rename",
    // ⚠ **Bare, because a qualified needle is one `use` away from useless.**
    // `use std::fs::{self, rename};` then a bare `rename(a, b)` matched neither of the two
    // above. The other four here are bare names and survive that; this one did not.
    "rename(",
];

/// Calls that destroy a file's contents without unlinking it.
///
/// ⚠ **Separate from [`FORBIDDEN`] because RULE ZERO is about content, not about which
/// syscall takes it.** `import` once used `std::fs::copy`, which truncates — so a stale copy
/// written over a live board would have replaced a newer board with an older one, with no
/// `rm` anywhere for the scan to catch. That was found by hand and guarded by hand at its one
/// call site.
///
/// These are not forbidden outright: writing into the server's own data directory is the job.
/// What is required is that each one is **acknowledged** — a `RULE ZERO:` note on the line or the line above, saying
/// why this particular write cannot destroy a board. The point is not the comment, it is that
/// adding one of these is a decision somebody has to make in writing.
const TRUNCATING: [&str; 5] = [
    // ⚠ **Bare, for the reason `FORBIDDEN`'s `rename(` gives three lines above** — and it
    // was written qualified anyway, in the same hunk that added that comment. `use
    // std::fs::copy;` then `copy(&stale, &live_board)` matched neither `std::fs::copy` nor
    // `fs::copy(`. Feedback 35's rule, stated in a comment and not applied below it.
    "copy(",
    // The one the first version of this list excused **by name** in its own doc, while two
    // live sites took an operator-supplied path with no `exists` check. `write` opens
    // create+truncate: `velmd snapshot --out b.vellum` replaced a board with raw bytes.
    //
    // Qualified, unlike `copy(` above, and the asymmetry is measured rather than chosen: a
    // bare `write(` matches `pub fn write(`, `manifest::write(` and every `Write` impl in
    // the workspace — sixteen lines on the first run, none of them a hazard. **A guard that
    // fires on things that are fine is a guard people learn to silence.** `fs::write(`
    // catches both spellings that exist here; a `use std::fs::write;` would escape it, and
    // that is a real hole, named rather than papered over.
    "fs::write(",
    "File::create",
    ".truncate(true)",
    "OpenOptions",
];

/// The code on a line, with a trailing comment removed.
///
/// ⚠ **A mention in prose is not a call — but only strip a comment that starts one.** This
/// was `line.split("//").next()`, which cut at the *first* `//` anywhere on the line: so
/// `let u = "https://x"; std::fs::remove_file(p);` scanned as `let u = "https:` and passed.
/// No such line exists today, which is exactly the problem — the guard was one URL literal
/// away from silently stopping, with nothing to say it had.
///
/// A `//` inside a string literal is not the start of a comment either. Counting quotes
/// before it is crude and errs towards scanning **more**, which is the safe direction for a
/// guard: the worst case is a false positive somebody has to look at, not a real one nobody
/// ever sees.
///
/// One function, because the A/B below has to exercise *this* logic. It used to keep its own
/// copy of the split, so it went on passing while the real matcher was blind.
fn code_of(line: &str) -> &str {
    match line.find("//") {
        Some(at) if line[..at].matches('"').count().is_multiple_of(2) => &line[..at],
        _ => line,
    }
}

fn sources(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(listing) = std::fs::read_dir(dir) else { return };
    for item in listing.flatten() {
        let path = item.path();
        if path.is_dir() {
            sources(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push(path);
        }
    }
}

#[test]
fn velmd_contains_no_code_that_can_remove_a_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    sources(&root.join("tests"), &mut files);
    assert!(!files.is_empty(), "found no source to scan — the test is not testing anything");

    let mut offences = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for (number, line) in text.lines().enumerate() {
            // This file names the calls in order to forbid them.
            if file.file_name().is_some_and(|n| n == "rule_zero.rs") {
                continue;
            }
            // ⚠ **A mention in prose is not a call — but only strip a comment that starts
            // one.** `line.split("//").next()` cut at the *first* `//` anywhere on the line,
            // so `let u = "https://x"; std::fs::remove_file(p);` scanned as `let u = "https:`
            // and passed. No such line exists today, which is exactly the problem: the guard
            // was one URL literal away from silently stopping.
            let code = code_of(line);
            for needle in FORBIDDEN {
                if code.contains(needle) {
                    offences.push(format!(
                        "{}:{}: {}",
                        file.strip_prefix(root).unwrap_or(file).display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        offences.is_empty(),
        "velmd must never remove or move a file — RULE ZERO. Found:\n  {}\n\n\
         If a board genuinely has to be removed, that belongs in the desktop app's \
         `Library::purge`, which is reachable from Recently deleted alone and is confirmed.",
        offences.join("\n  ")
    );
}

#[test]
fn the_scan_would_actually_catch_something() {
    // A/B for the test itself. The rule above passes trivially if the matcher is broken, and
    // a green test that cannot fail is worse than no test — CLAUDE.md records four of those
    // found in one review. This proves the needle matches a line that really does remove.
    let matches = |line: &str| FORBIDDEN.iter().any(|n| code_of(line).contains(n));

    assert!(
        matches("    let _ = std::fs::remove_file(&path);"),
        "the forbidden-call matcher does not match a real removal, so the rule above is vacuous"
    );
    // And that prose alone does not trip it, or the rule becomes unwriteable in its own docs.
    assert!(!matches("    // this never calls remove_file anywhere"));

    // ⚠ The three ways this matcher was shown to be defeatable. Each one is a line that
    // really does remove a file and really did pass.
    assert!(
        matches(r#"    let u = "https://example.com"; std::fs::remove_file(p);"#),
        "a URL literal earlier on the line must not blind the scan"
    );
    assert!(
        matches("    rename(&old, &new).unwrap();"),
        "a bare `rename` after `use std::fs::rename` must be caught"
    );
    assert!(
        !matches(r#"    let doc = "see https://x for why";"#),
        "a line that only mentions a URL must not be an offence"
    );
}

/// Every `fn` in a `mod tests` carries `#[test]`.
///
/// ⚠ **This exists because it happened three times in one session, twice in this file's own
/// crate.** Inserting a new test above an existing one, anchored on its `fn` line rather than
/// on its attribute, moves the attribute onto the *new* test — so the old one silently stops
/// running while the suite reports **more** tests than before, which is the reading least
/// likely to prompt a second look.
///
/// Clippy catches it (`duplicated attribute`, then `never used`) and clippy is `-D warnings`
/// in CI, so nothing was ever shipped. What clippy does not do is fail the *test* run, which
/// is what somebody watches while iterating — and three times the gap between those two was
/// long enough to keep editing on top of a suite that had quietly shrunk.
///
/// Helper functions are allowed: only a `fn` at the top level of the module counts, and one
/// taking arguments is a helper by construction.
#[test]
fn every_test_in_this_crate_still_has_its_attribute() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    sources(&root.join("tests"), &mut files);

    let mut orphans = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let Some(start) = lines.iter().position(|l| l.trim_start().starts_with("#[cfg(test)]"))
        else {
            continue;
        };
        for (index, line) in lines.iter().enumerate().skip(start) {
            let trimmed = line.trim_start();
            // Four spaces of indent is the module's top level; a nested `fn` is a closure or
            // an impl and is not a test.
            if !trimmed.starts_with("fn ") || line.len() - trimmed.len() != 4 {
                continue;
            }
            // A helper takes arguments; a test cannot.
            if !trimmed.contains("()") {
                continue;
            }
            // The attribute sits directly above, or above a doc block.
            let has_attribute = lines[..index]
                .iter()
                .rev()
                .take_while(|l| {
                    let t = l.trim_start();
                    t.starts_with("///") || t.starts_with("#[") || t.is_empty()
                })
                .any(|l| l.trim_start().starts_with("#[test]"));
            if !has_attribute {
                orphans.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(root).unwrap_or(file).display(),
                    index + 1,
                    trimmed
                ));
            }
        }
    }
    assert!(
        orphans.is_empty(),
        "these look like tests and will never run — most likely an insert above took their \n\
         `#[test]`. Anchor on the attribute, not the `fn`:\n{}",
        orphans.join("\n")
    );
}

/// Every truncating write is acknowledged in writing.
///
/// ⚠ **RULE ZERO is about content, not about which syscall takes it.** `import` once used
/// `std::fs::copy` — which truncates — so a stale copy written over a live directory would
/// have replaced a newer board with an older one, and there is no `rm` anywhere in that for
/// the scan above to catch. It was found by hand and guarded by hand; nothing structural
/// stopped the next one.
///
/// This does not forbid them: writing into the server's own data directory is the job. It
/// requires that each one carries a `RULE ZERO:` note saying why *that* write cannot destroy
/// a board. The value is not the comment, it is that adding one of these has to be a decision
/// somebody makes in writing rather than a line that slips in.
#[test]
fn every_truncating_write_says_why_it_is_safe() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // ⚠ **`src/` only, unlike the removal scan above, and the asymmetry is deliberate.**
    //
    // A test that *removes* a directory is exactly as dangerous as production code that does
    // — it runs on this machine, and a scratch path is one typo from a real one. That is why
    // the removal scan has no exemption and why `manifest.rs`'s helper builds a fresh
    // directory per run rather than clearing one.
    //
    // A test that *writes* a fixture into a `TempDir` is how every test in this crate is
    // written. Requiring a `RULE ZERO:` note on each would put sixteen of them in this crate
    // alone, none of which is a hazard, and a guard that fires on things that are fine is a
    // guard people learn to silence. What this exists to catch is a truncating write on a
    // path **an operator typed**, and only production takes one.
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(!files.is_empty(), "found no source to scan — the test is not testing anything");

    let mut unexplained = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // ⚠ **A `#[cfg(test)]` module inside `src/` is test code**, and the paragraph on this
        // test says why those are out of scope for truncation. Scanning to the marker rather
        // than filtering by directory, because that is where this crate's unit tests live —
        // conventionally last in the file, which is what makes a scan-until sound. If one
        // ever appears in the middle, this stops early and under-scans: the failure direction
        // is a guard that misses, so the marker is asserted to be last where it is present.
        let end = lines.iter().position(|l| l.trim_start().starts_with("#[cfg(test)]"));
        if let Some(at) = end {
            assert!(
                lines[at..].iter().filter(|l| l.trim_start().starts_with("mod ")).count() <= 1,
                "{}: more than one module after the #[cfg(test)] marker — this scan assumes \
                 the test module is last and would stop early",
                file.display()
            );
        }
        for (index, line) in lines.iter().take(end.unwrap_or(lines.len())).enumerate() {
            if !TRUNCATING.iter().any(|needle| code_of(line).contains(needle)) {
                continue;
            }
            // The line itself, or any of the six above it. Six because a `RULE ZERO:` note
            // sits at the top of a paragraph that says *why* the write is safe, and a real
            // one of those runs four or five lines — the first version of this test used
            // three and failed against two notes that were perfectly good, which would have
            // taught the next person to write a shorter comment rather than a clearer one.
            let context = lines[index.saturating_sub(6)..=index].join("\n");
            if !context.contains("RULE ZERO") {
                unexplained.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(root).unwrap_or(file).display(),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        unexplained.is_empty(),
        "these writes can destroy a file's contents and do not say why that is safe.\n\
         Add a `RULE ZERO:` note above each one, or use a call that cannot truncate:\n{}",
        unexplained.join("\n")
    );
}
