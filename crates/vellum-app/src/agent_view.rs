//! What the painter is told about the agents running on this board.
//!
//! This module is a **seam**, and it exists so that two things can be built against one
//! agreed shape: `crate::agent_runtime` fills these in once per frame, and `crate::draw`
//! reads them. Neither knows anything else about the other.
//!
//! # Why the painter is handed a view rather than the sessions
//!
//! A `vellum_agent::Session` owns a transport, a thread and a channel. The painter runs
//! inside a `&mut self` borrow of the whole `Painter` and must not be able to start a turn,
//! block on a read, or mutate a session by accident. Handing it a plain snapshot makes that
//! impossible by construction rather than by discipline — which is the same reason
//! `vellum_doc::Item` is a snapshot and not a live handle.
//!
//! It also keeps the expensive decision in one place. **Which events a node shows is
//! resolved here, not in the painter**: the display mode is applied, the tail is bounded,
//! and an off-screen node is never given a view at all. A painter that filtered for itself
//! would be a second definition of "clean mode" — and `TranscriptEvent::visible_in_clean_mode`
//! is deliberately the only one.
//!
//! # Nothing here drives a repaint
//!
//! A running agent produces events, and events are what cause the frame that draws them.
//! There is no spinner and no timer: this application's whole argument is that it costs
//! nothing while nothing is happening, and a rotating status glyph on an idle board would
//! spend a GPU on saying "still working".
//!
//! The one exception is [`AgentViews::pulses`], and it is the board library's fade rule
//! rather than a violation of it — a pulse asks for repaints for the fraction of a second a
//! message is actually travelling, and then stops. `vellum_agent::Bus::any_in_flight` is the
//! early-out that makes an idle board cost one boolean.

use vellum_scene::ItemId as SceneId;
use std::collections::HashMap;
use vellum_agent::{DisplayMode, LinkPulse, Status, TranscriptEvent};

/// One agent node, as the painter sees it.
#[derive(Debug, Clone, Default)]
pub struct AgentView {
    /// Idle, working, blocked on the user, or failed. Drawn as the status dot's colour.
    pub status: Status,
    /// One line saying what it is doing, or what went wrong. Shown under the role.
    ///
    /// A `String` rather than a borrowed `&str` because it is frequently *composed* — "ran
    /// 3 tools", "claude is not installed" — and a borrow would tie the view's lifetime to
    /// whichever session produced it, which is exactly the coupling this module removes.
    pub detail: String,
    /// The provider line: what it runs on and how it is billed.
    pub subtitle: String,
    /// The mode this node is actually drawing in, already resolved against the global
    /// default — see [`crate::agent::display_mode`].
    pub mode: DisplayMode,
    /// The events to draw, **already filtered for `mode` and already bounded**, oldest
    /// first. The painter draws what it is given and makes no decision about what belongs.
    pub events: Vec<TranscriptEvent>,
    /// True when there is more history than `events` carries, so the node can say so
    /// rather than silently appearing to be the whole story.
    pub truncated: bool,
    /// What the user has typed into this node's prompt row but not yet sent.
    ///
    /// Held by the runtime rather than by the caret, because a prompt survives clicking
    /// away from the node — losing a half-written instruction to a stray click is the kind
    /// of small betrayal that stops people trusting a tool.
    pub draft: String,
}

impl AgentView {
    /// Whether this node should draw itself as wanting a person.
    ///
    /// Delegated to `Status` rather than re-decided here, so the node, the inspector and
    /// the away-mode digest cannot come to disagree about which agents are blocked.
    pub const fn needs_attention(&self) -> bool {
        self.status.needs_attention()
    }
}

