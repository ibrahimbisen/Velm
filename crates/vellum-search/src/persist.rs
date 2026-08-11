//! The on-disk form, and loading without rebuilding.
//!
//! # What is stored, and what is recomputed
//!
//! The file holds exactly the two things that are expensive to produce and
//! impossible to derive from anything cheaper:
//!
//! - the **document table** — ids, kind, colour, tags and text, which is the
//!   corpus itself;
//! - the **term dictionary and its posting lists**, which is the tokenised,
//!   inverted form of that corpus.
//!
//! Everything else is rebuilt on load, because everything else is a *view*. The
//! facet indexes are one pass over the document table with no tokenisation and no
//! string comparison; storing them would add a third of the file for something
//! derivable, and — worse — would create a file that could disagree with itself.
//! There is no way to detect a facet list that has drifted from the documents it
//! describes, and no way to repair one.
//!
//! The dictionary is written in **ascending term order**, which is not a cosmetic
//! choice: it means load assigns term ids in sorted order, so the dictionary's
//! sorted view is the identity permutation and reconstructing it costs no
//! comparisons at all.
//!
//! # Compactness
//!
//! Everything numeric is a LEB128 varint, and both document ids and token positions
//! are delta-encoded against their predecessor within a posting list. Those lists
//! are already ascending — the write path keeps them that way for its own reasons —
//! so the deltas are small, and the overwhelming majority occupy one byte. A
//! posting therefore costs about three bytes on the corpus this targets — a
//! document delta, a position count and one position delta — where the same three
//! fields at fixed width would be twelve.
//!
//! Documents are **renumbered densely** as they are written. A long session's
//! deletions leave holes in the id space, and a file that preserved them would grow
//! monotonically with churn while describing the same corpus.
//!
//! # Integrity
//!
//! The header carries a checksum of the body. It is verified before a single field
//! is parsed, and a mismatch is an error rather than a best effort, because the
//! failure mode of a silently corrupted posting list is not a crash — it is a
//! search that returns the wrong items, indefinitely, with no symptom. Rebuilding
//! the index from the documents is always available and takes seconds; trusting a
//! damaged one is not recoverable at all.
//!
//! # Why not `serde`
//!
//! An index file is a file format, and its byte layout is a compatibility contract
//! this crate has to keep. A derive would delegate that contract to a crate whose
//! representation is free to change between minor versions, and would encode field
//! *names* or a schema-less sequence rather than the varints and deltas that make
//! the file small. This module is about two hundred lines and owns the format
//! outright.

use std::io::Write;
use std::path::Path;

use crate::colour::Colour;
use crate::dictionary::Dictionary;
use crate::error::SearchError;
use crate::index::{Doc, Index, Posting, PostingList};

/// File magic. Ends in a byte that is not valid UTF-8 so that a text editor, and
/// anything sniffing content types, treats the file as binary.
const MAGIC: &[u8; 8] = b"VLMSRCH\xff";

/// Bumped whenever the layout below changes in a way an older build would misread.
/// A file from a newer version is refused, not guessed at.
pub const FORMAT_VERSION: u16 = 1;

/// Ceiling on a length read from the file before the data behind it is seen.
///
/// A corrupt count would otherwise become a `Vec::with_capacity` of that many
/// elements — a several-gigabyte allocation from a few flipped bits, on a machine
/// `docs/01-architecture.md` §3 budgets 400MB of idle memory for. Capacity is
/// reserved up to this and then grows normally, so a legitimately larger index
/// costs a few reallocations and nothing else.
const CAPACITY_CEILING: usize = 1 << 20;

impl Index {
    /// Writes the index to `path`, atomically.
    ///
    /// The file is written to a sibling temporary and renamed over the target, so
    /// an interrupted save leaves the previous index intact rather than a truncated
    /// one. A half-written index is worse than none: it loads, and it is wrong.
    ///
    /// Takes `&mut self` in order to clear [`Index::is_dirty`].
    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<(), SearchError> {
        let path = path.as_ref();
        let temporary = path.with_extension("vsx-tmp");
        {
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&self.to_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, path)?;
        self.mark_clean();
        Ok(())
    }

    /// Reads an index written by [`Index::save`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SearchError> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    /// The index in its on-disk form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();

