//! Territory and spawn caps — the two limits an orchestrator cannot talk its way out of.
//!
//! An orchestrator manages other agents rather than doing the work itself. It owns a
//! rectangle of the board ([`Territory`]) and a hard number of simultaneous sub-agents
//! ([`AgentModel::effective_spawn_cap`]), and `docs/07-agent-canvas.md` §9 states the rule
//! this module exists to keep:
//!
//! > **Cap and territory are both enforced in Velm, never in the prompt — a limit that
//! > lives only in an instruction is not a limit.**
//!
//! That is the whole justification for the module. An orchestrator *told in words* to spawn
//! at most five agents inside a rectangle is an orchestrator that will eventually spawn a
//! sixth somewhere else, and the failure arrives as an API bill and a board nobody can read.
//! So the spawn request comes back through [`may_spawn`] or [`plan_spawn`], and a `Refused`
//! answer is the only thing a model can do about it.
//!
//! # A refusal is reported, never swallowed
//!
//! [`Refusal::into_event`] turns a refusal into a [`TranscriptEvent::Error`], which the node
//! shows in **both** display modes, so the orchestrator reads it and can adapt — *"I am at
//! my cap, so I will do this one myself"*. A silently dropped spawn is the worst of the three
//! possible outcomes: the orchestrator believes it delegated the work and nothing is doing it.
//! [`Refusal::message`] is therefore written for a model to act on, naming the remedy, rather
//! than for a log to record.
//!
//! The join with [`crate::summary`] is deliberate: the away-mode digest ranks
//! `TranscriptEvent::Error` as something that needs the user, so a run where an orchestrator
//! kept hitting its cap is visible when they come back rather than buried in a transcript.
//!
//! # What this module does not do
//!
//! It never mutates a document. [`descendants_of`] answers *which nodes belong to this
//! orchestrator*; deleting them is `vellum-app`'s, because only that crate may touch Loro.
//! And it never reads a clock or an RNG — [`place_in`] is a deterministic packing, so the
//! same board asked twice gets the same answer and a `--demo` fixture can assert a position.

use crate::{AgentModel, RoleKind, Territory, TranscriptEvent};

/// The clear space left between two sub-agents, in world units.
///
/// A constant rather than a parameter because it is a *look*, not a policy: the packing is
/// the same gap the tool palette leaves between two placed stickies, and a caller that could
/// choose it would be a caller that could choose zero and produce a wall of touching boxes.
pub const SPAWN_GUTTER: f64 = 24.0;

/// Slack in the cell count, in cells.
///
/// `(span + gutter) / (size + gutter)` is exactly the number of columns that fit, and the
/// only thing that can go wrong is a last unit in the last place turning 3.0 into
/// 2.9999999996 and losing a column. This rescues that and cannot invent one: no genuine
/// 2.6 becomes a 3. The candidate is checked against [`Territory::contains_box`] anyway, so
/// a column this lets through that does not truly fit is skipped rather than used.
const COUNT_SLACK: f64 = 1e-6;

/// A sanity clamp on the packing grid, per axis.
///
/// A territory big enough to hold a thousand agents across is not a case worth scanning
/// cell by cell, and refusing there is honest. The scan itself terminates long before this
/// in every real case — a cell is only rejected when a sibling overlaps it, and there are at
/// most [`AgentModel::effective_spawn_cap`] siblings.
const MAX_CELLS_PER_AXIS: usize = 1_000;

/// Boxes are compared with this much slack, in world units, so two rectangles that merely
/// touch are not called an overlap. A shared edge is what a packing *produces*.
const TOUCH: f64 = 1e-6;

