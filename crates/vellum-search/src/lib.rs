//! Search across every board at once.
//!
//! Vellum imports 20–100 real Miro boards. One of them alone holds 596 widgets and
//! 429 distinct text strings (`docs/02-miro-formats.md` §3), so "where did I write
//! that" stops being answerable by eye within about a week of use. This crate is
//! the answer: one inverted index over every board, built incrementally, persisted
//! compactly, and queried in under a millisecond.
//!
//! It is pure logic. No GPU, no UI, no filesystem beyond [`Index::save`] and
//! [`Index::load`], and — deliberately — no dependency on the document layer. See
//! [`source`] for the input contract and for how a `vellum_doc::Board` maps onto
//! it.
//!
//! ```
//! use vellum_search::{Colour, Index, Item, MatchTier, Query};
//!
//! let yellow = Colour::parse("#fff79e").unwrap();
//! let mut index = Index::build([
//!     Item::new("garage", "s1", "sticky", "Cooling fan relay").with_colour(yellow),
//!     Item::new("garage", "s2", "sticky", "Water pump").with_colour(yellow),
//!     Item::new("garage", "f1", "frame", "Engine bay"),
//!     Item::new("garage", "k1", "ink", ""),
//! ]);
//!
//! // A half-typed word finds the whole word.
//! let hits = index.search(&Query::parse("cool"));
//! assert_eq!(hits[0].item, "s1");
//! assert_eq!(hits[0].score.tier, MatchTier::Prefix);
//!
//! // A hit carries what it takes to jump straight to it, and a marked snippet.
//! assert_eq!(hits[0].board, "garage");
//! assert_eq!(hits[0].snippet.render("[", "]"), "[Cooling] fan relay");
//!
//! // Boards are visual, so kind and colour are searchable in plain words.
//! let yellow_stickies = index.search(&Query::parse("all yellow stickies"));
//! assert_eq!(yellow_stickies.len(), 2);
//! assert_eq!(index.search(&Query::parse("every frame"))[0].item, "f1");
//!
//! // Editing one item re-indexes one item.
//! index.upsert(&Item::new("garage", "s2", "sticky", "Thermostat housing"));
//! assert!(index.search(&Query::parse("water")).is_empty());
//! ```
//!
//! # The five things it has to do
//!
//! | | Where |
//! |---|---|
//! | Tokenise, fold, invert; update one item without touching the rest | [`index`], [`token`] |
//! | Substring, prefix, phrase and typo-tolerant matching | [`query`], [`search`](self#modules) |
//! | Rank exact over prefix over fuzzy, earliest first, and never reshuffle | [`rank`] |
//! | Filter by board, kind, colour and tag | [`query::Filters`] |
//! | Return board id, item id and a snippet with the match marked | [`Hit`], [`snippet`] |
//!
//! # Two decisions worth knowing before reading anything else
//!
//! **The index stores the text.** A document record holds the item's own text, not
//! a reference back to the board that owns it. It costs about a megabyte for the
//! whole 100-board corpus and it is what makes cross-board search viable: answering
//! a query, ranking it and drawing its snippets opens no board files at all. Only
//! the board the user actually jumps into is loaded.
//!
//! **Kinds and colours are indexed as facets, not as text.** "All yellow stickies"
//! and "every frame" are how people search a *visual* board, and neither is a text
//! query. A bare word naming a kind or a colour matches by that attribute as well
//! as by text, with text matches ranked first — see [`query`] for the resolution
//! rules and [`rank`] for the ordering.
//!
//! # Layout
//!
//! - [`source`] — the input trait, and the `Board` → index mapping.
//! - [`token`] — the tokenisation rule the indexer and the parser must share.
//! - [`index`] — documents, postings, facets, and incremental update.
//! - [`query`] — the query language and the matching modes.
//! - [`rank`] — the ordering, and why it is total.
//! - [`snippet`] — the windowed, span-marked result text.
//! - [`colour`] — colour, and the names people type for it.
//! - [`error`] — the failures, all of which concern a file.
//!
//! Two modules are private because they are implementation, not interface:
//! `search` holds query execution (its one public knob, [`MAX_EXPANSIONS`], is
//! re-exported here) and `persist` holds the on-disk format ([`FORMAT_VERSION`]).
//! `dictionary` holds the term dictionary and the bounded edit distance.

pub mod colour;
pub mod error;
pub mod index;
pub mod query;
pub mod rank;
pub mod snippet;
pub mod source;
pub mod token;

mod dictionary;
mod persist;
mod search;

pub use colour::{COLOUR_NAMES, Colour, ColourNames, is_colour_name};
pub use error::SearchError;
pub use index::{Index, IndexStats, Update};
pub use persist::FORMAT_VERSION;
pub use query::{
    ColourFilter, DEFAULT_LIMIT, Filters, Matching, Query, STOPWORDS, Term,
};
pub use rank::{Field, Hit, MatchTier, Score};
pub use search::MAX_EXPANSIONS;
pub use snippet::{SNIPPET_BUDGET, Snippet, Span};
pub use source::{Item, SearchItem, depluralise, resolve_kind};
pub use token::{Token, tokenise};