        // Documents, densely renumbered. `dense` maps the in-memory id to the id
        // the file uses, and the posting lists below are rewritten through it.
        let mut dense = vec![u32::MAX; self.doc_slots()];
        let mut documents = Vec::with_capacity(self.len());
        for (written, (id, doc)) in self.live_docs().enumerate() {
            dense[id as usize] = written as u32;
            documents.push(doc);
        }
        write_uvarint(&mut body, documents.len() as u64);
        for doc in documents {
            write_str(&mut body, &doc.board);
            write_str(&mut body, &doc.item);
            write_str(&mut body, &doc.kind);
            write_str(&mut body, &doc.text);
            // Zero is "no colour"; a real colour is stored one higher. Encoding an
            // `Option` as a sentinel rather than a flag byte saves a byte on every
            // uncoloured item, and most items are uncoloured.
            write_uvarint(&mut body, doc.colour.map_or(0, |c| u64::from(c.packed()) + 1));
            write_uvarint(&mut body, doc.tags.len() as u64);
            for tag in &doc.tags {
                write_str(&mut body, tag);
            }
        }

        // The dictionary, in ascending term order, each term followed by its
        // postings.
        let dictionary = self.dictionary();
        write_uvarint(&mut body, dictionary.len() as u64);
        for &term in dictionary.sorted_ids() {
            write_str(&mut body, dictionary.text(term).expect("sorted holds live ids"));
            let postings = self.postings_of(term);
            write_uvarint(&mut body, postings.len() as u64);
            let mut previous_doc = 0u32;
            for posting in postings.entries() {
                let doc = dense[posting.doc as usize];
                debug_assert_ne!(doc, u32::MAX, "a posting for a dead document");
                write_uvarint(&mut body, u64::from(doc - previous_doc));
                previous_doc = doc;
                write_uvarint(&mut body, posting.positions.len() as u64);
                let mut previous_position = 0u32;
                for &position in &posting.positions {
                    write_uvarint(&mut body, u64::from(position - previous_position));
                    previous_position = position;
                }
            }
        }