/// A rectangle on the board: a centre and an extent, in world units.
///
/// Centre-and-extent rather than corners because that is what `vellum_doc::Placement` and
/// [`Territory`] both are, so a proposal, an item's box and a region are compared with no
/// coordinate conversion in between — the conversion being exactly where an off-by-half-a-box
/// error lives.
///
/// `PartialEq` and not `Eq`: these are floats, and a packing that claimed to be hashable
/// would be one `NaN` away from being wrong about it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NodeBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl NodeBox {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    pub fn left(&self) -> f64 {
        self.x - self.width / 2.0
    }

    pub fn top(&self) -> f64 {
        self.y - self.height / 2.0
    }

    pub fn right(&self) -> f64 {
        self.x + self.width / 2.0
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height / 2.0
    }

    /// Whether two boxes overlap by more than a shared edge.
    ///
    /// Strictly, with [`TOUCH`] of slack: the packing lays boxes out a gutter apart, and a
    /// test that called two abutting rectangles an overlap would refuse its own output the
    /// moment the gutter was set to zero.
    pub fn overlaps(&self, other: &Self) -> bool {
        (self.x - other.x).abs() < (self.width + other.width) / 2.0 - TOUCH
            && (self.y - other.y).abs() < (self.height + other.height) / 2.0 - TOUCH
    }
}

/// Why a spawn was refused.
///
/// Five distinct reasons rather than one string, because the orchestrator's *next* move
/// differs for each: a cap says wait, a full region says ask for more room, no territory says
/// ask the user for one, and a wrong position says aim again. Collapsing them would produce
/// the one message a model cannot act on — *"the spawn was refused"*.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// This role may not spawn at all. A worker: see [`RoleKind::may_spawn`].
    RoleMayNotSpawn { role: RoleKind },
    /// The orchestrator has no region to spawn into, or the one it has is empty.
    ///
    /// **Refused rather than defaulted to the whole board.** An orchestrator with no
    /// territory that could spawn anywhere is an orchestrator with no territory *limit*,
    /// which is the failure this module exists to prevent, arrived at by omission.
    NoTerritory,
    /// The cap would be exceeded. `live` is what the caller counted as running.
    CapReached { cap: u32, live: u32 },
    /// The proposed box is not entirely inside the territory.
    OutsideTerritory { territory: Territory, proposed: NodeBox },
    /// Every place in the region is taken — a *different* problem from the cap, and the
    /// distinction matters: waiting will not help, and the remedy is a bigger region or a
    /// smaller agent.
    RegionFull { capacity: usize, siblings: usize },
}

impl Refusal {
    /// What the orchestrator is told, written for a model to read and act on.
    ///
    /// Every one of these names the remedy in its second sentence. A refusal that only says
    /// no is a refusal an agent answers by trying the identical call again.
    pub fn message(&self) -> String {
        match self {
            // Phrased around the label rather than around an article, because "a Agent"
            // and "an Orchestrator" cannot both be produced by one format string, and a
            // refusal with a grammatical error in it reads as a bug in the harness rather
            // than as a rule.
            Self::RoleMayNotSpawn { role } => format!(
                "Your role ({}) may not spawn sub-agents. Do this task yourself, or ask \
                 the user to make an orchestrator for it.",
                role.label()
            ),
            Self::NoTerritory => "You have no territory on this board, and an orchestrator \
                 may only spawn inside its own region. Do this task yourself, or ask the \
                 user to draw a territory for you."
                .into(),
            Self::CapReached { cap, live } => format!(
                "You already have {live} sub-agents running and your limit is {cap}. Wait \
                 for one of them to finish before spawning another, or do this task yourself."
            ),
            Self::OutsideTerritory { territory, proposed } => format!(
                "That position is outside your territory. Your region runs from \
                 ({:.0}, {:.0}) to ({:.0}, {:.0}); the agent you asked for would sit from \
                 ({:.0}, {:.0}) to ({:.0}, {:.0}). Place it entirely inside your region.",
                territory.x - territory.width / 2.0,
                territory.y - territory.height / 2.0,
                territory.x + territory.width / 2.0,
                territory.y + territory.height / 2.0,
                proposed.left(),
                proposed.top(),
                proposed.right(),
                proposed.bottom(),
            ),
            Self::RegionFull { capacity: 0, .. } => "An agent that size does not fit in your \
                 territory at all. Ask the user for a larger region, or do this task yourself."
                .into(),
            Self::RegionFull { capacity, siblings } => format!(
                "Your territory has room for {capacity} agents of that size and all of those \
                 places are taken ({siblings} already there). Waiting will not free one — ask \
                 the user for a larger region, or do this task yourself."
            ),
        }
    }