/// Every agent view for the frame being drawn, plus any messages currently in flight.
///
/// Deliberately **not** keyed by `ItemId`: the painter works in `SceneId`, and converting at
/// every lookup inside the draw loop is the kind of per-item cost this renderer is built to
/// avoid. The runtime does the conversion once, while it is already walking the projection.
#[derive(Debug, Clone, Default)]
pub struct AgentViews {
    views: HashMap<SceneId, AgentView>,
    /// A message travelling along a connector, by that connector's own scene id.
    ///
    /// A `Vec` rather than a map because it is nearly always empty and, when it is not, it
    /// holds one or two entries — a linear scan beats a hash on both.
    pulses: Vec<(SceneId, LinkPulse)>,
    /// Each visible note node's file contents, read once per frame by the runtime.
    ///
    /// **A note's text is not in the document** — that is the whole of §8 — so the painter
    /// has to be handed it, and this is the hand. A frame may not read a file.
    notes: HashMap<SceneId, String>,
    /// Each visible file-tree node's rows, likewise. Reading a directory is not something a
    /// frame may do either.
    trees: HashMap<SceneId, vellum_agent::filetree::View>,
    /// Whether browser nodes are permitted at all, from Preferences.
    ///
    /// Carried here rather than read from the library by the painter because the painter has
    /// no library — and because it is one bool that decides what every browser node on the
    /// board says about itself, so it belongs with the other per-frame agent facts.
    browser_nodes: bool,
    /// Whether the board holds **any** agent node, on screen or not.
    ///
    /// Distinct from `views` being non-empty, and the distinction is a real bug: `views`
    /// holds only what is *visible*, so a connector on screen whose two agent endpoints are
    /// both off screen would otherwise be drawn as an ordinary line. The link would flicker
    /// between styles as the user panned, which is worse than either answer alone.
    has_agents: bool,
}

