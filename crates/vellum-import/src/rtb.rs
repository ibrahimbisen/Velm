//! Reads assets out of a Miro `.rtb` backup.
//!
//! A `.rtb` is a ZIP. Its board content (`canvas.json`) is encrypted with a
//! server-side key and is not recoverable — `docs/02-miro-formats.md` records the
//! analysis. What *is* readable is every uploaded asset, at original resolution,
//! stored as `<resource-id>.<ext>`.
//!
//! That matters because Miro's clipboard carries image and document widgets as
//! *references* (`resource.id`) and never as pixels. A `.rtb` alongside a paste is
//! the highest-quality source for those bytes; the SVG export's embedded base64 is
//! the downscaled fallback.
//!
//! This module deliberately does not attempt to decrypt anything. It reads
//! `board.json` for identity, `resources.json` for the manifest, and serves assets
//! on demand rather than unpacking 109MB up front.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

/// Files that are encrypted and therefore never worth reading.
const ENCRYPTED_ENTRIES: &[&str] = &["canvas.json", "tables.json"];

/// One entry of the `resources.json` manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct Resource {
    /// Numeric asset id. Also the ZIP entry's stem, and the join key to a
    /// clipboard widget's `resource.id`.
    #[serde(deserialize_with = "id_as_string")]
    pub id: String,
    /// Original upload filename, e.g. `"image.png"`.
    pub name: String,
    pub extension: String,
    /// Miro's own classification; `"WIDGET"` throughout the observed board.
    #[serde(default, rename = "type")]
    pub kind: String,
    /// Miro's malware-scan flag. Refuse to serve anything flagged.
    #[serde(default)]
    pub infected: bool,
}

/// Miro writes ids as JSON numbers here but as strings in clipboard payloads.
/// Normalising to `String` lets the two be joined without a numeric cast that
/// would lose precision on 19-digit ids.
fn id_as_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    })
}

/// Board identity from `board.json`. The board's *content* is not here.
#[derive(Debug, Clone, Deserialize)]
pub struct BoardInfo {
    #[serde(deserialize_with = "id_as_string")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

impl BoardInfo {
    /// Miro's **public** board id — the one in a board URL and in a clipboard payload.
    ///
    /// `board.json` stores the internal signed 64-bit id; the public form is base64 of
    /// its big-endian bytes. Verified end to end against the reference board:
    /// `-1234567890123456789` → `7t3vC4IWfus=`, and `GET /v2/boards/7t3vC4IWfus=`
    /// answers `"name": "Reference Board"`.
    ///
    /// This is what lets a paste be *named*: the clipboard payload carries the board it
    /// came from as `boardId` in exactly this form, so matching it against the archives
    /// gives the board's real name with no typing and no network.
    #[must_use]
    pub fn api_id(&self) -> Option<String> {
        use base64::Engine as _;
        let internal: i64 = self.id.parse().ok()?;
        Some(base64::engine::general_purpose::STANDARD.encode(internal.to_be_bytes()))
    }
}

/// An opened `.rtb` archive.
pub struct RtbArchive {
    zip: zip::ZipArchive<File>,
    path: PathBuf,
    pub board: BoardInfo,
    /// Manifest entries by asset id.
    resources: HashMap<String, Resource>,
    /// ZIP entry name by asset id, resolved once so lookups don't rescan.
    entry_by_id: HashMap<String, String>,
}

impl RtbArchive {
    /// Opens a `.rtb`. Reads only the small manifests; assets stay on disk until
    /// requested, so opening a 109MB archive is cheap.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut zip = zip::ZipArchive::new(file)
            .with_context(|| format!("{} is not a ZIP archive", path.display()))?;

        let board: BoardInfo =
            serde_json::from_slice(&read_entry(&mut zip, "board.json")?).context("parsing board.json")?;

        #[derive(Deserialize)]
        struct Manifest {
            resources: Vec<Resource>,
        }
        let manifest: Manifest =
            serde_json::from_slice(&read_entry(&mut zip, "resources.json")?).context("parsing resources.json")?;

        let resources: HashMap<String, Resource> =
            manifest.resources.into_iter().map(|r| (r.id.clone(), r)).collect();

        // Assets are named `<id>.<ext>`. Index by stem so a manifest/extension
        // mismatch doesn't make an asset unreachable.
        let mut entry_by_id = HashMap::new();
        for name in zip.file_names() {
            if let Some((stem, _)) = name.rsplit_once('.')
                && !name.ends_with(".json")
            {
                entry_by_id.insert(stem.to_string(), name.to_string());
            }
        }

        Ok(Self { zip, path, board, resources, entry_by_id })
    }