    /// The refusal as it reaches the transcript.
    ///
    /// [`TranscriptEvent::Error`] because it is the only both-modes variant that carries free
    /// text, and because Clean mode is exactly where a silently refused spawn would look like
    /// an orchestrator that simply never delegated anything. `crate::summary` ranks an
    /// `Error` as needing the user, which is the behaviour wanted here: a run spent bouncing
    /// off a cap is something to see on coming back.
    pub fn into_event(self) -> TranscriptEvent {
        TranscriptEvent::Error { message: self.message() }
    }
}

impl From<Refusal> for crate::AgentError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal.message())
    }
}

/// One agent node, as the accounting queries see it.
///
/// A borrowed view of two fields rather than the whole [`AgentModel`]: these queries run over
/// every agent node on a board, and `spawned_by` is the only thing they read. Naming that in
/// the type is what stops the next reader wondering whether the cap secretly depends on the
/// provider or the schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentNode<'a> {
    /// The board item's id, in its string form — this crate holds identifiers, never
    /// `vellum-doc` types.
    pub id: &'a str,
    /// The orchestrator that spawned it, or `None` for one the user placed.
    pub spawned_by: Option<&'a str>,
}

impl<'a> AgentNode<'a> {
    pub fn new(id: &'a str, model: &'a AgentModel) -> Self {
        Self { id, spawned_by: model.spawned_by.as_deref() }
    }

    /// For a caller that has the parentage without a decoded model — the IPC boundary, where
    /// a spawn request names its parent directly.
    pub const fn raw(id: &'a str, spawned_by: Option<&'a str>) -> Self {
        Self { id, spawned_by }
    }
}

/// The agents this orchestrator spawned directly, in the order they were given.
///
/// A node that names itself as its own parent is skipped rather than trusted: `spawned_by` is
/// a string in a board file, and a board file is data this crate does not get to assume is
/// well formed.
pub fn children_of<'a>(parent: &str, nodes: &[AgentNode<'a>]) -> Vec<&'a str> {
    nodes
        .iter()
        .filter(|node| node.id != parent && node.spawned_by == Some(parent))
        .map(|node| node.id)
        .collect()
}

/// How many agents this orchestrator spawned.
///
/// **Not the number to check a cap against.** The cap is on *simultaneous* sub-agents, and
/// whether a child is still running is a fact about a live session that only `vellum-app`
/// holds — so [`may_spawn`] takes that count as an argument and this answers a different
/// question. Feeding this into the cap would refuse a spawn because of an agent that finished
/// an hour ago.
pub fn count_children(parent: &str, nodes: &[AgentNode<'_>]) -> u32 {
    children_of(parent, nodes).len() as u32
}

/// Every agent below this orchestrator, transitively — what to clean up when it is deleted.
///
/// Breadth-first in the order the nodes were given, so the answer is deterministic and a test
/// can name it. An orchestrator may spawn an orchestrator, so a single level would leave
/// grandchildren on the board with a parent that no longer exists.
///
/// **The ids only.** Removing them is `vellum-app`'s to do, inside one undo group, through
/// the same `Board::remove` a deleted frame uses — this crate never touches a document.
///
/// Cycle-safe: a board whose `spawned_by` chain loops back on itself is malformed data rather
/// than an impossibility, and the alternative to guarding is a hang on load.
pub fn descendants_of<'a>(parent: &str, nodes: &[AgentNode<'a>]) -> Vec<&'a str> {
    let mut found = children_of(parent, nodes);
    let mut next = 0;
    while next < found.len() {
        let current = found[next];
        next += 1;
        for child in children_of(current, nodes) {
            if child != parent && !found.contains(&child) {
                found.push(child);
            }
        }
    }
    found
}

/// How many agents of a given size a territory holds, as (columns, rows).
fn grid(territory: &Territory, width: f64, height: f64) -> (usize, usize) {
    if territory.is_empty() || !positive(width) || !positive(height) {
        return (0, 0);
    }
    (cells(territory.width, width), cells(territory.height, height))
}

