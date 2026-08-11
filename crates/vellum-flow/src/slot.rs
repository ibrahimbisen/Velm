//! Where a child goes when it is inserted or dropped.
//!
//! Shared by the kanban and the story map because they mean exactly the same thing
//! by it, and because the one ambiguous case — dropping a card back into the list it
//! is already in — has to be resolved the same way in both or a drag preview and the
//! drop it commits will disagree by one.
//!
//! **A [`Slot::Index`] is the position the child ends up at**, counted in the
//! destination's order *after* the child has been taken out of wherever it was. So
//! moving `A` of `[A, B, C]` to index 1 gives `[B, A, C]`: `A` is lifted first,
//! leaving `[B, C]`, and lands at position 1. Counting in the list as displayed
//! would make the same drag mean two different things depending on which side the
//! card was dragged from.

use serde::{Deserialize, Serialize};

use crate::rank::Rank;

/// A position in a list of children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Slot {
    /// Before every existing child.
    Top,
    /// After every existing child. What a plain "add a card" means.
    Bottom,
    /// At this position in the destination's final order, clamped to the list.
    ///
    /// Clamped rather than refused: this number comes from a drop preview computed
    /// against a layout that may be a frame old, and a card landing at the end of a
    /// column is a better answer than a card that vanishes because the list shrank
    /// while the pointer was moving.
    Index(usize),
}

impl Slot {
    /// The position this slot names in a list that will have `len` children in it
    /// once the insertion is done.
    pub fn index_in(self, len: usize) -> usize {
        match self {
            Self::Top => 0,
            Self::Bottom => len,
            Self::Index(index) => index.min(len),
        }
    }
}

/// Implemented by the ordered children of a container, so the rank arithmetic is
/// written once instead of once per widget.
pub(crate) trait Ranked {
    fn rank(&self) -> &Rank;
}

/// The rank a new child needs to land at `index` among `children`, which are already
/// in rank order.
///
/// This is the whole of the insert logic: read the two neighbours, ask for a key
/// between them, write it. Nothing else in the list is read and nothing else is
/// written.
pub(crate) fn rank_at<T: Ranked>(children: &[T], index: usize) -> Rank {
    let before = index.checked_sub(1).and_then(|i| children.get(i)).map(Ranked::rank);
    let after = children.get(index).map(Ranked::rank);
    Rank::between(before, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Child(Rank);
    impl Ranked for Child {
        fn rank(&self) -> &Rank {
            &self.0
        }
    }

    fn children(count: usize) -> Vec<Child> {
        let mut out: Vec<Child> = Vec::new();
        for _ in 0..count {
            let rank = rank_at(&out, out.len());
            out.push(Child(rank));
        }
        out
    }

    #[test]
    fn slots_clamp_rather_than_refusing_a_stale_index() {
        assert_eq!(Slot::Top.index_in(3), 0);
        assert_eq!(Slot::Bottom.index_in(3), 3);
        assert_eq!(Slot::Index(1).index_in(3), 1);
        assert_eq!(Slot::Index(99).index_in(3), 3);
        assert_eq!(Slot::Bottom.index_in(0), 0);
    }

    #[test]
    fn a_rank_at_an_index_falls_between_its_neighbours() {
        let list = children(4);
        assert!(list.windows(2).all(|w| w[0].0 < w[1].0), "the fixture must be ordered");

        let middle = rank_at(&list, 2);
        assert!(list[1].0 < middle && middle < list[2].0);

        let top = rank_at(&list, 0);
        assert!(top < list[0].0);

        let bottom = rank_at(&list, list.len());
        assert!(*list.last().unwrap().rank() < bottom);

        // The empty list is the case every container starts in.
        assert_eq!(rank_at::<Child>(&[], 0), Rank::first());
    }
}