    /// Manifest entries, in archive order.
    pub fn resources(&self) -> impl Iterator<Item = &Resource> {
        self.resources.values()
    }

    pub fn resource(&self, id: &str) -> Option<&Resource> {
        self.resources.get(id)
    }

    /// Whether this archive can supply bytes for an asset id.
    pub fn contains_asset(&self, id: &str) -> bool {
        self.entry_by_id.contains_key(id)
    }

    /// Reads an asset's bytes.
    ///
    /// Returns `Ok(None)` when the archive simply doesn't have it — expected when a
    /// paste references a board whose backup we don't hold. Errors on a genuine
    /// read failure, and refuses anything Miro flagged as infected.
    pub fn asset_bytes(&mut self, id: &str) -> Result<Option<Vec<u8>>> {
        if self.resources.get(id).is_some_and(|r| r.infected) {
            bail!("asset {id} is flagged as infected in resources.json; refusing to read it");
        }
        let Some(entry) = self.entry_by_id.get(id).cloned() else {
            return Ok(None);
        };
        Ok(Some(read_entry(&mut self.zip, &entry)?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Asset count actually present in the archive.
    pub fn asset_count(&self) -> usize {
        self.entry_by_id.len()
    }

    /// Names the encrypted entries this archive contains, so callers can explain
    /// the gap to the user rather than silently importing an empty board.
    pub fn encrypted_entries(&self) -> Vec<&'static str> {
        ENCRYPTED_ENTRIES
            .iter()
            .copied()
            .filter(|e| self.zip.file_names().any(|n| n == *e))
            .collect()
    }
}

/// Every `.rtb` the user has, searched as one.
///
/// A single archive was the whole model while there was one reference board and a
/// `--rtb` flag naming it. That does not survive contact with a real migration: a user
/// bringing 58 boards across has 58 backups, one paste can only carry one board's
/// widgets, and a flag set once at launch would supply assets for exactly one of them.
///
/// **Resolution is first-hit-wins across the set, keyed on the resource id alone.** No
/// board matching is needed and none is done: a Miro resource id is globally unique, so
/// asking every archive for one is correct as well as simple. It also degrades the right
/// way — an archive for a board you are not pasting simply never answers.
///
/// The set is deliberately not a `HashMap<board, archive>`. Two boards can share an
/// uploaded image, and a map keyed by board would fail to find it from the second board
/// while the bytes sat on disk.
#[derive(Default)]
pub struct ArchiveSet {
    archives: Vec<RtbArchive>,
}

impl ArchiveSet {
    /// An empty set. Every lookup answers `None`, which is [`AssetGap::NoArchive`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a `.rtb`, or **every `.rtb` in a directory**.
    ///
    /// The directory form is the one that matters: it is how a user points the app at a
    /// folder of backups once instead of naming each. Not recursive — a flat folder is
    /// what people make, and walking a tree invites reading a `.rtb` out of a Downloads
    /// directory nobody meant to include.
    ///
    /// An unreadable member is **skipped with a warning rather than failing the set**,
    /// because one corrupt download should not stop the other 57 boards importing.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut set = Self::new();
        set.add(path)?;
        Ok(set)
    }

    /// Adds a `.rtb`, or every `.rtb` in a directory, to an existing set.
    ///
    /// Separate from [`ArchiveSet::open`] so several sources compose — the app's own
    /// archive folder plus whatever `--rtb` named — without each replacing the last.
    pub fn add(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let set = self;
        if path.is_dir() {
            let mut paths: Vec<_> = std::fs::read_dir(path)
                .with_context(|| format!("reading {}", path.display()))?
                .filter_map(std::result::Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("rtb")))
                .collect();
            // Sorted so the first-hit-wins order is stable between runs rather than
            // whatever order the filesystem hands back.
            paths.sort();
            for p in paths {
                match RtbArchive::open(&p) {
                    Ok(archive) => set.archives.push(archive),
                    Err(error) => log::warn!("skipping {}: {error}", p.display()),
                }
            }
        } else {
            set.archives.push(RtbArchive::open(path)?);
        }
        Ok(())
    }

    /// Adds an already-open archive.
    pub fn push(&mut self, archive: RtbArchive) {
        self.archives.push(archive);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.archives.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.archives.is_empty()
    }

    /// The boards these archives back up, for reporting which ones are loaded.
    pub fn boards(&self) -> impl Iterator<Item = &BoardInfo> {
        self.archives.iter().map(|a| &a.board)
    }

