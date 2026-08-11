//! A shared, content-addressed store for images, PDFs and other binary assets.
//!
//! # Why it is shared, and outside the board files
//!
//! The reference Miro board alone carries 205 images, and the target is 20–100
//! boards. Company logos, screenshots and diagrams get pasted onto board after
//! board, so storing assets inside each board's database would multiply the same
//! bytes by the number of boards that reference them. Keyed by content, an image
//! used on twenty boards is one file, and a board file stays small enough to copy,
//! sync or back up on its own.
//!
//! Content addressing buys three things beyond deduplication: writes are idempotent
//! (importing the same board twice costs no extra disk), a reference cannot go
//! stale (the key *is* the content), and corruption is detectable rather than
//! silent — see [`BlobStore::get`].
//!
//! BLAKE3 is the hash because it runs at multiple GB/s, which is what makes
//! verifying on read affordable, and because the import pipeline already uses it to
//! check assets recovered from `.rtb` archives.

use crate::error::{Result, StoreError};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A BLAKE3 content hash: the identity of a blob.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Hashes the content. This is the only way a blob gets a name.
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Hashes a stream without holding it in memory.
    ///
    /// The reference `.rtb` is 109MB and its largest embedded PNG is 5.6MB;
    /// naming such a thing must not require a `Vec` the size of the thing.
    pub fn of_reader(reader: impl Read) -> io::Result<Self> {
        let mut hasher = blake3::Hasher::new();
        drain(reader, |chunk| {
            hasher.update(chunk);
            Ok(())
        })?;
        Ok(Self(*hasher.finalize().as_bytes()))
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, which is also the on-disk filename and the form stored in
    /// board documents.
    pub fn to_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Shows the full hash: a truncated one in a log is not enough to find the file.
impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl FromStr for Hash {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        blake3::Hash::from_hex(s)
            .map(|h| Self(*h.as_bytes()))
            .map_err(|_| StoreError::BadHash(s.to_owned()))
    }
}

/// Directory holding in-progress writes. Named with a leading dot so it can never
/// be mistaken for a shard, which is always exactly two hex characters.
const STAGING_DIR: &str = ".staging";

/// Bytes moved per iteration when streaming.
///
/// Large enough that per-`read` overhead disappears against BLAKE3's multi-GB/s
/// throughput, small enough to stay resident in cache and to keep the peak
/// footprint of storing a 40MB image at a quarter of a megabyte.
const STREAM_CHUNK: usize = 256 * 1024;

