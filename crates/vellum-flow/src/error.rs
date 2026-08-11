//! The ways a container refuses an edit.
//!
//! Every variant is a **refusal**, and every refusal is total: the container is
//! byte-identical afterwards. That matters more here than the message does, because
//! these operations are the ones a drag ends in. Half-applying a move — card removed
//! from its column, insert then rejected — would lose the card, and the tests assert
//! the whole-container serialisation is unchanged after each kind of failure rather
//! than only that an error came back.
//!
//! There is no "would exceed the WIP limit" variant, and that is deliberate: a WIP
//! limit is a signal, not a rule. Miro colours the column and lets you carry on,
//! because the point of the limit is to be *visibly* broken by a team that is taking
//! on too much. See [`crate::kanban::Wip`].

use thiserror::Error;

use crate::id::{ActivityId, BarId, CardId, ColumnId, ReleaseId, StoryId};

/// An edit a container would not perform. The container is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FlowError {
    /// The id names no live column — it was never in this kanban, or it has been
    /// removed and the caller is holding a stale handle. Ids are never reused (see
    /// [`crate::id`]), so this can never silently hit a different column.
    #[error("no column {0} in this kanban")]
    NoSuchColumn(ColumnId),

    #[error("no card {0} in this kanban")]
    NoSuchCard(CardId),

    #[error("no activity {0} in this story map")]
    NoSuchActivity(ActivityId),

    #[error("no release {0} in this story map")]
    NoSuchRelease(ReleaseId),

    #[error("no story {0} in this story map")]
    NoSuchStory(StoryId),

    #[error("no bar {0} in this timeline")]
    NoSuchBar(BarId),

    /// A bar cannot depend on itself. Called out separately from
    /// [`FlowError::DependencyCycle`] because it is a different mistake — usually a
    /// drag that started and ended on the same bar — and because a one-element cycle
    /// path reads as noise.
    #[error("{0} cannot depend on itself")]
    SelfDependency(BarId),

    /// The dependency would close a loop. The path is the existing chain that
    /// already leads from the proposed successor back to the proposed predecessor,
    /// so the message names every bar involved and the UI can highlight them.
    ///
    /// Detected **before** the edge is added, which is what stops every later
    /// traversal — topological order, violation checks, a UI walking the chain —
    /// from having to defend itself against a graph that cannot be walked.
    #[error("that dependency would close a cycle: {}", crate::error::path(.path))]
    DependencyCycle { path: Vec<BarId> },
}

/// Renders a cycle as `bar1 → bar4 → bar7 → bar1`, repeating the first bar at the
/// end so the loop reads as one.
pub(crate) fn path(bars: &[BarId]) -> String {
    let mut text = bars.iter().map(BarId::to_string).collect::<Vec<_>>().join(" → ");
    if let Some(first) = bars.first() {
        text.push_str(" → ");
        text.push_str(&first.to_string());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_child_names_itself() {
        let err = FlowError::NoSuchCard(CardId::from_raw(4));
        assert_eq!(err.to_string(), "no card card4 in this kanban");
    }

    #[test]
    fn a_cycle_reads_as_a_loop() {
        let err = FlowError::DependencyCycle {
            path: vec![BarId::from_raw(1), BarId::from_raw(4), BarId::from_raw(7)],
        };
        let expected = "that dependency would close a cycle: bar1 → bar4 → bar7 → bar1";
        assert_eq!(err.to_string(), expected);
    }
}