    /// The name of the board a clipboard payload came from, if one of these backs it up.
    ///
    /// `api_id` is the `boardId` in the payload — `"7t3vC4IWfus="`. Matching it here is
    /// what makes Board ▸ Import from Miro able to *offer* the board's real name instead
    /// of asking the user to type it once per board, which for a 58-board migration is
    /// the difference between a workflow and a chore.
    #[must_use]
    pub fn name_of(&self, api_id: &str) -> Option<&str> {
        self.archives
            .iter()
            .find(|a| a.board.api_id().is_some_and(|id| id == api_id))
            .map(|a| a.board.name.as_str())
    }

    /// The backed-up board's name, **only when the set holds exactly one archive**.
    ///
    /// Used to title a board created straight from a `--rtb`. `None` for a folder of
    /// backups on purpose: with 58 of them there is no single right answer, and picking
    /// the first would name every imported board after whichever sorted first.
    #[must_use]
    pub fn sole_board_name(&self) -> Option<&str> {
        match self.archives.as_slice() {
            [only] => Some(only.board.name.as_str()),
            _ => None,
        }
    }

    /// Total assets across the set.
    #[must_use]
    pub fn asset_count(&self) -> usize {
        self.archives.iter().map(RtbArchive::asset_count).sum()
    }

    /// Reads an asset's bytes from whichever archive has it.
    ///
    /// An `infected` flag makes one archive refuse; that is reported rather than
    /// silently retried against the next, since the answer "Miro thinks this file is
    /// malware" does not become different because another backup also holds it.
    pub fn asset_bytes(&mut self, id: &str) -> Result<Option<Vec<u8>>> {
        for archive in &mut self.archives {
            if !archive.contains_asset(id) {
                continue;
            }
            return archive.asset_bytes(id);
        }
        Ok(None)
    }
}

impl From<RtbArchive> for ArchiveSet {
    fn from(archive: RtbArchive) -> Self {
        Self { archives: vec![archive] }
    }
}

impl std::fmt::Debug for ArchiveSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveSet")
            .field("archives", &self.archives.len())
            .field("assets", &self.asset_count())
            .finish()
    }
}

/// Hand-written so the archive can appear in errors and test assertions without
/// requiring `Debug` from the ZIP reader, and without dumping the manifest.
impl std::fmt::Debug for RtbArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtbArchive")
            .field("path", &self.path)
            .field("board", &self.board.name)
            .field("assets", &self.entry_by_id.len())
            .finish()
    }
}

