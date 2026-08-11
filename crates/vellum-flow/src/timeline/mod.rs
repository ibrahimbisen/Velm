//! Timeline / roadmap / Gantt: bars on a date axis, packed into lanes, with
//! dependencies between them.
//!
//! ```
//! use vellum_flow::{DateRange, LinkKind, Timeline};
//!
//! let mut plan = Timeline::new("Renderer");
//! let day = |text: &str| text.parse().unwrap();
//! let sdf = plan.add_bar("SDF shapes", DateRange::new(day("2026-08-01"), day("2026-08-20")));
//! let text = plan.add_bar("Glyph atlas", DateRange::new(day("2026-08-10"), day("2026-09-05")));
//! let ship = plan.add_bar("Ship", DateRange::new(day("2026-09-06"), day("2026-09-12")));
//!
//! // Two bars that overlap need two lanes; the third starts after both and reuses one.
//! assert_eq!(plan.lanes().lane_count, 2);
//!
//! plan.add_dependency(sdf, ship, LinkKind::FinishToStart).unwrap();
//! plan.add_dependency(text, ship, LinkKind::FinishToStart).unwrap();
//!
//! // A dependency that would close a loop is refused, and the plan is untouched.
//! assert!(plan.add_dependency(ship, sdf, LinkKind::FinishToStart).is_err());
//! assert_eq!(plan.dependencies().len(), 2);
//! assert_eq!(plan.topological_order().unwrap().len(), 3);
//! ```
//!
//! # Lanes are derived, never stored
//!
//! A bar has dates; it does not have a row. Which lane it lands in comes out of
//! [`lanes::pack`], which is optimal for interval graphs — see that module for why a
//! greedy sweep is provably the best possible here. Storing lanes instead would mean
//! every date edit could silently leave a gap or an overlap that nothing recomputes,
//! and two bars drawn on top of each other is the one thing a roadmap must never do.
//!
//! # A cycle is refused, not tolerated
//!
//! [`Timeline::add_dependency`] checks whether the successor can already reach the
//! predecessor **before** it adds the edge, and refuses with the offending chain if
//! it can. That is what lets every traversal in this crate — topological order,
//! violation checks, a UI walking a chain of blockers — be written without a visited
//! set to defend against a graph that cannot be walked. A loaded document is checked
//! too, by [`Timeline::dependency_cycle`], because a file can contain anything.

pub mod axis;
pub mod lanes;
mod layout;

pub use axis::{Tick, TickScale, TimeAxis};
pub use lanes::Packing;
pub use layout::{
    BarDrag, BarEdge, BarLayout, LaneLayout, LinkLayout, TimelineDrop, TimelineLayout,
    TimelineTarget,
};

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::date::{Date, DateRange};
use crate::error::FlowError;
use crate::id::{BarId, IdSource};
use crate::metrics::TimelineMetrics;

/// A time axis with bars on it. The container itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    title: String,
    bars: Vec<Bar>,
    dependencies: Vec<Dependency>,
    span: AxisSpan,
    policy: LanePolicy,
    metrics: TimelineMetrics,
    bar_ids: IdSource,
}

/// One bar: a label and the days it covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    id: BarId,
    label: String,
    range: DateRange,
}

/// One arrow between two bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// The predecessor.
    pub from: BarId,
    /// The successor, the one that is constrained.
    pub to: BarId,
    pub kind: LinkKind,
}

/// Which ends of the two bars a dependency relates.
///
/// The four are the standard set, and they are all here because a roadmap that only
/// has finish-to-start cannot express "these two start together", which is half of
/// what dependencies get drawn for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkKind {
    /// The successor may not start until the predecessor has finished. The default,
    /// and what an unlabelled arrow on a Gantt chart means.
    FinishToStart,
    /// The successor may not start before the predecessor starts.
    StartToStart,
    /// The successor may not finish before the predecessor finishes.
    FinishToFinish,
    /// The successor may not finish before the predecessor starts. Rare, and here
    /// for completeness rather than because anyone reaches for it.
    StartToFinish,
}

