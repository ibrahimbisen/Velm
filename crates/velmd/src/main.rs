//! `velmd` — the Velm board server.
//!
//! # RULE ZERO, in code rather than in prose
//!
//! The boards this program handles are a migration of real Miro boards that **cannot be
//! re-imported**: the `.rtb` backups are encrypted, and Miro's REST API returns no content
//! for 46% of items. A board lost here is very likely lost for good.
//!
//! So this binary has one absolute property, and it is worth stating before anything else:
//!
//! **`velmd` never removes a file.** There is no subcommand, no request, and no code path
//! that unlinks a `.vellum`, a blob, or anything else. `crates/velmd/tests/rule_zero.rs`
//! greps this crate's own source for `remove_file`/`remove_dir_all`/`rename` and fails the
//! build on a hit. That test is the enforcement; this comment is only the explanation.
//!
//! Deletion stays where it already is — `Library::purge` in the desktop app, reachable from
//! Recently deleted alone, behind its own confirmation.
//!
//! # The subcommands, and why they are separate
//!
//! Migration is three steps rather than one because each answers a different question, and
//! stopping between them is the point:
//!
//! - `manifest` — *what is on the Mac?* A pure file walk with BLAKE3. **It opens nothing
//!   with SQLite**, which is not fussiness: [`vellum_store::BoardDb::open`] is not read-only
//!   (it runs `CREATE TABLE IF NOT EXISTS`, may bump `user_version`, and WAL mode creates a
//!   `-wal` sidecar). A tool pointed at the user's live boards must be incapable of touching
//!   them, and the way to be incapable is to have no SQLite in the code path at all.
//! - `verify` — *did the copy arrive intact?* Recompute every hash, diff against the
//!   manifest. Answers in bytes, before anything semantic is attempted.
//! - `import` — *are the copies actually readable as boards?* Copies into the live data
//!   directory, then runs recovery/integrity/load **on the copies only**.

use std::path::PathBuf;
use std::process::ExitCode;

mod manifest;
mod migrate;

const USAGE: &str = "\
velmd — the Velm board server

USAGE:
    velmd manifest --data <DIR> --out <FILE>
    velmd verify   --data <DIR> --manifest <FILE>
    velmd import   --from <DIR> --data <DIR>
    velmd --version

Nothing in this program removes a file. Migration copies; it never moves.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("velmd: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(());
    };
    match command {
        "--version" | "-V" => {
            println!("velmd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--help" | "-h" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "manifest" => {
            let data = flag(args, "--data")?;
            let out = flag(args, "--out")?;
            manifest::write(&data, &out)
        }
        "verify" => {
            let data = flag(args, "--data")?;
            let file = flag(args, "--manifest")?;
            migrate::verify(&data, &file)
        }
        "import" => {
            let from = flag(args, "--from")?;
            let data = flag(args, "--data")?;
            migrate::import(&from, &data)
        }
        other => {
            print!("{USAGE}");
            anyhow::bail!("unknown command {other:?}")
        }
    }
}

/// Read `--name <value>`.
///
/// Hand-rolled rather than `clap`, following `vellum-app`'s `options.rs` and `build.py`:
/// this crate has six flags across three subcommands, and a dependency that parses them
/// would be larger than the parser.
fn flag(args: &[String], name: &str) -> anyhow::Result<PathBuf> {
    let at = args
        .iter()
        .position(|a| a == name)
        .ok_or_else(|| anyhow::anyhow!("missing {name} <PATH>"))?;
    let value = args
        .get(at + 1)
        .ok_or_else(|| anyhow::anyhow!("{name} needs a path after it"))?;
    anyhow::ensure!(!value.starts_with("--"), "{name} needs a path after it, not {value:?}");
    Ok(PathBuf::from(value))
}