        let mut out = Vec::with_capacity(body.len() + MAGIC.len() + 12);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved
        out.extend_from_slice(&checksum(&body).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Parses the on-disk form. See the module docs on what is rebuilt.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SearchError> {
        if bytes.len() < MAGIC.len() + 12 || &bytes[..MAGIC.len()] != MAGIC {
            return Err(SearchError::NotAnIndex);
        }
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if version != FORMAT_VERSION {
            return Err(SearchError::UnsupportedVersion {
                found: version,
                expected: FORMAT_VERSION,
            });
        }
        let stored = u64::from_le_bytes(
            bytes[12..20].try_into().expect("the length was checked above"),
        );
        let body = &bytes[20..];
        let computed = checksum(body);
        if stored != computed {
            return Err(SearchError::ChecksumMismatch { stored, computed });
        }

        let mut reader = Reader { rest: body };
        let document_count = reader.length("document count")?;
        let mut documents = Vec::with_capacity(document_count.min(CAPACITY_CEILING));
        for _ in 0..document_count {
            let board = reader.string("board id")?;
            let item = reader.string("item id")?;
            let kind = reader.string("item kind")?;
            let text = reader.string("item text")?;
            let colour = match reader.uvarint("colour")? {
                0 => None,
                packed => Some(Colour::from_packed(
                    u32::try_from(packed - 1).map_err(|_| SearchError::Malformed("colour"))?,
                )),
            };
            let tag_count = reader.length("tag count")?;
            let mut tags = Vec::with_capacity(tag_count.min(CAPACITY_CEILING));
            for _ in 0..tag_count {
                tags.push(reader.string("tag")?);
            }
            documents.push(Doc { board, item, kind, text, colour, tags });
        }

        let term_count = reader.length("term count")?;
        let mut terms = Vec::with_capacity(term_count.min(CAPACITY_CEILING));
        let mut postings = Vec::with_capacity(term_count.min(CAPACITY_CEILING));
        for _ in 0..term_count {
            terms.push(reader.string("term")?);
            let posting_count = reader.length("posting count")?;
            let mut entries = Vec::with_capacity(posting_count.min(CAPACITY_CEILING));
            let mut doc = 0u32;
            for _ in 0..posting_count {
                doc = doc
                    .checked_add(reader.u32("document delta")?)
                    .ok_or(SearchError::Malformed("document id overflow"))?;
                if doc as usize >= documents.len() {
                    return Err(SearchError::Malformed("posting for an absent document"));
                }
                let position_count = reader.length("position count")?;
                let mut positions = Vec::with_capacity(position_count.min(CAPACITY_CEILING));
                let mut position = 0u32;
                for _ in 0..position_count {
                    position = position
                        .checked_add(reader.u32("position delta")?)
                        .ok_or(SearchError::Malformed("position overflow"))?;
                    positions.push(position);
                }
                entries.push(Posting { doc, positions });
            }
            postings.push(PostingList::from_sorted(entries));
        }

        if !reader.rest.is_empty() {
            return Err(SearchError::Malformed("trailing bytes after the dictionary"));
        }
        Ok(Self::from_parts(documents, Dictionary::from_sorted(terms), postings))
    }
}

/// FNV-1a over the body.
///
/// Not a cryptographic hash and not trying to be: the threat is a partial write or
/// a bad sector, not a forged index file. FNV is a few lines, has no dependency,
/// and runs at roughly a byte per cycle — fast enough that verifying on every load
/// does not register against reading the file in the first place.
fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn write_uvarint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn write_str(out: &mut Vec<u8>, text: &str) {
    write_uvarint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl Reader<'_> {
    fn uvarint(&mut self, what: &'static str) -> Result<u64, SearchError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.rest.split_first().ok_or(SearchError::Malformed(what))?;
            self.rest = rest;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(SearchError::Malformed(what))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, SearchError> {
        u32::try_from(self.uvarint(what)?).map_err(|_| SearchError::Malformed(what))
    }

    fn length(&mut self, what: &'static str) -> Result<usize, SearchError> {
        let value = self.uvarint(what)?;
        // A length can never exceed what is left in the file, whatever it claims.
        // Checking here turns a corrupt count into an error rather than a loop that
        // allocates until it runs out of bytes.
        if value > self.rest.len() as u64 {
            return Err(SearchError::Malformed(what));
        }
        Ok(value as usize)
    }

    fn string(&mut self, what: &'static str) -> Result<Box<str>, SearchError> {
        let length = self.length(what)?;
        let (bytes, rest) =
            self.rest.split_at_checked(length).ok_or(SearchError::Malformed(what))?;
        self.rest = rest;
        std::str::from_utf8(bytes).map(Into::into).map_err(|_| SearchError::Malformed(what))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;
    use crate::source::Item;

    fn corpus() -> Index {
        let yellow = Colour::parse("#fff79e").unwrap();
        Index::build([
            Item::new("garage", "s1", "sticky", "Cooling fan relay")
                .with_colour(yellow)
                .with_tags(["review", "engine"]),
            Item::new("garage", "s2", "sticky", "Water pump water").with_colour(yellow),
            Item::new("garage", "f1", "frame", "Engine bay"),
            Item::new("garage", "i1", "ink", ""),
            Item::new("keys", "t1", "text", "Switch lubrication — Motoröl 5W30"),
        ])
    }

    fn round_trip(index: &Index) -> Index {
        Index::from_bytes(&index.to_bytes()).expect("what we just wrote")
    }

    #[test]
    fn a_round_trip_preserves_every_document_and_every_posting() {
        let original = corpus();
        let restored = round_trip(&original);
        assert_eq!(restored.stats(), original.stats());
        assert_eq!(restored.boards().collect::<Vec<_>>(), original.boards().collect::<Vec<_>>());
        assert_eq!(restored.kinds().collect::<Vec<_>>(), original.kinds().collect::<Vec<_>>());
        assert_eq!(restored.tags().collect::<Vec<_>>(), original.tags().collect::<Vec<_>>());
        assert_eq!(restored.colours().collect::<Vec<_>>(), original.colours().collect::<Vec<_>>());
    }

    #[test]
    fn a_restored_index_answers_every_query_identically() {
        let original = corpus();
        let restored = round_trip(&original);
        for text in [
            "cooling",
            "cool",
            "\"cooling fan\"",
            "water",
            "kind:frame",
            "colour:yellow",
            "tag:review",
            "board:keys",
            "all yellow stickies",
            "motoröl",
            "every frame",
        ] {
            let query = Query::parse(text);
            assert_eq!(restored.search(&query), original.search(&query), "query {text:?}");
        }
    }

    #[test]
    fn a_restored_index_is_still_incrementally_updatable() {
        let mut restored = round_trip(&corpus());
        restored.upsert(&Item::new("garage", "s1", "sticky", "Thermostat"));
        restored.upsert(&Item::new("new", "n1", "sticky", "Cooling"));

        let mut expected = corpus();
        expected.upsert(&Item::new("garage", "s1", "sticky", "Thermostat"));
        expected.upsert(&Item::new("new", "n1", "sticky", "Cooling"));

        assert_eq!(restored.stats(), expected.stats());
        let query = Query::parse("cooling");
        assert_eq!(restored.search(&query), expected.search(&query));
        assert!(restored.search(&Query::parse("relay")).is_empty(), "the old text is gone");
    }

    /// Deletions leave holes in the id space; the file must not.
    #[test]
    fn deleted_documents_are_renumbered_away_rather_than_stored_as_holes() {
        let mut churned = corpus();
        churned.remove("garage", "s2");
        churned.remove("garage", "i1");
        let restored = round_trip(&churned);

        assert_eq!(restored.len(), 3);
        assert_eq!(restored.stats(), churned.stats());
        let query = Query::parse("cooling");
        assert_eq!(restored.search(&query), churned.search(&query));

        // And the bytes are exactly those of an index that never held them.
        let mut fresh = Index::new();
        fresh.extend([
            Item::new("garage", "s1", "sticky", "Cooling fan relay")
                .with_colour(Colour::parse("#fff79e").unwrap())
                .with_tags(["review", "engine"]),
            Item::new("garage", "f1", "frame", "Engine bay"),
            Item::new("keys", "t1", "text", "Switch lubrication — Motoröl 5W30"),
        ]);
        assert_eq!(fresh.to_bytes(), churned.to_bytes());
    }

    #[test]
    fn an_empty_index_round_trips() {
        let restored = round_trip(&Index::new());
        assert!(restored.is_empty());
        assert_eq!(restored.stats(), crate::IndexStats::default());
    }

    #[test]
    fn saving_and_loading_through_the_filesystem_works_and_clears_the_dirty_flag() {
        let directory = std::env::temp_dir().join(format!(
            "vellum-search-persist-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("index.vsx");

        let mut original = corpus();
        assert!(original.is_dirty());
        original.save(&path).unwrap();
        assert!(!original.is_dirty());

        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.stats(), original.stats());
        assert!(!directory.join("index.vsx-tmp").exists(), "the temporary is renamed away");

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_file_that_is_not_an_index_is_refused() {
        assert!(matches!(Index::from_bytes(b""), Err(SearchError::NotAnIndex)));
        assert!(matches!(Index::from_bytes(b"hello, world"), Err(SearchError::NotAnIndex)));
    }

    #[test]
    fn a_future_format_version_is_refused_rather_than_guessed_at() {
        let mut bytes = corpus().to_bytes();
        bytes[8] = 99;
        assert!(matches!(
            Index::from_bytes(&bytes),
            Err(SearchError::UnsupportedVersion { found: 99, expected: FORMAT_VERSION })
        ));
    }

    #[test]
    fn a_single_flipped_bit_anywhere_in_the_body_is_caught() {
        let original = corpus().to_bytes();
        for at in (20..original.len()).step_by(7) {
            let mut damaged = original.clone();
            damaged[at] ^= 0b0001_0000;
            assert!(
                matches!(Index::from_bytes(&damaged), Err(SearchError::ChecksumMismatch { .. })),
                "byte {at} was not caught",
            );
        }
    }

    #[test]
    fn a_truncated_file_is_an_error_rather_than_a_panic() {
        let original = corpus().to_bytes();
        for length in 0..original.len() {
            let result = Index::from_bytes(&original[..length]);
            assert!(result.is_err(), "a prefix of {length} bytes parsed as a whole index");
        }
    }

    #[test]
    fn varints_round_trip_across_the_whole_range() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut bytes = Vec::new();
            write_uvarint(&mut bytes, value);
            let mut reader = Reader { rest: &bytes };
            assert_eq!(reader.uvarint("test").unwrap(), value);
            assert!(reader.rest.is_empty());
        }
    }

    #[test]
    fn a_varint_that_never_terminates_is_an_error() {
        let mut reader = Reader { rest: &[0x80; 32] };
        assert!(reader.uvarint("test").is_err());
    }

    /// The compactness claim, measured rather than asserted in prose: the *marginal*
    /// cost of a posting, isolated by growing the corpus by terms alone.
    #[test]
    fn a_posting_costs_about_three_bytes_on_disk() {
        const DOCS: usize = 2000;
        const ADDED: &str = " bb cc dd ee ff";

        let lean = Index::build(
            (0..DOCS).map(|n| Item::new("b", format!("{n:04}"), "sticky", format!("aa{n:04}"))),
        );
        let fat = Index::build((0..DOCS).map(|n| {
            Item::new("b", format!("{n:04}"), "sticky", format!("aa{n:04}{ADDED}"))
        }));

        let extra_postings = fat.stats().postings - lean.stats().postings;
        assert_eq!(extra_postings, DOCS * 5, "five new terms in every document");

        let growth = fat.to_bytes().len() - lean.to_bytes().len() - DOCS * ADDED.len();
        let per_posting = growth as f64 / extra_postings as f64;
        assert!(
            (2.9..3.3).contains(&per_posting),
            "{per_posting:.2} bytes per posting, expected about three",
        );
    }
}