/// What the axis covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AxisSpan {
    /// The bars' own extent, with `pad_days` of margin at each end. The default,
    /// because a roadmap is read as a shape and an axis that fits its content is the
    /// one that shows that shape.
    Auto { pad_days: i32 },
    /// A span the caller decides. Scrolling a long plan then feels like scrolling
    /// rather than zooming, and a drag cannot change the scale under itself.
    Fixed(DateRange),
}

impl Default for AxisSpan {
    fn default() -> Self {
        Self::Auto { pad_days: 3 }
    }
}

/// How bars are assigned to rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LanePolicy {
    /// As few lanes as possible — the roadmap reading, where a lane is just space.
    #[default]
    Packed,
    /// One lane per bar, in model order — the Gantt reading, where a row is a task
    /// and lining rows up with a list beside them matters more than compactness.
    OnePerBar,
}

/// A bar that has been deleted, with the arrows that went with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemovedBar {
    pub bar: Bar,
    /// Every dependency that named it, in either direction. Removed with it, because
    /// an arrow to a bar that no longer exists is not a thing that can be drawn.
    pub dependencies: Vec<Dependency>,
}

/// A dependency whose bars' dates do not satisfy it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub dependency: Dependency,
    /// How many days short the successor is. Always negative — zero or more is not a
    /// violation.
    pub slack_days: i32,
}

impl Bar {
    pub fn id(&self) -> BarId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn range(&self) -> DateRange {
        self.range
    }
}

impl LinkKind {
    /// Days of headroom this dependency has, given the two ranges. Negative means it
    /// is violated by exactly that many days.
    ///
    /// The four definitions, written once so the layout, the checker and the tests
    /// cannot drift apart:
    ///
    /// | Kind | Satisfied when |
    /// |---|---|
    /// | Finish → start | the successor starts after the predecessor's last day |
    /// | Start → start | the successor starts no earlier than the predecessor |
    /// | Finish → finish | the successor ends no earlier than the predecessor |
    /// | Start → finish | the successor ends no earlier than the predecessor starts |
    pub fn slack(self, predecessor: DateRange, successor: DateRange) -> i32 {
        match self {
            Self::FinishToStart => successor.start().days_since(predecessor.end_exclusive()),
            Self::StartToStart => successor.start().days_since(predecessor.start()),
            Self::FinishToFinish => successor.end().days_since(predecessor.end()),
            Self::StartToFinish => successor.end().days_since(predecessor.start()),
        }
    }
}

impl Timeline {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            bars: Vec::new(),
            dependencies: Vec::new(),
            span: AxisSpan::default(),
            policy: LanePolicy::default(),
            metrics: TimelineMetrics::default(),
            bar_ids: IdSource::default(),
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn metrics(&self) -> TimelineMetrics {
        self.metrics
    }

    pub fn set_metrics(&mut self, metrics: TimelineMetrics) {
        self.metrics = metrics;
    }

    pub fn policy(&self) -> LanePolicy {
        self.policy
    }

    pub fn set_policy(&mut self, policy: LanePolicy) {
        self.policy = policy;
    }

    pub fn axis_span(&self) -> AxisSpan {
        self.span
    }

    pub fn set_axis_span(&mut self, span: AxisSpan) {
        self.span = span;
    }

    /// The dates the axis actually covers.
    ///
    /// An empty timeline on an automatic axis has nothing to fit, so it reports a
    /// four-week window at the epoch. That is a placeholder for an axis with nothing
    /// on it, not a claim about today: this crate has no clock, deliberately — see
    /// [`crate::date`].
    pub fn span(&self) -> DateRange {
        match self.span {
            AxisSpan::Fixed(range) => range,
            AxisSpan::Auto { pad_days } => {
                let mut ranges = self.bars.iter().map(Bar::range);
                match ranges.next() {
                    Some(first) => ranges.fold(first, DateRange::union).padded(pad_days.max(0)),
                    None => DateRange::days_from(Date::EPOCH, 28),
                }
            }
        }
    }

    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    pub fn bar(&self, id: BarId) -> Option<&Bar> {
        self.bars.iter().find(|bar| bar.id == id)
    }

