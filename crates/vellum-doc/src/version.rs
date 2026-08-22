//! Putting a [`Version`](crate::Version) on the wire.
//!
//! [`Board::version`](crate::Board::version),
//! [`Board::export_since`](crate::Board::export_since) and
//! [`Board::apply`](crate::Board::apply) have been here since the document layer was
//! written, and between them they are the whole of sync: a peer says what it has
//! seen, the other answers with everything it is missing. The one piece missing was
//! a way to *say* it. `Version` is opaque on purpose — its own doc comment exists so
//! that the persistence layer never has to depend on Loro directly — and an opaque
//! type cannot cross HTTP.
//!
//! This module is that byte form and nothing else. It knows nothing about the sync
//! endpoint, its framing or its transport.
//!
//! # RULE ZERO — a version vector is not board content
//!
//! A version vector says only *what this peer has already seen*. None of it is
//! anybody's work: lose one outright and the cost is bandwidth, because
//! [`Board::apply`](crate::Board::apply) merges and is idempotent, so answering an
//! empty version with the whole document is always correct — the receiver replays
//! operations it already holds and its board does not change by a byte.
//! `a_corrupt_version_costs_bandwidth_and_never_content` asserts exactly that,
//! against a board that is already current.
//!
//! The dangerous direction is the other one. A version that claims *more* than the
//! peer really has makes the far side send *less*, and the operations in that gap
//! are never delivered at all — no error, no retry, just a board that quietly
//! disagrees with itself on the next machine. That is the only way this module could
//! lose an edit. So a version that will not parse degrades to **empty**, which
//! claims the least of any version, and never to whatever happened to parse before
//! the bytes ran out. postcard is all-or-nothing — `from_bytes` returns a whole
//! `VersionVector` or an error, never a half-filled map — which is what makes the
//! safe degradation the only one on offer.
//!
//! Both behaviours are here because they belong to different callers:
//!
//! - [`Version::decode`] is strict and returns the error. A handler that wants to
//!   count malformed requests, or a test, needs to be told.
//! - [`Version::decode_or_empty`] is the sync handler's call: a request whose
//!   version will not parse is answered with everything, which is what a client's
//!   first sync gets anyway.
//!
//! What must never happen is a **panic**, and that is not a matter of taste. These
//! bytes arrive from the network and `[profile.release]` sets `panic = "abort"`, so
//! a panic in this decoder is the whole server going down on one malformed request.
//! `garbage_off_the_network_is_an_error_rather_than_a_panic` is the guard.
//!
//! # The encoding
//!
//! `loro::VersionVector::{encode, decode}` (loro-internal 1.13.7,
//! `src/version.rs:962` and `:967`) is postcard over the `{PeerID → Counter}` map: a
//! varint entry count, then a varint peer and a varint counter per entry. Sixteen
//! bytes per peer at worst, and a board edited on one machine has one peer, so a
//! version on the wire is a dozen bytes rather than a header.
//!
//! Two properties of that encoding are load-bearing and neither is visible from the
//! call:
//!
//! - **An empty version does not encode to an empty slice.** postcard writes the
//!   entry count first, so `Version::empty().encode()` is one byte — `0x00`, zero
//!   entries — while a client that has never synced sends no version bytes at all.
//!   Those are two spellings of the same claim and both have to mean "seen nothing",
//!   so [`Version::decode`] answers `Version::empty()` for the empty slice before
//!   postcard, which would refuse it, ever sees it.
//! - **`postcard::from_bytes` ignores trailing bytes** (postcard 1.1.3,
//!   `src/de/mod.rs:12`, *"the unused portion (if any) of the byte slice is not
//!   returned"*). Decoding is therefore **not** a check on the framing: hand it the
//!   version bytes plus the first byte of the delta and it succeeds. The four-byte
//!   length prefix is the frame, and getting it exact is the transport's job — this
//!   module cannot notice a mis-framed request.
//!
//! There is deliberately no byte cap here, and that was checked rather than assumed.
//! A hostile entry count cannot make the decoder allocate: postcard's
//! `MapAccess::size_hint` answers `None` once the promised count exceeds the bytes
//! actually remaining (`src/de/deserializer.rs:164`), and serde caps a map's
//! pre-allocation at 1 MiB of entries regardless (serde 1.0.229,
//! `size_hint::cautious`). A claimed 2^60 entries therefore reserves nothing and
//! errors on the first entry it cannot read. Bounding the request *body* is still
//! the transport's job, not this one's.

use crate::board::Version;
use crate::error::DocError;
use loro::{LoroError, VersionVector};

