//! What can go wrong.
//!
//! Note what is *not* here: querying. [`Query::parse`](crate::Query::parse) cannot
//! fail, and [`Index::search`](crate::Index::search) returns a `Vec`, not a
//! `Result`. A search box receives half-typed nonsense on every keystroke — an
//! unbalanced quote, a filter with no value, a stray colon — and the only useful
//! response to all of it is "here is what that matches so far". Making the caller
//! handle a parse error would mean the UI either swallowing it or flashing a
//! message the user is already in the middle of fixing.
//!
//! So every error below concerns a *file*: the index on disk was written by a
//! different version, was truncated, or the filesystem said no.

/// A failure loading or storing an index.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("search index i/o failed: {0}")]
    Io(#[from] std::io::Error),

    /// The file does not begin with this crate's magic. Almost always a path
    /// pointing at something that is not an index at all.
    #[error("not a Velm search index (bad magic)")]
    NotAnIndex,

    /// The file is an index, but of a format this build does not read. Reported
    /// rather than guessed at: a best-effort parse of an unknown layout produces a
    /// silently wrong index, which is worse than rebuilding from the documents.
    #[error("search index format v{found} is not readable by this build (expects v{expected})")]
    UnsupportedVersion { found: u16, expected: u16 },

    /// The file ended early, or a length field pointed past the end. Carries where
    /// the parse gave up so a truncated write is distinguishable from a corrupted
    /// one in a bug report.
    #[error("search index is malformed: {0}")]
    Malformed(&'static str),

    /// The body does not hash to the value in the header. The index is rebuilt from
    /// the documents rather than trusted, because a bit-flip inside a posting list
    /// makes search quietly return the wrong items instead of failing.
    #[error("search index checksum mismatch: stored {stored:#018x}, computed {computed:#018x}")]
    ChecksumMismatch { stored: u64, computed: u64 },
}