/// A directory of blobs, addressed by content hash.
///
/// Cheap to clone-by-reopening and safe to share: every operation is a single
/// filesystem call sequence with no in-memory state to keep coherent, so two
/// processes pointed at the same root cannot corrupt each other.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Opens (creating if needed) a blob store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join(STAGING_DIR))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a blob lives, whether or not it is there.
    ///
    /// Exposed so large assets can be memory-mapped or streamed straight into a GPU
    /// upload instead of being copied through a `Vec`. That skips the verification
    /// [`BlobStore::get`] does, which is the point: the caller decides whether a
    /// second pass over 40MB of pixels is worth it.
    ///
    /// Blobs are sharded by the first byte of the hash, giving 256 buckets. At the
    /// scale this is built for — a few tens of thousands of assets across a
    /// hundred boards — that is tens of files per directory. A second level would
    /// create more directories than files.
    pub fn path_for(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(hex)
    }

    pub fn contains(&self, hash: &Hash) -> bool {
        self.path_for(hash).exists()
    }

    /// Stores `bytes` and returns their hash.
    ///
    /// Idempotent: storing content that is already present does no I/O at all, which
    /// is what makes re-importing a board cheap.
    ///
    /// The write is a staged file plus a rename, so a crash mid-write can leave a
    /// stray file in the staging directory but can never leave a *partial* blob
    /// under a valid hash — which would be indistinguishable from corruption on
    /// every later read.
    pub fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        if self.contains(&hash) {
            return Ok(hash);
        }

        let mut staged = self.stage()?;
        staged.write_all(bytes)?;
        self.publish(staged, hash)
    }

    /// Stores everything `reader` yields, hashing and writing as the bytes arrive.
    ///
    /// This is the entry point for anything that is not already a `Vec`: an image
    /// being extracted from a `.rtb` archive, a file being dropped onto the canvas,
    /// a decoded video frame sequence. Peak memory is [`STREAM_CHUNK`] regardless of
    /// the asset's size, which is the whole point — the reference `.rtb` is 109MB and
    /// [`BlobStore::put`] would make all of it resident to store a 5.6MB PNG from
    /// inside it.
    ///
    /// The cost of streaming is that deduplication can only be discovered at the end:
    /// the content's name is not known until the last byte has been read, so storing
    /// something already present still writes a staging file and then discards it.
    /// [`BlobStore::put_file`] avoids that, because a file can be read twice.
    pub fn put_reader(&self, reader: impl Read) -> Result<Hash> {
        let (staged, hash) = self.stage_stream(reader)?;
        if self.contains(&hash) {
            // Dropping the staging file deletes it; nothing was published, and the
            // bytes already on disk are the same bytes by construction.
            return Ok(hash);
        }
        self.publish(staged, hash)
    }

    /// Stores the contents of a file.
    ///
    /// Hashes first and copies only on a miss. That costs a second pass over the
    /// bytes the first time a file is stored and saves the entire write every time
    /// after, which is the trade that matters: re-importing the same board — the
    /// reference `.rtb` carries 205 assets — then touches no data at all, and the
    /// second pass reads from the page cache the first one just warmed.
    pub fn put_file(&self, path: impl AsRef<Path>) -> Result<Hash> {
        let path = path.as_ref();
        let hash = Hash::of_reader(fs::File::open(path)?)?;
        if self.contains(&hash) {
            return Ok(hash);
        }
        self.put_reader(fs::File::open(path)?)
    }

    /// Reads a blob, verifying that it still hashes to its key.
    ///
    /// `Ok(None)` means "not stored here"; a hash mismatch is an error rather than a
    /// miss, because a board referencing a blob that has rotted needs to say so, not
    /// quietly render as if the image had never been added.
    pub fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        let bytes = match fs::read(self.path_for(hash)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let actual = Hash::of(&bytes);
        if actual != *hash {
            return Err(StoreError::CorruptBlob { expected: *hash, actual });
        }
        Ok(Some(bytes))
    }

    /// Opens a blob as a stream that verifies itself as it is consumed.
    ///
    /// The counterpart to [`BlobStore::put_reader`], and the right way to hand a
    /// large asset to a decoder: an image decoder wants a `Read`, not 40MB of
    /// `Vec` it will immediately throw away.
    ///
    /// Verification necessarily completes only at the end of the stream, so a reader
    /// that stops early gets unverified bytes. See [`BlobReader`].
    pub fn get_reader(&self, hash: &Hash) -> Result<Option<BlobReader>> {
        let file = match fs::File::open(self.path_for(hash)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let len = file.metadata()?.len();
        let hasher = blake3::Hasher::new();
        Ok(Some(BlobReader { file, hasher, expected: *hash, len, ended: false }))
    }

    /// How big a stored blob is, without reading it.
    ///
    /// Lets a caller choose between [`BlobStore::get`] and [`BlobStore::get_reader`]
    /// — or decline to load an asset at all when the texture budget is spent.
    pub fn size_of(&self, hash: &Hash) -> Result<Option<u64>> {
        match fs::metadata(self.path_for(hash)) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Re-hashes a stored blob to prove it has not rotted, without holding it in
    /// memory. `Ok(false)` means the blob is not there at all.
    pub fn verify(&self, hash: &Hash) -> Result<bool> {
        let Some(mut reader) = self.get_reader(hash)? else {
            return Ok(false);
        };
        // `BlobReader` raises the mismatch at the end of the stream, so consuming it
        // is the check.
        match io::copy(&mut reader, &mut io::sink()) {
            Ok(_) => Ok(true),
            Err(error) => Err(corruption_from(error)),
        }
    }

    /// An empty file in the staging directory, on the same filesystem as the shards
    /// so that publishing it is a rename rather than a copy.
    fn stage(&self) -> Result<tempfile::NamedTempFile> {
        Ok(tempfile::NamedTempFile::new_in(self.root.join(STAGING_DIR))?)
    }

    fn stage_stream(&self, reader: impl Read) -> Result<(tempfile::NamedTempFile, Hash)> {
        let mut staged = self.stage()?;
        let mut hasher = blake3::Hasher::new();
        drain(reader, |chunk| {
            hasher.update(chunk);
            staged.write_all(chunk)
        })?;
        Ok((staged, Hash(*hasher.finalize().as_bytes())))
    }

    /// Moves a staged file into place under its content hash.
    ///
    /// The write is a staged file plus a rename, so a crash mid-write can leave a
    /// stray file in the staging directory but can never leave a *partial* blob
    /// under a valid hash — which would be indistinguishable from corruption on
    /// every later read.
    fn publish(&self, staged: tempfile::NamedTempFile, hash: Hash) -> Result<Hash> {
        let destination = self.path_for(&hash);
        if let Some(shard) = destination.parent() {
            fs::create_dir_all(shard)?;
        }

        // Durable before it becomes visible: an fsync after the rename would leave a
        // window where the name resolves to unwritten data.
        staged.as_file().sync_all()?;

        if let Err(error) = staged.persist(&destination) {
            // Another writer storing the same content wins the race harmlessly —
            // the file it put there has the same bytes by construction.
            if !destination.exists() {
                return Err(error.error.into());
            }
        }
        sync_directory(destination.parent());
        Ok(hash)
    }

    /// Deletes a blob, reporting whether it was there. Only safe once nothing
    /// references it — that bookkeeping belongs to a garbage collector that can see
    /// every board, not here.
    pub fn remove(&self, hash: &Hash) -> Result<bool> {
        match fs::remove_file(self.path_for(hash)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

/// A blob being read as a stream, checking itself against its key as it goes.
///
/// The hash of a stream is only known once the stream has ended, so the mismatch
/// surfaces on the read that returns zero bytes rather than the first one. A caller
/// that reads to the end — [`std::io::copy`], a decoder consuming a whole image —
/// therefore gets the same guarantee [`BlobStore::get`] gives. A caller that stops
/// early has, by definition, not seen enough of the blob to say anything about it.
///
/// Corruption arrives as an [`std::io::Error`] wrapping a
/// [`StoreError::CorruptBlob`], because `Read` has no other channel; use
/// [`corrupt_blob`] to recover the typed error.
#[derive(Debug)]
pub struct BlobReader {
    file: fs::File,
    hasher: blake3::Hasher,
    expected: Hash,
    len: u64,
    ended: bool,
}

impl BlobReader {
    /// Size of the blob in bytes, known before a single byte is read.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The key this stream is being checked against.
    pub fn hash(&self) -> Hash {
        self.expected
    }
}

impl Read for BlobReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read(buffer)?;
        if read > 0 {
            self.hasher.update(&buffer[..read]);
            return Ok(read);
        }

        // End of stream, and the first time we can say whether the bytes were the
        // ones this blob is named after. `ended` keeps a caller that keeps reading
        // past EOF from paying for `finalize` again.
        if !self.ended {
            self.ended = true;
            let actual = Hash(*self.hasher.finalize().as_bytes());
            if actual != self.expected {
                return Err(io::Error::other(StoreError::CorruptBlob {
                    expected: self.expected,
                    actual,
                }));
            }
        }
        Ok(0)
    }
}

/// Recovers the [`StoreError::CorruptBlob`] a [`BlobReader`] reported through
/// `std::io`, for callers that want to distinguish rot from a disk error.
pub fn corrupt_blob(error: &io::Error) -> Option<(Hash, Hash)> {
    match error.get_ref()?.downcast_ref::<StoreError>() {
        Some(StoreError::CorruptBlob { expected, actual }) => Some((*expected, *actual)),
        _ => None,
    }
}

fn corruption_from(error: io::Error) -> StoreError {
    match error.downcast::<StoreError>() {
        Ok(store) => store,
        Err(io) => StoreError::Io(io),
    }
}

/// Feeds `reader` to `sink` in [`STREAM_CHUNK`]-sized pieces until it ends.
///
/// `Read::read` is allowed to return short reads and `ErrorKind::Interrupted`, and
/// both are normal on a pipe; treating either as the end of the stream would store a
/// truncated asset under a hash that describes it, which is the one failure this
/// module exists to make impossible.
fn drain(mut reader: impl Read, mut sink: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    let mut buffer = vec![0u8; STREAM_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => sink(&buffer[..read])?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Makes a rename itself durable, not just the bytes it exposes.
///
/// Best-effort, and Unix-only: Windows has no equivalent of opening a directory to
/// fsync it, and `NamedTempFile::persist` there goes through `MoveFileEx`, which is
/// atomic with respect to readers regardless.
fn sync_directory(path: Option<&Path>) {
    #[cfg(unix)]
    if let Some(path) = path
        && let Ok(dir) = fs::File::open(path)
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        (dir, store)
    }

    #[test]
    fn hashes_round_trip_through_hex() {
        let hash = Hash::of(b"vellum");
        let hex = hash.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex.parse::<Hash>().unwrap(), hash);
        assert_eq!(hash.to_string(), hex);
    }

    #[test]
    fn malformed_hashes_are_rejected() {
        assert!(matches!("nonsense".parse::<Hash>(), Err(StoreError::BadHash(_))));
        assert!("ab".repeat(31).parse::<Hash>().is_err(), "31 bytes should not parse");
        assert!("zz".repeat(32).parse::<Hash>().is_err(), "non-hex should not parse");
    }

    #[test]
    fn a_blob_comes_back_exactly_as_it_went_in() {
        let (_dir, store) = store();
        let bytes: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();

        let hash = store.put(&bytes).unwrap();
        assert!(store.contains(&hash));
        assert_eq!(store.get(&hash).unwrap().unwrap(), bytes);
    }

    /// The reason the store is content-addressed: the same image on twenty boards
    /// is one file on disk.
    #[test]
    fn identical_bytes_are_stored_once() {
        let (_dir, store) = store();
        let bytes = b"the same screenshot pasted on twenty boards";

        let first = store.put(bytes).unwrap();
        let second = store.put(bytes).unwrap();

        assert_eq!(first, second);
        assert_eq!(count_blobs(&store), 1, "the same content was written twice");
    }

    #[test]
    fn different_bytes_get_different_files() {
        let (_dir, store) = store();
        let a = store.put(b"a").unwrap();
        let b = store.put(b"b").unwrap();

        assert_ne!(a, b);
        assert_eq!(count_blobs(&store), 2);
        assert_eq!(store.get(&a).unwrap().unwrap(), b"a");
        assert_eq!(store.get(&b).unwrap().unwrap(), b"b");
    }

    #[test]
    fn a_missing_blob_is_absent_not_an_error() {
        let (_dir, store) = store();
        let never_stored = Hash::of(b"never stored");

        assert!(!store.contains(&never_stored));
        assert_eq!(store.get(&never_stored).unwrap(), None);
        assert!(!store.remove(&never_stored).unwrap());
    }

    /// Content addressing is only worth anything if a mismatch is caught. This
    /// simulates bitrot by rewriting the file under its existing name.
    #[test]
    fn corrupted_bytes_are_detected_on_read() {
        let (_dir, store) = store();
        let hash = store.put(b"original contents").unwrap();

        fs::write(store.path_for(&hash), b"tampered contents").unwrap();

        match store.get(&hash) {
            Err(StoreError::CorruptBlob { expected, actual }) => {
                assert_eq!(expected, hash);
                assert_eq!(actual, Hash::of(b"tampered contents"));
            }
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn blobs_are_sharded_by_hash_prefix() {
        let (_dir, store) = store();
        let hash = store.put(b"anything").unwrap();
        let hex = hash.to_hex();

        let path = store.path_for(&hash);
        assert_eq!(path.file_name().unwrap(), hex.as_str());
        assert_eq!(path.parent().unwrap().file_name().unwrap(), &hex[..2]);
        assert!(path.exists());
    }

    /// Staging must not leave anything behind that a shard walk would trip over.
    #[test]
    fn writing_leaves_no_staged_files() {
        let (_dir, store) = store();
        store.put(b"one").unwrap();
        store.put(b"two").unwrap();

        let staged = fs::read_dir(store.root().join(STAGING_DIR)).unwrap().count();
        assert_eq!(staged, 0, "a staged file was left behind");
    }

    #[test]
    fn an_empty_blob_is_storable() {
        let (_dir, store) = store();
        let hash = store.put(b"").unwrap();
        assert_eq!(store.get(&hash).unwrap().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn removing_frees_the_file_and_the_hash_can_be_stored_again() {
        let (_dir, store) = store();
        let hash = store.put(b"transient").unwrap();

        assert!(store.remove(&hash).unwrap());
        assert!(!store.contains(&hash));

        assert_eq!(store.put(b"transient").unwrap(), hash);
        assert!(store.contains(&hash));
    }

    /// A second `BlobStore` over the same directory must see the first one's work —
    /// that is what makes the store shareable across board files and processes.
    #[test]
    fn a_reopened_store_sees_existing_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        let hash = BlobStore::open(&root).unwrap().put(b"persisted").unwrap();

        let reopened = BlobStore::open(&root).unwrap();
        assert!(reopened.contains(&hash));
        assert_eq!(reopened.get(&hash).unwrap().unwrap(), b"persisted");
    }

    // ----- streaming ------------------------------------------------------

    /// A blob stored as a stream must be indistinguishable from the same blob
    /// stored as a slice — same hash, same file, same deduplication.
    #[test]
    fn streaming_and_slice_writes_agree() {
        let (_dir, store) = store();
        let bytes: Vec<u8> = (0..=255u8).cycle().take(700_000).collect();

        let streamed = store.put_reader(bytes.as_slice()).unwrap();
        let sliced = store.put(&bytes).unwrap();

        assert_eq!(streamed, sliced);
        assert_eq!(streamed, Hash::of(&bytes));
        assert_eq!(count_blobs(&store), 1);
        assert_eq!(store.get(&streamed).unwrap().unwrap(), bytes);
    }

    /// The reason `put_reader` exists: content larger than one buffer has to be
    /// hashed and written across many, and an off-by-one there would corrupt every
    /// large asset silently.
    #[test]
    fn a_stream_spanning_many_chunks_round_trips() {
        let (_dir, store) = store();
        // Deliberately not a multiple of the chunk size, so the last read is short.
        let bytes: Vec<u8> = (0..STREAM_CHUNK * 3 + 12_345).map(|i| (i % 251) as u8).collect();

        let hash = store.put_reader(bytes.as_slice()).unwrap();

        assert_eq!(hash, Hash::of(&bytes));
        assert_eq!(store.get(&hash).unwrap().unwrap(), bytes);
        assert_eq!(store.size_of(&hash).unwrap(), Some(bytes.len() as u64));
    }

    /// A reader that hands over one byte at a time is what a decompressing or
    /// network-backed source looks like; short reads must accumulate, not truncate.
    #[test]
    fn a_reader_that_dribbles_bytes_still_stores_the_whole_asset() {
        struct Dribble<'a>(&'a [u8]);
        impl Read for Dribble<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() || buffer.is_empty() {
                    return Ok(0);
                }
                buffer[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }

        let (_dir, store) = store();
        let bytes = b"one byte at a time, like a decompressor".to_vec();

        let hash = store.put_reader(Dribble(&bytes)).unwrap();

        assert_eq!(hash, Hash::of(&bytes));
        assert_eq!(store.get(&hash).unwrap().unwrap(), bytes);
    }

    #[test]
    fn an_empty_stream_is_storable() {
        let (_dir, store) = store();
        let hash = store.put_reader(&[][..]).unwrap();

        assert_eq!(hash, Hash::of(b""));
        assert_eq!(store.get_reader(&hash).unwrap().unwrap().len(), 0);
        assert!(store.get_reader(&hash).unwrap().unwrap().is_empty());
    }

    #[test]
    fn a_failing_reader_stores_nothing_and_leaves_no_staged_file() {
        struct Fails;
        impl Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("the disk went away"))
            }
        }

        let (_dir, store) = store();
        assert!(store.put_reader(Fails).is_err());

        assert_eq!(count_blobs(&store), 0);
        assert_eq!(fs::read_dir(store.root().join(STAGING_DIR)).unwrap().count(), 0);
    }

    #[test]
    fn storing_a_file_matches_storing_its_bytes() {
        let (dir, store) = store();
        let bytes: Vec<u8> = (0..300_000).map(|i| (i % 97) as u8).collect();
        let source = dir.path().join("screenshot.png");
        fs::write(&source, &bytes).unwrap();

        let hash = store.put_file(&source).unwrap();

        assert_eq!(hash, Hash::of(&bytes));
        assert_eq!(store.get(&hash).unwrap().unwrap(), bytes);
    }

    /// The point of hashing a file before copying it: importing the same `.rtb`
    /// twice must not rewrite 109MB.
    #[test]
    fn storing_a_file_already_present_writes_nothing() {
        let (dir, store) = store();
        let source = dir.path().join("logo.png");
        fs::write(&source, b"already stored").unwrap();
        let first = store.put_file(&source).unwrap();

        let before = fs::metadata(store.path_for(&first)).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let second = store.put_file(&source).unwrap();
        let after = fs::metadata(store.path_for(&second)).unwrap().modified().unwrap();

        assert_eq!(first, second);
        assert_eq!(before, after, "the blob was rewritten");
        assert_eq!(count_blobs(&store), 1);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_silent_empty_blob() {
        let (dir, store) = store();
        assert!(store.put_file(dir.path().join("nothing-here")).is_err());
        assert_eq!(count_blobs(&store), 0);
    }

    #[test]
    fn a_streamed_read_returns_the_stored_bytes() {
        let (_dir, store) = store();
        let bytes: Vec<u8> = (0..STREAM_CHUNK * 2 + 7).map(|i| (i % 13) as u8).collect();
        let hash = store.put(&bytes).unwrap();

        let mut reader = store.get_reader(&hash).unwrap().unwrap();
        assert_eq!(reader.len(), bytes.len() as u64);
        assert_eq!(reader.hash(), hash);

        let mut read_back = Vec::new();
        reader.read_to_end(&mut read_back).unwrap();
        assert_eq!(read_back, bytes);
    }

    #[test]
    fn a_missing_blob_has_no_reader_and_no_size() {
        let (_dir, store) = store();
        let absent = Hash::of(b"never stored");

        assert!(store.get_reader(&absent).unwrap().is_none());
        assert_eq!(store.size_of(&absent).unwrap(), None);
        assert!(!store.verify(&absent).unwrap());
    }

    /// Streaming must not weaken the corruption guarantee that content addressing
    /// exists to provide — it only defers it to the end of the stream.
    #[test]
    fn a_streamed_read_reports_corruption_at_the_end_of_the_stream() {
        let (_dir, store) = store();
        let hash = store.put(b"original contents").unwrap();
        fs::write(store.path_for(&hash), b"tampered contentsX").unwrap();

        let mut reader = store.get_reader(&hash).unwrap().unwrap();
        let mut buffer = Vec::new();
        let error = reader.read_to_end(&mut buffer).expect_err("corruption went unreported");

        let (expected, actual) = corrupt_blob(&error).expect("not a corruption error");
        assert_eq!(expected, hash);
        assert_eq!(actual, Hash::of(b"tampered contentsX"));

        match store.verify(&hash) {
            Err(StoreError::CorruptBlob { expected, .. }) => assert_eq!(expected, hash),
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn verifying_an_intact_blob_succeeds_without_loading_it() {
        let (_dir, store) = store();
        let hash = store.put_reader(vec![7u8; STREAM_CHUNK + 1].as_slice()).unwrap();
        assert!(store.verify(&hash).unwrap());
    }

    #[test]
    fn hashing_a_stream_agrees_with_hashing_a_slice() {
        let bytes: Vec<u8> = (0..STREAM_CHUNK * 2).map(|i| (i % 7) as u8).collect();
        assert_eq!(Hash::of_reader(bytes.as_slice()).unwrap(), Hash::of(&bytes));
    }

    #[test]
    fn streaming_writes_leave_no_staged_files() {
        let (_dir, store) = store();
        store.put_reader(b"one".as_slice()).unwrap();
        store.put_reader(b"one".as_slice()).unwrap();
        store.put_reader(b"two".as_slice()).unwrap();

        let staged = fs::read_dir(store.root().join(STAGING_DIR)).unwrap().count();
        assert_eq!(staged, 0, "a staged file was left behind");
        assert_eq!(count_blobs(&store), 2);
    }

    fn count_blobs(store: &BlobStore) -> usize {
        fs::read_dir(store.root())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|shard| shard.file_name() != STAGING_DIR)
            .map(|shard| fs::read_dir(shard.path()).unwrap().count())
            .sum()
    }
}
