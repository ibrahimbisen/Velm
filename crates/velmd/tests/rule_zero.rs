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
const FORBIDDEN: [&str; 5] =
    ["remove_file", "remove_dir_all", "remove_dir", "std::fs::rename", "fs::rename"];

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
            // A mention in prose is not a call. Only code counts.
            let code = line.split("//").next().unwrap_or("");
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
    let sample = "    let _ = std::fs::remove_file(&path);";
    let code = sample.split("//").next().unwrap_or("");
    assert!(
        FORBIDDEN.iter().any(|needle| code.contains(needle)),
        "the forbidden-call matcher does not match a real removal, so the rule above is vacuous"
    );

    // And that prose alone does not trip it, or the rule becomes unwriteable in its own docs.
    let prose = "    // this never calls remove_file anywhere";
    let code = prose.split("//").next().unwrap_or("");
    assert!(!FORBIDDEN.iter().any(|needle| code.contains(needle)));
}