/// Bytes that were offered as a version vector and are not one.
///
/// One variant, because malformed is the only way this can fail: encoding is
/// infallible and an empty slice is a legal version. It is its own type rather than
/// a new [`DocError`] variant because a version arrives from the *network*, where
/// every other `DocError` arrives from a board file — a sync handler wants to answer
/// "your request is malformed" without having to decide whether the user's document
/// is damaged.
#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("version vector from the wire is malformed: {0}")]
    Malformed(#[source] LoroError),
}

/// So that `Version::decode(bytes)?` still works inside a function returning this
/// crate's own [`Result`](crate::Result).
///
/// ⚠ This is why [`DocError`] must **not** grow a `#[from] VersionError` variant:
/// thiserror would generate a second `From<VersionError> for DocError` and the two
/// would collide.
impl From<VersionError> for DocError {
    fn from(err: VersionError) -> Self {
        DocError::Malformed(err.to_string())
    }
}

impl Version {
    /// The version of a board that has seen nothing — a client before its first
    /// sync, and the answer to any version that will not parse.
    ///
    /// A function rather than the `Version::EMPTY` constant a caller reaches for
    /// first: a version vector is a hash map and `FxHashMap::default()` is not a
    /// `const fn`. It costs nothing to call — an empty `HashMap` holds no heap
    /// buffer.
    pub fn empty() -> Self {
        Version(VersionVector::default())
    }

    /// Whether this version has seen nothing.
    ///
    /// Defined by comparison, not by asking the map whether it is empty, and the
    /// difference is real: Loro allows an entry whose counter is zero — its own note
    /// on `VersionVector` says a range's `start_vv` can carry one — so `{peer: 0}`
    /// has one entry and has still seen nothing. `VersionVector`'s `PartialEq`
    /// normalises those away and `HashMap::is_empty` does not.
    pub fn is_empty(&self) -> bool {
        *self == Self::empty()
    }

    /// This version as bytes, for the body of a sync request or response.
    ///
    /// Infallible, and that is postcard's shape rather than an assumption on our
    /// part: serialising a map of `u64 → i32` into a growable `Vec` has no failure
    /// mode, which is why `VersionVector::encode` itself returns a bare `Vec<u8>`.
    pub fn encode(&self) -> Vec<u8> {
        self.0.encode()
    }

    /// Reads a version off the wire.
    ///
    /// An **empty slice is not an error** — it is a client that has never synced,
    /// which is the most common request this will ever see. Answering it here rather
    /// than at the endpoint is what keeps a first sync an ordinary request instead of
    /// a branch every handler has to remember.
    ///
    /// Everything else is either a whole version vector or an error; see the module
    /// doc for why a partial read must never become a version.
    pub fn decode(bytes: &[u8]) -> Result<Self, VersionError> {
        if bytes.is_empty() {
            return Ok(Self::empty());
        }
        VersionVector::decode(bytes).map(Version).map_err(VersionError::Malformed)
    }