/// A real, usable extent. `is_finite` first, so a `NaN` size — which every comparison
/// answers `false` for, including `<= 0.0` — is rejected rather than divided by.
fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn cells(span: f64, size: f64) -> usize {
    let count = (span + SPAWN_GUTTER) / (size + SPAWN_GUTTER) + COUNT_SLACK;
    if count.is_finite() && count >= 1.0 {
        // `f64 as usize` saturates rather than wrapping, and the clamp bounds it anyway.
        (count as usize).min(MAX_CELLS_PER_AXIS)
    } else {
        0
    }
}

/// The most sub-agents of this size that could ever stand in this region at once.
///
/// Public because the inspector says it — *"room for 6"* beside the cap is what makes a cap
/// of 20 in a region that holds 6 obviously wrong before anything is spawned.
pub fn capacity(territory: &Territory, width: f64, height: f64) -> usize {
    let (columns, rows) = grid(territory, width, height);
    columns * rows
}

/// Where a new sub-agent of this size goes, given the boxes already in the region.
///
/// Rows within the territory, left to right and top to bottom, first free cell wins. Three
/// properties are load-bearing and each rules out a tidier alternative:
///
/// - **Deterministic.** No clock, no RNG — this crate has neither and must not want one. The
///   same territory and the same siblings give the same answer every time, which is what lets
///   a fixture assert a position and what stops two spawns in one frame landing on top of
///   each other by luck.
/// - **First free cell, not "after the last one".** Cells freed by a finished agent that was
///   deleted are reused, so an orchestrator that spawns and reaps all day does not walk its
///   own region and fall out of the bottom of it.
/// - **Siblings are checked by overlap, not by cell.** A box the user dragged in by hand sits
///   on no grid at all, and packing around it is the difference between a proposal and a
///   collision.
///
/// A candidate is verified against [`Territory::contains_box`] before it is offered, so the
/// result is inside the region by construction — [`may_spawn`] does not need to be asked again.
pub fn place_in(
    territory: &Territory,
    siblings: &[NodeBox],
    width: f64,
    height: f64,
) -> Result<NodeBox, Refusal> {
    let (columns, rows) = grid(territory, width, height);
    let left = territory.x - territory.width / 2.0;
    let top = territory.y - territory.height / 2.0;

    for row in 0..rows {
        for column in 0..columns {
            let candidate = NodeBox::new(
                left + column as f64 * (width + SPAWN_GUTTER) + width / 2.0,
                top + row as f64 * (height + SPAWN_GUTTER) + height / 2.0,
                width,
                height,
            );
            if !territory.contains_box(candidate.x, candidate.y, width, height) {
                continue;
            }
            if siblings.iter().any(|sibling| sibling.overlaps(&candidate)) {
                continue;
            }
            return Ok(candidate);
        }
    }

    Err(Refusal::RegionFull { capacity: columns * rows, siblings: siblings.len() })
}

/// Whether this orchestrator may spawn an agent at a position it chose itself.
///
/// The three checks, in the order a refusal is most useful in: **role**, then **cap**, then
/// **territory**. The order is not cosmetic — a worker asked to spawn must be told that
/// workers do not spawn, and since `effective_spawn_cap()` is 0 for a worker, a cap check
/// first would answer *"your limit is 0"*, which is true and useless.
///
/// `live_children` is what the **caller** counts as running. See [`count_children`] for why
/// that is not the same number as "agents this orchestrator has ever spawned".
pub fn may_spawn(
    parent: &AgentModel,
    live_children: u32,
    proposed: NodeBox,
) -> Result<(), Refusal> {
    let territory = managing_territory(parent, live_children)?;
    if territory.contains_box(proposed.x, proposed.y, proposed.width, proposed.height) {
        Ok(())
    } else {
        Err(Refusal::OutsideTerritory { territory, proposed })
    }
}

/// Whether this orchestrator may spawn, and where the new agent goes.
///
/// The whole answer in one call: role, cap, and a position that is inside the territory and
/// clear of the siblings already in it. `width`/`height` are the caller's, because measuring
/// a node is `vellum-app`'s job and this crate deliberately does no layout.
pub fn plan_spawn(
    parent: &AgentModel,
    live_children: u32,
    siblings: &[NodeBox],
    width: f64,
    height: f64,
) -> Result<NodeBox, Refusal> {
    let territory = managing_territory(parent, live_children)?;
    place_in(&territory, siblings, width, height)
}