fn read_entry<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut entry = zip
        .by_name(name)
        .with_context(|| format!("{name} is missing from the archive"))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).with_context(|| format!("reading {name}"))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Builds a `.rtb`-shaped archive, including the encrypted entries, so tests
    /// exercise the real structure rather than an idealised one.
    ///
    /// Each call gets its own directory, and the returned guard must be held for
    /// as long as the archive is read. Sharing one path across tests made them
    /// race: `cargo test` runs them in parallel, so one would truncate and rewrite
    /// the file while another was mid-read, producing "invalid checksum" and "not
    /// a ZIP archive" failures that depended on thread scheduling.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.rtb");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let opts: zip::write::FileOptions<'_, ()> = Default::default();

        zip.start_file("board.json", opts).unwrap();
        zip.write_all(br#"{"id":-1234567890123456789,"name":"Reference Board","description":""}"#)
            .unwrap();

        zip.start_file("resources.json", opts).unwrap();
        zip.write_all(
            br#"{"document":null,"image":null,"resources":[
                {"id":3458764500000000006,"name":"photo.jpg","extension":"jpg","type":"WIDGET","infected":false},
                {"id":3458764500000000007,"name":"image.png","extension":"png","type":"WIDGET","infected":false},
                {"id":9999999999999999999,"name":"bad.png","extension":"png","type":"WIDGET","infected":true}]}"#,
        )
        .unwrap();

        // Encrypted in a real archive; here just opaque bytes.
        zip.start_file("canvas.json", opts).unwrap();
        zip.write_all(&[0xd9, 0xe7, 0xad, 0x95]).unwrap();

        zip.start_file("3458764500000000006.jpg", opts).unwrap();
        zip.write_all(b"\xff\xd8\xff\xe0JFIF-bytes").unwrap();
        zip.start_file("3458764500000000007.png", opts).unwrap();
        zip.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        zip.start_file("9999999999999999999.png", opts).unwrap();
        zip.write_all(b"malware").unwrap();

        zip.finish().unwrap();
        (dir, path)
    }

    /// The whole point of a set: a folder of backups, searched as one.
    ///
    /// Pointing the app at a directory is the only workable shape for a migration —
    /// 58 boards means 58 `.rtb` files, one paste carries one board, and a single
    /// archive fixed at launch would supply assets for exactly one of them.
    #[test]
    fn a_folder_of_backups_is_searched_as_one_set() {
        let (dir_a, path_a) = fixture();
        let (_dir_b, path_b) = fixture();

        // Two backups plus a file that is not one, in a folder of their own.
        let home = tempfile::tempdir().unwrap();
        std::fs::copy(&path_a, home.path().join("board-a.rtb")).unwrap();
        std::fs::copy(&path_b, home.path().join("board-b.rtb")).unwrap();
        std::fs::write(home.path().join("notes.txt"), b"not an archive").unwrap();
        // A subdirectory is not descended into: a flat folder is what people make, and
        // walking a tree invites reading a `.rtb` nobody meant to include.
        std::fs::create_dir(home.path().join("old")).unwrap();
        std::fs::copy(&path_a, home.path().join("old/board-c.rtb")).unwrap();

        let mut set = ArchiveSet::open(home.path()).unwrap();
        assert_eq!(set.len(), 2, "the .txt and the subdirectory are skipped");

        // Resolution is by resource id across the set, with no board matching at all.
        assert_eq!(
            set.asset_bytes("3458764500000000006").unwrap().as_deref(),
            Some(&b"\xff\xd8\xff\xe0JFIF-bytes"[..])
        );
        assert_eq!(set.asset_bytes("nope").unwrap(), None);

        // With more than one archive there is no single board to name an import after.
        assert_eq!(set.sole_board_name(), None);
        assert_eq!(ArchiveSet::open(&path_a).unwrap().sole_board_name(), Some("Reference Board"));

        drop(dir_a);
    }

    /// An empty set is the ordinary state before the user has put anything in the
    /// folder, and it must answer like no archive rather than like a failure.
    #[test]
    fn an_empty_set_answers_none_rather_than_erroring() {
        let mut set = ArchiveSet::new();
        assert!(set.is_empty());
        assert_eq!(set.asset_count(), 0);
        assert_eq!(set.asset_bytes("3458764500000000006").unwrap(), None);
        assert_eq!(set.sole_board_name(), None);
    }

    /// One unreadable download must not stop the other boards importing.
    #[test]
    fn a_corrupt_member_is_skipped_not_fatal() {
        let (_dir, path) = fixture();
        let home = tempfile::tempdir().unwrap();
        std::fs::copy(&path, home.path().join("good.rtb")).unwrap();
        std::fs::write(home.path().join("truncated.rtb"), b"not a zip at all").unwrap();

        let set = ArchiveSet::open(home.path()).expect("a bad member does not fail the set");
        assert_eq!(set.len(), 1);
        assert_eq!(set.sole_board_name(), Some("Reference Board"));
    }

    #[test]
    fn reads_identity_and_manifest() {
        let (_dir, path) = fixture();
        let mut a = RtbArchive::open(path).unwrap();
        assert_eq!(a.board.name, "Reference Board");
        // 19-digit ids must survive as strings, not lose precision via f64.
        assert_eq!(a.board.id, "-1234567890123456789");
        assert_eq!(a.resource("3458764500000000006").unwrap().name, "photo.jpg");
        assert_eq!(a.asset_count(), 3);
        assert!(a.asset_bytes("3458764500000000006").unwrap().unwrap().starts_with(b"\xff\xd8\xff"));
    }

    /// The join key between a clipboard `image` widget and the backup's pixels.
    #[test]
    fn asset_ids_join_clipboard_widgets_to_bytes() {
        let (_dir, path) = fixture();
        let mut a = RtbArchive::open(path).unwrap();
        assert!(a.contains_asset("3458764500000000007"));
        assert!(a.asset_bytes("3458764500000000007").unwrap().unwrap().starts_with(b"\x89PNG"));
    }

    /// A missing asset is an expected condition, not a failure.
    #[test]
    fn unknown_asset_is_none_not_error() {
        let (_dir, path) = fixture();
        let mut a = RtbArchive::open(path).unwrap();
        assert!(!a.contains_asset("123"));
        assert!(a.asset_bytes("123").unwrap().is_none());
    }

    #[test]
    fn refuses_infected_assets() {
        let (_dir, path) = fixture();
        let mut a = RtbArchive::open(path).unwrap();
        let err = a.asset_bytes("9999999999999999999").unwrap_err();
        assert!(err.to_string().contains("infected"), "{err}");
    }

    /// Callers must be able to tell the user *why* content is missing.
    #[test]
    fn reports_encrypted_entries() {
        let (_dir, path) = fixture();
        let a = RtbArchive::open(path).unwrap();
        assert_eq!(a.encrypted_entries(), vec!["canvas.json"]);
    }

    #[test]
    fn non_zip_input_errors_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vellum-not-a-zip.rtb");
        std::fs::write(&p, b"definitely not a zip").unwrap();
        let err = RtbArchive::open(&p).unwrap_err();
        assert!(err.to_string().contains("not a ZIP"), "{err}");
    }
}