    /// [`Version::decode`], degrading bytes that will not parse to "seen nothing".
    ///
    /// The sync handler's call. Safe for the reason the module doc gives: the worst
    /// it can do is send a client the whole document when it needed a few hundred
    /// bytes, and [`Board::apply`](crate::Board::apply) merges what the client
    /// already had.
    ///
    /// It is a named function rather than an `unwrap_or_else` at each call site so
    /// that there is **one** derivation of that judgement instead of one per
    /// endpoint. The failure mode a second copy invites is a later handler degrading
    /// to something that is not empty, which is the one direction that loses
    /// operations.
    ///
    /// Reach for [`Version::decode`] where the failure is worth counting or logging;
    /// this one throws the reason away by design.
    pub fn decode_or_empty(bytes: &[u8]) -> Self {
        Self::decode(bytes).unwrap_or_else(|_| Self::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Board, ItemKind, NewItem, Placement, StyledText};

    fn sticky(text: &str) -> NewItem {
        NewItem::new(
            ItemKind::Sticky { text: StyledText::plain(text), background: None },
            Placement::new(0.0, 0.0, 199.0, 228.0),
        )
    }

    fn board_with(notes: &[&str]) -> Board {
        let mut board = Board::new();
        for note in notes {
            board.add(sticky(note)).unwrap();
        }
        board
    }

    #[test]
    fn a_real_version_survives_the_wire() {
        let board = board_with(&["fan"]);
        let version = board.version();

        let bytes = version.encode();
        assert!(!bytes.is_empty(), "a version that has seen an edit is not zero bytes");
        assert!(
            bytes.len() < 32,
            "a one-peer version should be a dozen-odd bytes, got {}",
            bytes.len()
        );

        assert_eq!(Version::decode(&bytes).unwrap(), version);
    }

    /// Equality is the cheap assertion; this is the one that matters. A version is
    /// only worth anything if [`Board::export_since`](crate::Board::export_since)
    /// treats the decoded copy exactly as it treats the original — a vector that
    /// compared equal and asked for a different span would still lose operations.
    ///
    /// Two peers on purpose. A single-entry map is the case a round-trip passes by
    /// accident; the second peer is what puts a real map through postcard.
    #[test]
    fn a_version_off_the_wire_asks_for_exactly_what_the_original_would() {
        let mut board = board_with(&["first"]);
        let checkpoint = board.version();
        let snapshot = board.to_bytes().unwrap();

        // A second machine picking the board up. `crdt()` is documented as the sync
        // transport's door and this is that transport's test.
        board.crdt().set_peer_id(0x5645_4C4D_0000_0001).unwrap();
        board.add(sticky("second")).unwrap();
        let later = board.version();

        assert_ne!(later, checkpoint);
        assert!(
            later.encode().len() > checkpoint.encode().len(),
            "the second peer should have added an entry to the vector"
        );

        let from_the_wire = Version::decode(&checkpoint.encode()).unwrap();
        assert_eq!(from_the_wire, checkpoint);
        let updates = board.export_since(&from_the_wire).unwrap();

        let mut replayed = Board::from_bytes(&snapshot).unwrap();
        assert_eq!(replayed.item_count(), 1);
        replayed.apply(&updates).unwrap();
        assert_eq!(
            replayed.items().unwrap(),
            board.items().unwrap(),
            "the span asked for by the decoded version did not carry the board across"
        );
    }

    #[test]
    fn a_client_that_has_never_synced_sends_an_empty_slice() {
        assert_eq!(Version::decode(&[]).unwrap(), Version::empty());
        assert!(Version::empty().is_empty());

        // The other spelling of the same claim: a client that encodes its still-empty
        // version sends one byte, postcard's zero entry count, not zero bytes. Both
        // have to arrive as "seen nothing" or the same client gets two different
        // answers depending on which one it happened to send.
        let encoded = Version::empty().encode();
        assert!(!encoded.is_empty(), "postcard writes an entry count even for an empty map");
        assert_eq!(Version::decode(&encoded).unwrap(), Version::empty());
        assert!(Version::decode(&encoded).unwrap().is_empty());
    }

    /// The test this module exists for. `panic = "abort"` in `[profile.release]`
    /// means a panic here is not an error response, it is the server process.
    #[test]
    fn garbage_off_the_network_is_an_error_rather_than_a_panic() {
        let junk: [&[u8]; 5] = [
            b"not a version vector",
            // A varint that never terminates.
            &[0xFF_u8; 32],
            // One entry promised, none supplied.
            &[0x01_u8],
            // Continuation bits, and then the bytes stop.
            &[0x80_u8, 0x80, 0x80, 0x80, 0x80],
            // ~2^32 entries claimed against six bytes of input — the case that would
            // pre-allocate, if postcard and serde did not both refuse to.
            &[0xFF_u8, 0xFF, 0xFF, 0xFF, 0x0F, 0x2A],
        ];

        for bytes in junk {
            assert!(
                Version::decode(bytes).is_err(),
                "{bytes:?} was accepted as a version vector"
            );
            assert!(
                Version::decode_or_empty(bytes).is_empty(),
                "{bytes:?} degraded to something other than the empty version"
            );
        }
    }

    /// A truncated body is the realistic corruption — a connection cut, or a length
    /// prefix that disagrees with what followed it. Every proper prefix of a real
    /// version either ends inside a varint or ends before the counter it promised,
    /// and both are errors; the empty prefix is excluded because an empty slice is a
    /// legal first sync.
    #[test]
    fn every_truncation_of_a_real_version_is_refused() {
        let full = board_with(&["fan"]).version().encode();
        assert!(full.len() > 2, "nothing to truncate: {} bytes", full.len());

        for cut in 1..full.len() {
            assert!(
                Version::decode(&full[..cut]).is_err(),
                "a {cut}-byte prefix of a {}-byte version decoded to a version",
                full.len()
            );
        }
    }

    /// RULE ZERO, as an assertion rather than an argument: degrading a corrupt
    /// version to empty makes the far side send everything, and everything applied
    /// to a board that already has it changes nothing.
    #[test]
    fn a_corrupt_version_costs_bandwidth_and_never_content() {
        let board = board_with(&["first", "second"]);

        let corrupt = Version::decode_or_empty(b"\xde\xad\xbe\xef");
        assert!(corrupt.is_empty(), "a corrupt version must claim the least, not the most");
        let everything = board.export_since(&corrupt).unwrap();

        let mut peer = Board::from_bytes(&board.to_bytes().unwrap()).unwrap();
        let before = peer.items().unwrap();
        peer.apply(&everything).unwrap();

        assert_eq!(
            peer.items().unwrap(),
            before,
            "a full resync rewrote a board that was already current"
        );
        assert_eq!(peer.item_count(), 2);
    }
}
