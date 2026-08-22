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
//! **There is no exemption for test code**, which is why `manifest.rs`'s scratch helper builds
//! a fresh directory per run rather than clearing one. An exemption is a hole, and a hole in
//! this particular rule is how an irreplaceable board goes missing.
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
/// These are not forbidden outright: writing into the server's own data directory is the job,
/// and `create_dir_all` and `write` are how a manifest gets written. What is required is that
/// each one is **acknowledged** — a `RULE ZERO:` note on the line or the line above, saying
/// why this particular write cannot destroy a board. The point is not the comment, it is that
/// adding one of these is a decision somebody has to make in writing.
const TRUNCATING: [&str; 4] =
    ["std::fs::copy", "fs::copy(", "File::create", ".truncate(true)"];

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
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(!files.is_empty(), "found no source to scan — the test is not testing anything");

    let mut unexplained = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
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