    pub fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }

    // ----- bars -----------------------------------------------------------

    pub fn add_bar(&mut self, label: impl Into<String>, range: DateRange) -> BarId {
        let id = BarId::from_raw(self.bar_ids.next());
        self.bars.push(Bar { id, label: label.into(), range });
        id
    }

    /// Removes a bar and every dependency that named it.
    pub fn remove_bar(&mut self, id: BarId) -> Result<RemovedBar, FlowError> {
        let index = self.bars.iter().position(|bar| bar.id == id).ok_or(FlowError::NoSuchBar(id))?;
        let bar = self.bars.remove(index);
        let mut dependencies = Vec::new();
        self.dependencies.retain(|link| {
            if link.from == id || link.to == id {
                dependencies.push(*link);
                false
            } else {
                true
            }
        });
        Ok(RemovedBar { bar, dependencies })
    }

    /// Moves or resizes a bar.
    ///
    /// Dependencies are **not** enforced: the dates go where the user put them, and
    /// [`Timeline::violations`] reports the arrows that no longer hold. Rescheduling
    /// someone else's work because a link says so is a decision, not a layout, and a
    /// container widget is the wrong place to make it.
    pub fn set_range(&mut self, id: BarId, range: DateRange) -> Result<(), FlowError> {
        self.bar_mut(id)?.range = range;
        Ok(())
    }

    pub fn set_label(&mut self, id: BarId, label: impl Into<String>) -> Result<(), FlowError> {
        self.bar_mut(id)?.label = label.into();
        Ok(())
    }

    // ----- dependencies ---------------------------------------------------

    /// Adds an arrow from `from` to `to`, or updates its kind if one is already
    /// there.
    ///
    /// Refused, with the whole offending chain, if it would close a cycle. The
    /// timeline is unchanged when it is.
    pub fn add_dependency(&mut self, from: BarId, to: BarId, kind: LinkKind) -> Result<(), FlowError> {
        if self.bar(from).is_none() {
            return Err(FlowError::NoSuchBar(from));
        }
        if self.bar(to).is_none() {
            return Err(FlowError::NoSuchBar(to));
        }
        if from == to {
            return Err(FlowError::SelfDependency(from));
        }
        // The edge closes a cycle exactly when the successor can already reach the
        // predecessor. Checking that way means the graph is acyclic at every instant,
        // so this search itself can never run forever.
        if let Some(chain) = self.route(to, from) {
            let mut path = vec![from];
            path.extend(chain.iter().take(chain.len() - 1));
            return Err(FlowError::DependencyCycle { path });
        }
        match self.dependencies.iter_mut().find(|link| link.from == from && link.to == to) {
            Some(existing) => existing.kind = kind,
            None => self.dependencies.push(Dependency { from, to, kind }),
        }
        Ok(())
    }

    /// Removes an arrow. Reports whether there was one.
    pub fn remove_dependency(&mut self, from: BarId, to: BarId) -> bool {
        let before = self.dependencies.len();
        self.dependencies.retain(|link| !(link.from == from && link.to == to));
        self.dependencies.len() != before
    }

    /// Every dependency naming this bar, in either direction.
    pub fn dependencies_touching(&self, bar: BarId) -> Vec<Dependency> {
        self.dependencies.iter().copied().filter(|link| link.from == bar || link.to == bar).collect()
    }

    /// A cycle in the dependency graph, if there is one, as the bars around the loop.
    ///
    /// Should always be `None` for a timeline this crate built — [`add_dependency`]
    /// makes one impossible. It exists for graphs that arrive from outside: a saved
    /// document, an import, a hand-edited file. Iterative depth-first search with
    /// three-colour marking, so a graph that is one huge chain does not overflow the
    /// stack and a graph full of loops still terminates.
    ///
    /// [`add_dependency`]: Timeline::add_dependency
    pub fn dependency_cycle(&self) -> Option<Vec<BarId>> {
        const UNVISITED: u8 = 0;
        const IN_PROGRESS: u8 = 1;
        const DONE: u8 = 2;

        let successors = self.successors();
        let mut state: HashMap<BarId, u8> = HashMap::with_capacity(self.bars.len());

        for root in &self.bars {
            if state.get(&root.id).copied().unwrap_or(UNVISITED) != UNVISITED {
                continue;
            }
            state.insert(root.id, IN_PROGRESS);
            let mut stack: Vec<(BarId, usize)> = vec![(root.id, 0)];
            while let Some(&(node, edge)) = stack.last() {
                let edges = successors.get(&node).map_or(&[][..], Vec::as_slice);
                let Some(&next) = edges.get(edge) else {
                    state.insert(node, DONE);
                    stack.pop();
                    continue;
                };
                stack.last_mut().expect("the stack is not empty inside this loop").1 += 1;
                match state.get(&next).copied().unwrap_or(UNVISITED) {
                    UNVISITED => {
                        state.insert(next, IN_PROGRESS);
                        stack.push((next, 0));
                    }
                    IN_PROGRESS => {
                        // `next` is an ancestor on the current path, so the loop is
                        // everything from it to the top of the stack.
                        let start = stack
                            .iter()
                            .position(|(bar, _)| *bar == next)
                            .expect("an in-progress node is on the stack");
                        return Some(stack[start..].iter().map(|(bar, _)| *bar).collect());
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// The bars in an order where every predecessor comes before its successors.
    ///
    /// Kahn's algorithm, taking bars in model order so the result is stable. Fails
    /// with the offending cycle rather than looping or returning a partial order.
    pub fn topological_order(&self) -> Result<Vec<BarId>, FlowError> {
        let successors = self.successors();
        let mut indegree: HashMap<BarId, usize> =
            self.bars.iter().map(|bar| (bar.id, 0usize)).collect();
        for link in &self.dependencies {
            if let Some(count) = indegree.get_mut(&link.to) {
                *count += 1;
            }
        }

        let mut ready: Vec<BarId> = self
            .bars
            .iter()
            .filter(|bar| indegree[&bar.id] == 0)
            .map(Bar::id)
            .collect();
        let mut ordered = Vec::with_capacity(self.bars.len());
        while let Some(bar) = ready.pop() {
            ordered.push(bar);
            for &next in successors.get(&bar).map_or(&[][..], Vec::as_slice) {
                if let Some(count) = indegree.get_mut(&next) {
                    *count -= 1;
                    if *count == 0 {
                        ready.push(next);
                    }
                }
            }
        }

        if ordered.len() == self.bars.len() {
            Ok(ordered)
        } else {
            Err(FlowError::DependencyCycle { path: self.dependency_cycle().unwrap_or_default() })
        }
    }

    /// Every dependency the dates no longer satisfy, with how many days short it is.
    pub fn violations(&self) -> Vec<Violation> {
        self.dependencies
            .iter()
            .filter_map(|link| {
                let from = self.bar(link.from)?.range;
                let to = self.bar(link.to)?.range;
                let slack_days = link.kind.slack(from, to);
                (slack_days < 0).then_some(Violation { dependency: *link, slack_days })
            })
            .collect()
    }

    // ----- lanes ----------------------------------------------------------

    /// Which lane each bar sits in, parallel to [`Timeline::bars`].
    pub fn lanes(&self) -> Packing {
        match self.policy {
            LanePolicy::Packed => {
                let ranges: Vec<DateRange> = self.bars.iter().map(Bar::range).collect();
                lanes::pack(&ranges)
            }
            LanePolicy::OnePerBar => Packing {
                lanes: (0..self.bars.len()).collect(),
                lane_count: self.bars.len(),
            },
        }
    }

    // ----- internals ------------------------------------------------------

    fn bar_mut(&mut self, id: BarId) -> Result<&mut Bar, FlowError> {
        self.bars.iter_mut().find(|bar| bar.id == id).ok_or(FlowError::NoSuchBar(id))
    }

    /// Adjacency, built once per traversal rather than rescanned per node.
    fn successors(&self) -> HashMap<BarId, Vec<BarId>> {
        let mut map: HashMap<BarId, Vec<BarId>> = HashMap::with_capacity(self.bars.len());
        for link in &self.dependencies {
            map.entry(link.from).or_default().push(link.to);
        }
        map
    }

    /// A path from `start` to `goal` along the arrows, or `None`. Breadth-first, so
    /// the chain reported in a cycle error is the shortest one.
    fn route(&self, start: BarId, goal: BarId) -> Option<Vec<BarId>> {
        let successors = self.successors();
        let mut came_from: HashMap<BarId, BarId> = HashMap::new();
        let mut queue = VecDeque::from([start]);
        let mut seen: HashSet<BarId> = HashSet::from([start]);

        while let Some(node) = queue.pop_front() {
            if node == goal {
                let mut path = vec![node];
                let mut current = node;
                while let Some(&previous) = came_from.get(&current) {
                    path.push(previous);
                    current = previous;
                }
                path.reverse();
                return Some(path);
            }
            for &next in successors.get(&node).map_or(&[][..], Vec::as_slice) {
                if seen.insert(next) {
                    came_from.insert(next, node);
                    queue.push_back(next);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> Date {
        text.parse().unwrap()
    }

    fn range(start: &str, end: &str) -> DateRange {
        DateRange::new(day(start), day(end))
    }

    /// Four bars: two overlapping, one after them, one much later.
    fn plan() -> (Timeline, Vec<BarId>) {
        let mut plan = Timeline::new("Renderer");
        let bars = vec![
            plan.add_bar("SDF shapes", range("2026-08-01", "2026-08-20")),
            plan.add_bar("Glyph atlas", range("2026-08-10", "2026-09-05")),
            plan.add_bar("Ship", range("2026-09-06", "2026-09-12")),
            plan.add_bar("Retro", range("2026-10-01", "2026-10-02")),
        ];
        (plan, bars)
    }

    #[test]
    fn lanes_are_packed_from_the_dates_alone() {
        let (plan, bars) = plan();
        let packing = plan.lanes();
        assert_eq!(packing.lane_count, 2);
        assert_ne!(packing.lanes[0], packing.lanes[1], "the two overlapping bars split");
        assert_eq!(packing.lanes[2], 0, "the third starts after both and reuses lane 0");

        // Moving a bar re-packs it; nothing stores a lane.
        let mut plan = plan;
        plan.set_range(bars[2], range("2026-08-15", "2026-08-25")).unwrap();
        assert_eq!(plan.lanes().lane_count, 3);
    }

    #[test]
    fn one_lane_per_bar_is_available_for_the_gantt_reading() {
        let (mut plan, _) = plan();
        plan.set_policy(LanePolicy::OnePerBar);
        let packing = plan.lanes();
        assert_eq!(packing.lanes, vec![0, 1, 2, 3]);
        assert_eq!(packing.lane_count, 4);
    }

    /// The required case: a cycle must be rejected, and rejecting it must not hang.
    #[test]
    fn a_dependency_cycle_is_refused_and_the_timeline_is_untouched() {
        let (mut plan, bars) = plan();
        plan.add_dependency(bars[0], bars[1], LinkKind::FinishToStart).unwrap();
        plan.add_dependency(bars[1], bars[2], LinkKind::FinishToStart).unwrap();
        let snapshot = serde_json::to_string(&plan).unwrap();

        let err = plan.add_dependency(bars[2], bars[0], LinkKind::FinishToStart).unwrap_err();
        assert_eq!(
            err,
            FlowError::DependencyCycle { path: vec![bars[2], bars[0], bars[1]] },
            "the chain that already leads back, starting from the proposed predecessor"
        );
        assert!(err.to_string().contains(&format!("{} → {}", bars[2], bars[0])), "{err}");
        assert_eq!(serde_json::to_string(&plan).unwrap(), snapshot, "nothing was written");

        // The graph is still walkable, which is the point of refusing.
        assert_eq!(plan.dependency_cycle(), None);
        assert_eq!(plan.topological_order().unwrap(), vec![bars[3], bars[0], bars[1], bars[2]]);
    }

    #[test]
    fn a_bar_cannot_depend_on_itself() {
        let (mut plan, bars) = plan();
        assert_eq!(
            plan.add_dependency(bars[0], bars[0], LinkKind::FinishToStart),
            Err(FlowError::SelfDependency(bars[0]))
        );
        assert!(plan.dependencies().is_empty());
    }

    /// Depth is the thing that breaks a recursive graph walk, and a chain is the
    /// worst case. Ten thousand deep is past what a recursive depth-first search
    /// survives on a test thread's stack, so this is the assertion that the traversal
    /// really is iterative rather than merely written to look it.
    ///
    /// The chain is injected rather than built with `add_dependency`, which would
    /// spend `O(E)` per call rebuilding the adjacency map — correct, and irrelevant
    /// for a user action, but quadratic for ten thousand of them in a row.
    #[test]
    fn a_ten_thousand_deep_chain_is_walked_without_recursion() {
        const DEPTH: u64 = 10_000;
        let mut plan = Timeline::new("long");
        let bars: Vec<BarId> = (0..DEPTH as i32)
            .map(|n| plan.add_bar(format!("b{n}"), DateRange::days_from(Date::from_days(n), 1)))
            .collect();

        let mut document = serde_json::to_value(&plan).unwrap();
        document["dependencies"] = bars
            .windows(2)
            .map(|pair| {
                let (from, to) = (pair[0].raw(), pair[1].raw());
                serde_json::json!({ "from": from, "to": to, "kind": "FinishToStart" })
            })
            .collect();
        let plan: Timeline = serde_json::from_value(document).unwrap();

        assert_eq!(plan.dependencies().len(), DEPTH as usize - 1);
        assert_eq!(plan.dependency_cycle(), None);
        assert_eq!(plan.topological_order().unwrap().len(), DEPTH as usize);

        // And closing the loop from the far end is refused rather than searched
        // forever — one traversal of the whole chain, then a refusal.
        let mut plan = plan;
        assert!(plan.add_dependency(*bars.last().unwrap(), bars[0], LinkKind::FinishToStart).is_err());
    }

    /// A cyclic graph can still arrive from a file. It has to be detected, not
    /// looped over — this builds one by deserialising a document nothing in this
    /// crate would have written.
    #[test]
    fn a_cycle_loaded_from_a_document_is_detected_rather_than_walked_forever() {
        let (plan, bars) = plan();
        let mut corrupt = serde_json::to_value(&plan).unwrap();
        corrupt["dependencies"] = serde_json::json!([
            { "from": bars[0].raw(), "to": bars[1].raw(), "kind": "FinishToStart" },
            { "from": bars[1].raw(), "to": bars[2].raw(), "kind": "FinishToStart" },
            { "from": bars[2].raw(), "to": bars[0].raw(), "kind": "FinishToStart" },
        ]);
        let loaded: Timeline = serde_json::from_value(corrupt).unwrap();

        let cycle = loaded.dependency_cycle().expect("the loop must be found");
        assert_eq!(cycle.len(), 3);
        assert!(cycle.contains(&bars[0]) && cycle.contains(&bars[1]) && cycle.contains(&bars[2]));
        assert!(matches!(loaded.topological_order(), Err(FlowError::DependencyCycle { .. })));
        // And it still lays out: a picture of a broken plan is better than no picture.
        assert_eq!(loaded.lanes().lane_count, 2);
    }

    #[test]
    fn adding_the_same_arrow_twice_updates_it_rather_than_duplicating_it() {
        let (mut plan, bars) = plan();
        plan.add_dependency(bars[0], bars[1], LinkKind::FinishToStart).unwrap();
        plan.add_dependency(bars[0], bars[1], LinkKind::StartToStart).unwrap();
        assert_eq!(plan.dependencies().len(), 1);
        assert_eq!(plan.dependencies()[0].kind, LinkKind::StartToStart);
        assert!(plan.remove_dependency(bars[0], bars[1]));
        assert!(!plan.remove_dependency(bars[0], bars[1]));
    }

    #[test]
    fn removing_a_bar_takes_its_arrows_with_it() {
        let (mut plan, bars) = plan();
        plan.add_dependency(bars[0], bars[1], LinkKind::FinishToStart).unwrap();
        plan.add_dependency(bars[1], bars[2], LinkKind::FinishToStart).unwrap();

        let removed = plan.remove_bar(bars[1]).unwrap();
        assert_eq!(removed.bar.label(), "Glyph atlas");
        assert_eq!(removed.dependencies.len(), 2, "both the arrow in and the arrow out");
        assert!(plan.dependencies().is_empty());
        assert_eq!(plan.remove_bar(bars[1]), Err(FlowError::NoSuchBar(bars[1])));
    }

    #[test]
    fn a_dependency_to_a_bar_that_does_not_exist_is_refused() {
        let (mut plan, bars) = plan();
        let ghost = BarId::from_raw(9999);
        assert_eq!(
            plan.add_dependency(ghost, bars[0], LinkKind::FinishToStart),
            Err(FlowError::NoSuchBar(ghost))
        );
        assert_eq!(
            plan.add_dependency(bars[0], ghost, LinkKind::FinishToStart),
            Err(FlowError::NoSuchBar(ghost))
        );
        assert!(plan.dependencies().is_empty());
    }

    #[test]
    fn violations_report_the_days_they_are_short_by() {
        let (mut plan, bars) = plan();
        // Ship starts the day after Glyph atlas ends: satisfied, with no slack.
        plan.add_dependency(bars[1], bars[2], LinkKind::FinishToStart).unwrap();
        assert!(plan.violations().is_empty());
        assert_eq!(LinkKind::FinishToStart.slack(range("2026-08-10", "2026-09-05"), range("2026-09-06", "2026-09-12")), 0);

        // Pull Ship five days earlier and the arrow no longer holds.
        plan.set_range(bars[2], range("2026-09-01", "2026-09-07")).unwrap();
        let violations = plan.violations();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].slack_days, -5);
        assert_eq!(violations[0].dependency.to, bars[2]);

        // The other three kinds, on the same pair.
        let predecessor = range("2026-08-10", "2026-09-05");
        let successor = range("2026-09-01", "2026-09-07");
        assert_eq!(LinkKind::StartToStart.slack(predecessor, successor), 22);
        assert_eq!(LinkKind::FinishToFinish.slack(predecessor, successor), 2);
        assert_eq!(LinkKind::StartToFinish.slack(predecessor, successor), 28);
    }

    #[test]
    fn an_automatic_axis_fits_the_bars_and_an_empty_one_still_has_a_span() {
        let (mut plan, _) = plan();
        let span = plan.span();
        assert_eq!(span.start(), day("2026-07-29"), "three days of margin");
        assert_eq!(span.end(), day("2026-10-05"));

        plan.set_axis_span(AxisSpan::Fixed(range("2026-01-01", "2026-12-31")));
        assert_eq!(plan.span(), range("2026-01-01", "2026-12-31"));

        let empty = Timeline::new("nothing");
        assert_eq!(empty.span().length(), 28);
        assert_eq!(empty.lanes().lane_count, 0);

        // A single bar still produces a span with margin around it.
        let mut one = Timeline::new("one");
        one.add_bar("only", range("2026-05-05", "2026-05-06"));
        assert_eq!(one.span(), range("2026-05-02", "2026-05-09"));
    }

    #[test]
    fn a_timeline_survives_a_save_and_load_unchanged() {
        let (mut plan, bars) = plan();
        plan.add_dependency(bars[0], bars[2], LinkKind::StartToStart).unwrap();
        plan.set_axis_span(AxisSpan::Fixed(range("2026-07-01", "2026-10-31")));
        plan.set_policy(LanePolicy::OnePerBar);

        let json = serde_json::to_string(&plan).unwrap();
        let loaded: Timeline = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, plan);
        assert_eq!(serde_json::to_string(&loaded).unwrap(), json);
    }
}