impl AgentViews {
    /// The empty set. What every board that has no agents on it hands the painter, and what
    /// the tests that care about nothing else use.
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared, permanently empty set, for every caller that has no agents to report.
    ///
    /// `'static` rather than a temporary because [`crate::draw::DrawContext`] borrows it for
    /// the frame's lifetime, and a caller that built one on the spot would have to keep it
    /// alive by hand at every site. One allocation for the life of the process, made the
    /// first time it is asked for and never on a board that has no agents *and* never draws
    /// — which is why it is a `OnceLock` rather than a `const`: `HashMap::new` is not const.
    pub fn empty() -> &'static Self {
        static EMPTY: std::sync::OnceLock<AgentViews> = std::sync::OnceLock::new();
        EMPTY.get_or_init(Self::default)
    }

    /// True when there is nothing agent-shaped to draw at all.
    ///
    /// The painter's early-out, and the property that makes this layer free on an ordinary
    /// board: a board with no agent nodes costs one `is_empty` per frame.
    pub fn is_empty(&self) -> bool {
        self.views.is_empty() && self.pulses.is_empty()
    }

    pub fn insert(&mut self, id: SceneId, view: AgentView) {
        self.views.insert(id, view);
    }

    pub fn get(&self, id: SceneId) -> Option<&AgentView> {
        self.views.get(&id)
    }

    /// Records a message travelling along `connector`.
    pub fn add_pulse(&mut self, connector: SceneId, pulse: LinkPulse) {
        self.pulses.push((connector, pulse));
    }

    /// The pulse on one connector, if a message is passing along it right now.
    pub fn pulse(&self, connector: SceneId) -> Option<LinkPulse> {
        self.pulses.iter().find(|(id, _)| *id == connector).map(|(_, pulse)| *pulse)
    }

    /// Whether any message is in flight anywhere on the board.
    ///
    /// What decides whether the frame needs to be repainted again immediately. On an idle
    /// board this is a length check, which is the point.
    pub fn any_in_flight(&self) -> bool {
        !self.pulses.is_empty()
    }

    /// How many agent nodes have a view this frame. For the HUD.
    pub fn len(&self) -> usize {
        self.views.len()
    }

    /// Whether the board holds any agent node at all, visible or not.
    ///
    /// What the connector styling gates on. Gating on `is_empty()` instead makes a link
    /// between two off-screen agents draw as an ordinary connector, so the line changes
    /// style as the user pans — see the field's own comment.
    pub const fn has_agents(&self) -> bool {
        self.has_agents
    }

    pub const fn set_has_agents(&mut self, any: bool) {
        self.has_agents = any;
    }

    /// One visible note's file contents.
    pub fn note(&self, id: SceneId) -> Option<&str> {
        self.notes.get(&id).map(String::as_str)
    }

    pub fn set_note(&mut self, id: SceneId, body: String) {
        self.notes.insert(id, body);
    }

    /// One visible file tree's rows.
    pub fn tree(&self, id: SceneId) -> Option<&vellum_agent::filetree::View> {
        self.trees.get(&id)
    }

    pub fn set_tree(&mut self, id: SceneId, view: vellum_agent::filetree::View) {
        self.trees.insert(id, view);
    }

    /// Whether a browser node may run an engine at all.
    pub const fn browser_nodes(&self) -> bool {
        self.browser_nodes
    }

    pub const fn set_browser_nodes(&mut self, allowed: bool) {
        self.browser_nodes = allowed;
    }

    /// Empties the frame's answers while keeping the allocations.
    ///
    /// Called at the head of every rebuild. `clear` rather than a fresh `AgentViews` so a
    /// steady-state frame with agents on it allocates nothing, which is the same reason
    /// `DrawList` is owned and reused.
    pub fn clear(&mut self) {
        self.views.clear();
        self.pulses.clear();
        self.notes.clear();
        self.trees.clear();
        self.has_agents = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SceneId` is a bare `u64` in `vellum-scene` — named here so the tests read as being
    /// about scene ids rather than about numbers.
    const fn scene(n: u64) -> SceneId {
        n
    }

    /// The property the whole layer rests on: a board with no agents costs nothing. If this
    /// ever stops being a cheap check, every board in the application pays for a feature it
    /// is not using.
    #[test]
    fn a_board_with_no_agents_hands_the_painter_nothing() {
        let views = AgentViews::new();
        assert!(views.is_empty());
        assert!(!views.any_in_flight());
        assert_eq!(views.len(), 0);
        assert!(views.get(scene(1)).is_none());
        assert!(views.pulse(scene(1)).is_none());
    }

    #[test]
    fn a_view_is_found_by_the_scene_id_it_was_filed_under() {
        let mut views = AgentViews::new();
        views.insert(
            scene(7),
            AgentView {
                status: Status::Running,
                detail: "ran 3 tools".into(),
                ..AgentView::default()
            },
        );
        assert_eq!(views.len(), 1);
        assert!(!views.is_empty());
        assert_eq!(views.get(scene(7)).map(|v| v.status), Some(Status::Running));
        assert!(views.get(scene(8)).is_none(), "a view answered for the wrong node");
    }

    /// A pulse is asked for once per visible connector per frame, so the lookup has to be
    /// right about *which* connector — a pulse that answered for every line would light the
    /// whole board up whenever any two agents spoke.
    #[test]
    fn a_pulse_belongs_to_one_connector_and_not_the_others() {
        let mut views = AgentViews::new();
        views.add_pulse(scene(3), LinkPulse { forward: true, progress: 0.25 });
        assert!(views.any_in_flight());
        assert!(!views.is_empty());

        let found = views.pulse(scene(3)).expect("the pulse was not on its own connector");
        assert!(found.forward);
        assert!((found.progress - 0.25).abs() < f32::EPSILON);
        assert!(views.pulse(scene(4)).is_none(), "a pulse lit a connector it was not on");
    }

    /// Attention is delegated, never re-decided — so the node, the inspector and the
    /// away-mode digest cannot disagree about which agents are blocked.
    #[test]
    fn attention_comes_from_the_status_and_is_not_re_derived() {
        for status in [Status::Idle, Status::Running, Status::WaitingForPermission, Status::Error]
        {
            let view = AgentView { status, ..AgentView::default() };
            assert_eq!(
                view.needs_attention(),
                status.needs_attention(),
                "{status:?} disagreed with its own status"
            );
        }
        assert!(AgentView { status: Status::Error, ..AgentView::default() }.needs_attention());
        assert!(!AgentView { status: Status::Running, ..AgentView::default() }.needs_attention());
    }
}
