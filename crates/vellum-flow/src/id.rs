//! Handles to a container's children.
//!
//! Every id is an opaque `u64` that its container hands out and **never reuses**.
//! That single rule is what makes a stale handle safe: a card that has been deleted
//! leaves an id that matches nothing, so an operation naming it fails with
//! [`FlowError::NoSuchCard`](crate::FlowError::NoSuchCard) rather than quietly
//! editing whichever card happened to take its place. A bare index cannot offer
//! that, and it is the failure mode a drag that outlives its card would hit first.
//!
//! Ids are **not interchangeable between containers**. A kanban card id and a story
//! map story id are different types precisely so that passing one where the other
//! belongs is a compile error rather than a lookup that mysteriously misses; they
//! are separate widgets whose children happen to look alike on screen.
//!
//! The numbers are per-container and start at 1, so two kanbans on the same board
//! both have a `card1`. Nothing joins ids across containers, and the container is
//! always in hand when one is used.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! flow_id {
    ($(#[$docs:meta])* $name:ident, $prefix:literal) => {
        $(#[$docs])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(u64);

        impl $name {
            pub(crate) const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// The underlying number. Exposed for the document layer, which has to
            /// store it; nothing in this crate does arithmetic on it.
            pub const fn raw(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $prefix, self.0)
            }
        }
    };
}

flow_id!(
    /// A kanban column.
    ColumnId,
    "col"
);
flow_id!(
    /// A kanban card.
    CardId,
    "card"
);
flow_id!(
    /// A story map activity — one column of the backbone, across the top.
    ActivityId,
    "act"
);
flow_id!(
    /// A story map release — one row down the side.
    ReleaseId,
    "rel"
);
flow_id!(
    /// A story map card. Called a *story* because that is what it is, and because it
    /// keeps it distinct from [`CardId`], which belongs to a different widget.
    StoryId,
    "story"
);
flow_id!(
    /// A timeline bar: one date range on the axis.
    BarId,
    "bar"
);

/// Hands out ids for one container, never repeating one.
///
/// A plain counter rather than a free list: reuse is the whole thing this type
/// exists to prevent, and a `u64` that ticks once per created child outlives any
/// board — a card a second for a million years does not exhaust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct IdSource(u64);

impl IdSource {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_never_reused_so_a_stale_handle_matches_nothing() {
        let mut source = IdSource::default();
        let first = CardId::from_raw(source.next());
        let second = CardId::from_raw(source.next());
        assert_ne!(first, second);
        assert_eq!(first.raw(), 1, "ids start at 1, so 0 is available as a niche");
        // Nothing hands an id back, so re-running `next` cannot return `first`.
        assert_ne!(CardId::from_raw(source.next()), first);
    }

    #[test]
    fn ids_print_with_their_widget_so_a_log_line_says_what_it_is() {
        assert_eq!(ColumnId::from_raw(3).to_string(), "col3");
        assert_eq!(StoryId::from_raw(12).to_string(), "story12");
        assert_eq!(BarId::from_raw(7).to_string(), "bar7");
    }
}
