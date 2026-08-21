//! Fingerprinting a Velm data directory, without opening a single board.
//!
//! This runs on the user's Mac, against boards that cannot be replaced. Everything about it
//! is shaped by one rule: **it must be incapable of modifying what it is measuring.**
//!
//! That is why there is no `vellum_store` import in this file. [`vellum_store::BoardDb::open`]
//! is *not* read-only — it runs `CREATE TABLE IF NOT EXISTS`, may bump SQLite's `user_version`,
//! and opening in WAL mode creates a `-wal` sidecar. All three are harmless in the app and all
//! three are writes. A tool whose whole promise is "this only reads" should not be one refactor
//! away from breaking that promise, so the promise is structural: with no SQLite in the code
//! path, there is nothing here that *could* write.
//!
//! The unit is the **file**, not the board. Copying a Velm data directory means copying
//! `boards/*.vellum` plus any `-wal`/`-shm` sidecars, `boards/library.json` (which lives
//! inside `boards/`, not beside it), the whole content-addressed `blobs/` tree, `archives/`,
//! and the agent sidecars. Hashing every file and comparing the set is the only check that
//! notices a file nobody thought to look for.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One file, as it was found on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the data directory, with `/` separators on every platform.
    pub path: String,
    pub bytes: u64,
    /// BLAKE3, lower-case hex. The same function the blob store keys on.
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub entries: Vec<Entry>,
}

pub const MANIFEST_VERSION: u32 = 1;

/// Hash every file under `data` and write the manifest to `out`.
pub fn write(data: &Path, out: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(data.is_dir(), "{} is not a directory", data.display());

    let entries = walk(data)?;
    let boards = entries.iter().filter(|e| e.path.ends_with(".vellum")).count();
    let bytes: u64 = entries.iter().map(|e| e.bytes).sum();

    let manifest = Manifest { version: MANIFEST_VERSION, entries };
    let json = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(out, json)?;

    println!(
        "{boards} boards, {} files, {:.1} GB, fingerprinted",
        manifest.entries.len(),
        bytes as f64 / 1_000_000_000.0
    );
    println!("manifest written to {}", out.display());
    Ok(())
}

/// Every file under `root`, sorted by path so two runs are comparable line for line.
pub fn walk(root: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // A directory that cannot be read is reported, not skipped silently: a migration
        // that quietly omitted a folder would verify clean and be missing boards.
        let listing = std::fs::read_dir(&dir)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?;
        for item in listing {
            let item = item?;
            let path = item.path();
            let kind = item.file_type()?;
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let relative = relative_to(root, &path)?;
                // `.staging` holds half-written blobs that `BlobStore::publish` has not yet
                // renamed into place. They are not content and their presence is a race, not
                // a fact, so counting them would make two manifests of one directory differ.
                if relative.starts_with(".staging/") || relative.contains("/.staging/") {
                    continue;
                }
                let (bytes, hash) = hash_file(&path)?;
                found.push(Entry { path: relative, bytes, hash });
            }
            // Symlinks are deliberately neither followed nor recorded: following one can
            // leave the tree entirely, and Velm never creates one.
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

fn relative_to(root: &Path, path: &Path) -> anyhow::Result<String> {
    let rest = path
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("{} is not under {}", path.display(), root.display()))?;
    let mut parts = Vec::new();
    for part in rest.components() {
        parts.push(part.as_os_str().to_string_lossy().into_owned());
    }
    Ok(parts.join("/"))
}

/// BLAKE3 of a file, streamed.
///
/// Streamed rather than read whole because this walks a blob store: the reference board's
/// assets run to tens of megabytes each and a `.rtb` archive is over a hundred, so reading
/// each into memory would make peak usage the size of the largest file for no benefit.
/// `STREAM_CHUNK` matches `vellum_store::blob`'s own 256KB.
pub fn hash_file(path: &Path) -> anyhow::Result<(u64, String)> {
    const STREAM_CHUNK: usize = 256 * 1024;
    let mut file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; STREAM_CHUNK];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((total, hasher.finalize().to_hex().to_string()))
}

/// Read a manifest written by [`write`].
pub fn read(path: &Path) -> anyhow::Result<Manifest> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        manifest.version <= MANIFEST_VERSION,
        "manifest was written by a newer velmd (version {})",
        manifest.version
    );
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        // Named from an atomic counter, not the clock. CLAUDE.md records a fixture that
        // derived its scratch directory from `unix_now()` in *seconds*, so two tests starting
        // in the same second shared a directory and the first to finish deleted the other's
        // data -- presenting as two sidecar failures that looked like a bug in the sidecar.
        // `cargo test` is a parallel runner; a clock-derived path is a collision waiting.
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // Nothing is removed, not even here. The counter makes each directory unique, so
        // there is no stale state to clear -- which lets `tests/rule_zero.rs` grep this whole
        // crate for `remove_` and expect *zero* hits, test code included. A rule with an
        // exemption for tests is a rule with a hole in it.
        let dir = std::env::temp_dir().join(format!("velmd-{name}-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_manifest_records_every_file_with_its_hash() {
        let dir = scratch("walk");
        std::fs::create_dir_all(dir.join("boards")).unwrap();
        std::fs::create_dir_all(dir.join("blobs/ab")).unwrap();
        std::fs::write(dir.join("boards/one.vellum"), b"board one").unwrap();
        std::fs::write(dir.join("boards/library.json"), b"{}").unwrap();
        std::fs::write(dir.join("blobs/ab/abcd"), b"an image").unwrap();

        let entries = walk(&dir).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["blobs/ab/abcd", "boards/library.json", "boards/one.vellum"]);

        let board = &entries[2];
        assert_eq!(board.bytes, 9);
        assert_eq!(board.hash, blake3::hash(b"board one").to_hex().to_string());
    }

    #[test]
    fn staging_is_skipped_because_it_is_a_race_and_not_content() {
        // `BlobStore::publish` stages into `.staging` and renames. A file caught mid-flight
        // there would make two manifests of one unchanged directory disagree.
        let dir = scratch("staging");
        std::fs::create_dir_all(dir.join("blobs/.staging")).unwrap();
        std::fs::write(dir.join("blobs/.staging/half"), b"partial").unwrap();
        std::fs::write(dir.join("blobs/real"), b"whole").unwrap();

        let entries = walk(&dir).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["blobs/real"], "a staged blob must not reach the manifest");
    }

    #[test]
    fn a_manifest_round_trips_through_disk() {
        let dir = scratch("roundtrip");
        std::fs::write(dir.join("a.vellum"), b"x").unwrap();
        let out = dir.join("m.json");
        write(&dir, &out).unwrap();
        let back = read(&out).unwrap();
        // The manifest file itself lands in the directory it describes, so re-walking finds
        // one more entry than was recorded. Comparing the recorded set is the invariant.
        assert!(back.entries.iter().any(|e| e.path == "a.vellum"));
        assert_eq!(back.version, MANIFEST_VERSION);
    }
}