/// Role and cap, and the region to spawn into — the part [`may_spawn`] and [`plan_spawn`]
/// share, so the two cannot come to disagree about which check runs first.
fn managing_territory(parent: &AgentModel, live_children: u32) -> Result<Territory, Refusal> {
    if !parent.role_kind.may_spawn() {
        return Err(Refusal::RoleMayNotSpawn { role: parent.role_kind });
    }
    let cap = parent.effective_spawn_cap();
    if live_children >= cap {
        return Err(Refusal::CapReached { cap, live: live_children });
    }
    match parent.territory {
        Some(territory) if !territory.is_empty() => Ok(territory),
        _ => Err(Refusal::NoTerritory),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1000 × 600 region at the origin. With a 200 × 100 agent that is exactly 4 columns
    /// (4 × 200 + 3 × 24 = 872 ≤ 1000, and a fifth would need 1096) and 5 rows
    /// (5 × 100 + 4 × 24 = 596 ≤ 600) — room for 20.
    ///
    /// Whole numbers on purpose: a test whose expected column count depends on the last bit
    /// of an f64 division is a test that fails on somebody else's machine.
    fn region() -> Territory {
        Territory::new(0.0, 0.0, 1000.0, 600.0)
    }

    fn boss() -> AgentModel {
        AgentModel { territory: Some(region()), ..AgentModel::orchestrator() }
    }

    /// Unbounded recursive spawning is the failure the cap exists to prevent, so a worker
    /// may not spawn *at all* — and it must be told that, not told its limit is zero.
    ///
    /// The message assertion is the half that matters: a build with the role check deleted
    /// still refuses here, through `effective_spawn_cap() == 0`, with `CapReached { cap: 0 }`
    /// — a refusal that is true, useless, and would pass a test that only checked `is_err()`.
    #[test]
    fn a_worker_may_not_spawn_and_is_told_why() {
        let worker = AgentModel { territory: Some(region()), ..AgentModel::worker() };
        let refusal = may_spawn(&worker, 0, NodeBox::new(0.0, 0.0, 100.0, 100.0))
            .expect_err("a worker was allowed to spawn");

        assert_eq!(refusal, Refusal::RoleMayNotSpawn { role: RoleKind::Worker });
        let message = refusal.message();
        assert!(message.contains("may not spawn"), "{message}");
        assert!(message.contains("yourself"), "a refusal with no remedy: {message}");
        assert!(!message.contains('0'), "a worker was told about a limit of 0: {message}");
    }

    /// The cap refuses the (n+1)th, and the message says so — with both numbers in it,
    /// because *"you have 3 of your 3"* is what an orchestrator can act on and *"refused"*
    /// is not.
    #[test]
    fn the_cap_refuses_the_next_one_and_the_message_says_so() {
        let capped = AgentModel { spawn_cap: Some(3), ..boss() };
        let inside = NodeBox::new(0.0, 0.0, 100.0, 100.0);

        assert!(may_spawn(&capped, 2, inside).is_ok(), "the third spawn was refused");

        let refusal = may_spawn(&capped, 3, inside).expect_err("the fourth spawn was allowed");
        assert_eq!(refusal, Refusal::CapReached { cap: 3, live: 3 });
        let message = refusal.message();
        assert!(message.contains('3'), "{message}");
        assert!(message.contains("limit"), "{message}");
        assert!(message.contains("Wait"), "the refusal did not say what to do: {message}");

        // And the default, for an orchestrator that never chose one.
        let refusal = may_spawn(&boss(), AgentModel::DEFAULT_SPAWN_CAP, inside).unwrap_err();
        assert_eq!(
            refusal,
            Refusal::CapReached { cap: AgentModel::DEFAULT_SPAWN_CAP, live: 5 }
        );
    }

    /// Whole-box containment, not centre containment — the rule `Territory::contains_box`
    /// already states. An agent spawned half outside its orchestrator's region is one the
    /// user has to tidy up, and a centre test lets exactly that through.
    #[test]
    fn a_box_hanging_over_the_edge_is_refused() {
        let boss = boss();
        // Centred at x = 480, 200 wide: right edge at 580, the region ends at 500.
        let overhanging = NodeBox::new(480.0, 0.0, 200.0, 100.0);
        assert!(region().contains_point(overhanging.x, overhanging.y), "the centre is inside");

        let refusal = may_spawn(&boss, 0, overhanging).expect_err("a straddling box fitted");
        assert!(matches!(refusal, Refusal::OutsideTerritory { .. }));
        let message = refusal.message();
        assert!(message.contains("-500") && message.contains("580"), "{message}");

        // Nudged fully inside, the same size is fine.
        assert!(may_spawn(&boss, 0, NodeBox::new(390.0, 0.0, 200.0, 100.0)).is_ok());
    }

    /// An orchestrator with no region is refused rather than allowed the whole board:
    /// unbounded spawning arrived at by omission is still unbounded spawning.
    #[test]
    fn an_orchestrator_with_no_region_cannot_spawn_anywhere() {
        let unplaced = AgentModel::orchestrator();
        assert_eq!(unplaced.territory, None);
        assert_eq!(
            may_spawn(&unplaced, 0, NodeBox::new(0.0, 0.0, 10.0, 10.0)),
            Err(Refusal::NoTerritory)
        );

        // A zero-sized territory is the same statement written differently.
        let flat =
            AgentModel { territory: Some(Territory::new(0.0, 0.0, 0.0, 0.0)), ..boss() };
        assert_eq!(plan_spawn(&flat, 0, &[], 10.0, 10.0), Err(Refusal::NoTerritory));
    }

    /// Placement packs rather than piles: five agents placed one after another must all be
    /// inside the region and none may overlap another. Checked pairwise, because a bug that
    /// puts the third on top of the first passes any check that only compares neighbours.
    #[test]
    fn placement_never_overlaps_a_sibling() {
        let boss = boss();
        let mut placed: Vec<NodeBox> = Vec::new();
        for index in 0..5 {
            let next = plan_spawn(&boss, index, &placed, 200.0, 100.0)
                .expect("a region with room for 20 refused agent number 5");
            assert!(
                region().contains_box(next.x, next.y, next.width, next.height),
                "placed outside the territory: {next:?}"
            );
            placed.push(next);
        }

        for (i, one) in placed.iter().enumerate() {
            for other in placed.iter().skip(i + 1) {
                assert!(!one.overlaps(other), "{one:?} overlaps {other:?}");
            }
        }
        // Four columns fit, so the fifth wraps to the second row rather than running off
        // the right-hand edge.
        assert_eq!(placed[4].top(), placed[0].top() + 100.0 + SPAWN_GUTTER);
    }

    /// A box the user dragged in by hand sits on no grid, and the packing must go round it
    /// rather than through it.
    #[test]
    fn placement_avoids_a_box_that_is_not_on_the_grid() {
        let stray = NodeBox::new(-390.0, -240.0, 210.0, 110.0);
        let placed = place_in(&region(), &[stray], 200.0, 100.0).expect("refused with room left");
        assert!(!placed.overlaps(&stray), "packed straight through a hand-placed box");
        assert!(region().contains_box(placed.x, placed.y, placed.width, placed.height));
    }

    /// A full region and a reached cap are different problems with different remedies —
    /// waiting fixes one and never fixes the other — so they are different refusals with
    /// different messages.
    #[test]
    fn a_full_region_refuses_for_a_different_reason_than_a_reached_cap() {
        // 424 × 100 holds exactly two 200 × 100 agents across (200 + 24 + 200) and one down.
        let narrow = Territory::new(0.0, 0.0, 424.0, 100.0);
        let boss = AgentModel { territory: Some(narrow), spawn_cap: Some(9), ..boss() };
        assert_eq!(capacity(&narrow, 200.0, 100.0), 2);

        let first = plan_spawn(&boss, 0, &[], 200.0, 100.0).unwrap();
        let second = plan_spawn(&boss, 1, &[first], 200.0, 100.0).unwrap();

        let refusal = plan_spawn(&boss, 2, &[first, second], 200.0, 100.0)
            .expect_err("a third agent fitted in a region with room for two");
        assert_eq!(refusal, Refusal::RegionFull { capacity: 2, siblings: 2 });

        let capped = Refusal::CapReached { cap: 2, live: 2 };
        assert_ne!(refusal, capped);
        assert_ne!(refusal.message(), capped.message());
        assert!(
            refusal.message().contains("larger region"),
            "a full region was reported as something waiting would fix: {}",
            refusal.message()
        );
        assert!(capped.message().contains("Wait"));

        // And an agent larger than the whole region is its own sentence, not "all places
        // are taken" with a capacity of zero in it.
        let too_big = place_in(&narrow, &[], 900.0, 100.0).unwrap_err();
        assert_eq!(too_big, Refusal::RegionFull { capacity: 0, siblings: 0 });
        assert!(too_big.message().contains("does not fit"), "{}", too_big.message());
    }

    /// The packing reads no clock and no RNG — this crate has neither — so the same board
    /// asked twice must answer the same, or two spawns in one frame could land on top of
    /// each other and a fixture could never assert a position.
    #[test]
    fn placement_is_deterministic() {
        let siblings = [NodeBox::new(-400.0, -250.0, 200.0, 100.0)];
        let once = place_in(&region(), &siblings, 200.0, 100.0).unwrap();
        let twice = place_in(&region(), &siblings, 200.0, 100.0).unwrap();
        assert_eq!(once, twice);

        // And a freed cell is reused rather than walked past: with the sibling gone, the
        // next agent goes back to the first place.
        let empty = place_in(&region(), &[], 200.0, 100.0).unwrap();
        assert_eq!(empty, siblings[0]);
    }

    /// The cleanup query: an orchestrator's children, and its children's children, because
    /// an orchestrator may spawn an orchestrator and a single level would leave
    /// grandchildren behind with a parent that no longer exists.
    #[test]
    fn deleting_an_orchestrator_names_every_agent_below_it() {
        let nodes = [
            AgentNode::raw("boss", None),
            AgentNode::raw("a", Some("boss")),
            AgentNode::raw("sub", Some("boss")),
            AgentNode::raw("b", Some("sub")),
            AgentNode::raw("stranger", None),
            AgentNode::raw("theirs", Some("stranger")),
        ];

        assert_eq!(children_of("boss", &nodes), ["a", "sub"]);
        assert_eq!(count_children("boss", &nodes), 2);
        assert_eq!(descendants_of("boss", &nodes), ["a", "sub", "b"]);
        assert!(!descendants_of("boss", &nodes).contains(&"theirs"), "took another's child");
        assert!(descendants_of("a", &nodes).is_empty());
    }

    /// `spawned_by` is a string in a board file, which is data this crate does not get to
    /// assume is well formed. A self-parent or a loop is malformed rather than impossible,
    /// and the alternative to guarding is a hang while opening a board.
    #[test]
    fn a_parentage_loop_terminates_instead_of_hanging() {
        let looped = [
            AgentNode::raw("boss", Some("child")),
            AgentNode::raw("child", Some("boss")),
            AgentNode::raw("self", Some("self")),
        ];
        assert_eq!(descendants_of("boss", &looped), ["child"]);
        assert!(children_of("self", &looped).is_empty(), "a node adopted itself");
    }

    /// A refusal the orchestrator cannot read is a spawn that silently did not happen — the
    /// worst of the three outcomes, because the work is neither delegated nor done. It goes
    /// into the transcript as an error, which is visible in **both** display modes.
    #[test]
    fn a_refusal_reaches_the_transcript_in_both_display_modes() {
        let event = Refusal::CapReached { cap: 5, live: 5 }.into_event();
        assert!(event.visible_in_clean_mode(), "a refusal was hidden by a display preference");
        match &event {
            TranscriptEvent::Error { message } => {
                assert!(message.contains("limit is 5"), "{message}");
            }
            other => panic!("a refusal became {other:?}"),
        }
        assert!(event.headline().starts_with("error:"));

        // And the same text is what a toast gets, through the crate's one error type.
        let error: crate::AgentError = Refusal::NoTerritory.into();
        assert!(error.to_string().contains("territory"), "{error}");
    }
}
