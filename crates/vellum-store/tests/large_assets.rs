//! The streaming blob API against the asset that motivated it.
//!
//! `Reference Board.rtb` is 109MB and carries 205 images, the largest a 5.6MB PNG.
//! Storing it through [`BlobStore::put`] would make all 109MB resident to move it a
//! few hundred bytes; the point of [`BlobStore::put_file`] and
//! [`BlobStore::get_reader`] is that neither ever holds more than one buffer.
//!
//! The archive is not in the repository, so these tests **skip cleanly** when it is
//! absent — and say so, rather than passing quietly and looking like coverage.

use std::io::Read;
use std::path::PathBuf;
use vellum_store::{BlobStore, Hash};

/// The reference archive, looked up from the crate rather than the working
/// directory so it is found however the tests are invoked.
fn reference_archive() -> Option<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?.to_path_buf();
    let path = workspace.join("Reference Board.rtb");
    path.is_file().then_some(path)
}

fn skip(what: &str) {
    println!("skipping {what}: the reference .rtb is not present");
}

/// The memory claim, made concrete: a 109MB file goes in and comes back out, and
/// nothing on the way through is bigger than a buffer.
#[test]
fn a_hundred_megabyte_archive_streams_in_and_out_intact() {
    let Some(archive) = reference_archive() else {
        return skip("large asset round trip");
    };
    let home = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
    let size = std::fs::metadata(&archive).unwrap().len();

    let hash = blobs.put_file(&archive).unwrap();

    // The name has to be the hash of the file, or the store would be addressing
    // something other than the content it holds.
    assert_eq!(hash, Hash::of_reader(std::fs::File::open(&archive).unwrap()).unwrap());
    assert_eq!(blobs.size_of(&hash).unwrap(), Some(size));
    assert!(blobs.verify(&hash).unwrap());

    // Reading it back verifies as it goes, without ever materialising the archive.
    let mut reader = blobs.get_reader(&hash).unwrap().unwrap();
    assert_eq!(reader.len(), size);
    let mut buffer = vec![0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        let n = reader.read(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        read += n as u64;
    }
    assert_eq!(read, size, "the stream ended early");

    println!("streamed {size} bytes ({:.1}MB) through a 256KB buffer", size as f64 / 1e6);
}

/// Re-importing a board must not rewrite its assets. With 205 of them behind a
/// 109MB archive, "we already have this" has to be answered without a copy.
#[test]
fn re_storing_the_same_archive_copies_nothing() {
    let Some(archive) = reference_archive() else {
        return skip("large asset deduplication");
    };
    let home = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(home.path().join("blobs")).unwrap();

    let first = blobs.put_file(&archive).unwrap();
    let written = std::fs::metadata(blobs.path_for(&first)).unwrap().modified().unwrap();

    let second = blobs.put_file(&archive).unwrap();

    assert_eq!(first, second);
    assert_eq!(
        std::fs::metadata(blobs.path_for(&second)).unwrap().modified().unwrap(),
        written,
        "the archive was written a second time"
    );
}
