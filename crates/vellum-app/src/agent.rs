//! Agent nodes on the board: the document token, and the one layout that decides where
//! every piece of one is.
//!
//! `vellum-agent` owns everything about *running* an agent — providers, transports, rules,
//! the transcript. It never measures text and never lays anything out, exactly as
//! `vellum-flow` and `vellum-mindmap` never do. This module is the join: it turns
//! [`ItemKind::Agent`]'s opaque string into an [`AgentModel`] and back, and it works out
//! where the header, the status dot, the mode toggle, the transcript and the prompt row sit
//! inside the item's box.
//!
//! # The token
//!
//! An opaque string, as [`crate::shapes`], [`crate::table`], [`crate::chart`],
//! [`crate::mindmap`] and [`crate::kanban`] use, for the layering reason those record:
//! `vellum-doc` depends on `loro` and `thiserror` and nothing else, and holding an
//! `AgentModel` there would drag an HTTP client and a PTY host into the document layer.
//!
//! Unlike those five, an agent's **role label is not in the token**. It lives beside it as
//! the item's `StyledText`, the way a shape's label lives beside its form — which is what
//! puts a role inside search, the on-canvas caret and `Board::set_text` with no new path.
//!
//! # One layout, two readers
//!
//! [`layout`] is called by the painter *and* by the press path. That is not an
//! optimisation, it is the rule this repo has already paid for twice — `draw::kanban_runs`
//! and `CardLayout::badge` both exist because a second copy of a layout is a click that
//! lands where the paint is not. Anything that can be pressed on an agent node gets its
//! rectangle from here.

use vellum_agent::{AgentModel, ChatTheme, DisplayMode, RoleKind};
use vellum_doc::{ArrowKind, ItemKind};

/// An axis-aligned box in the item's own space: origin at the item's top-left, units are
/// world units before the placement's scale.
///
/// Local to this module rather than borrowed from `vellum-doc`, because everything here is
/// *inside* one item and a `Placement` is a centre plus an extent in board coordinates —
/// converting back and forth at every row would be the arithmetic this type exists to
/// avoid.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// The same box inset on every side, clamped so it can never invert.
    ///
    /// Clamped rather than allowed to go negative: an inverted rectangle passes
    /// [`Rect::contains`] for nothing and draws as a zero-size quad, so the failure is a
    /// piece of the node silently missing — which is precisely the class of bug the badge
    /// arithmetic produced on short cards (feedback 25).
    pub fn inset(&self, by: f64) -> Self {
        Self {
            x: self.x + by,
            y: self.y + by,
            width: (self.width - by * 2.0).max(0.0),
            height: (self.height - by * 2.0).max(0.0),
        }
    }
}

/// The token stored in the document for an agent node.
pub fn encode(model: &AgentModel) -> String {
    serde_json::to_string(model).unwrap_or_else(|error| {
        log::warn!("an agent node would not encode ({error}); storing nothing");
        String::new()
    })
}

/// The configuration a token names, or a default worker when it cannot be read.
///
/// **Never fails.** An unreadable token costs the node's settings, not the node — the same
/// degradation rule the document layer applies to an unknown style value. A node that
/// refused to decode would be an item on the board that cannot be drawn, selected or
/// deleted, which is strictly worse than one that came back as a plain worker.
pub fn decode(token: &str) -> AgentModel {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable agent node ({error}); using the default configuration");
        }
        AgentModel::worker()
    })
}

/// What the agent tool places, in world units.
///
/// Wide enough for a line of prose to be a line rather than four words, and tall enough
/// that a short exchange is visible without resizing. Measured against the 13px body size
/// `docs/05-design-language.md` §5 sets: ~60 characters to the line, ~14 lines of
/// transcript.
pub const DEFAULT_SIZE: (f64, f64) = (520.0, 400.0);

/// The smallest an agent node is allowed to be laid out at.
///
/// Below this the header alone does not fit, and [`layout`] would start returning empty
/// rectangles for rows that are still being drawn. Rather than let that happen silently,
/// [`layout`] reports [`AgentLayout::too_small`] and the painter draws a compact badge.
pub const MIN_SIZE: (f64, f64) = (180.0, 96.0);

/// Padding inside the node's own edge.
const PAD: f64 = 12.0;

/// The header row's height: the role, the provider and the status.
const HEADER_HEIGHT: f64 = 34.0;

/// The prompt row's height at the foot of the node.
const PROMPT_HEIGHT: f64 = 32.0;

/// The status dot's diameter.
const DOT: f64 = 10.0;

/// The square controls in the header — the mode toggle, the run/stop button.
const CONTROL: f64 = 22.0;

/// Where every part of an agent node is.
///
/// Rectangles are in the item's own space — see [`Rect`]. Any of them may be empty when the
/// node is too small to hold it; callers must test rather than assume, which is why
/// [`Rect::is_empty`] exists and why [`AgentLayout::too_small`] is reported rather than
/// inferred from a zero height somewhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgentLayout {
    /// The whole node.
    pub bounds: Rect,
    /// The header strip: role label, provider, controls.
    pub header: Rect,
    /// The role label's text box. Yields to the controls to its right — it is clipped to
    /// what is left, rather than the controls being allowed to sit on top of it.
    pub role: Rect,
    /// The status dot, at the header's left.
    pub status: Rect,
    /// The raw/clean toggle.
    pub mode: Rect,
    /// Run, or stop while a turn is in flight. One button, two meanings, because they are
    /// never both available and two buttons would leave one of them permanently dead.
    pub run: Rect,
    /// The transcript. The tall one.
    pub transcript: Rect,
    /// The prompt row at the foot.
    pub prompt: Rect,
    /// True when the node is smaller than [`MIN_SIZE`] and only a compact badge is drawn.
    pub too_small: bool,
}

impl AgentLayout {
    /// Which part of the node a point in item space is over, if any.
    ///
    /// The press path's single question, answered from the same rectangles the painter drew
    /// — see the module header.
    pub fn hit(&self, x: f64, y: f64) -> Option<AgentPart> {
        if self.too_small {
            // A compact node is one target: pressing it selects the item, nothing more.
            // Offering a 6-unit mode toggle would be a control nobody can hit.
            return None;
        }
        // Controls first, then the regions behind them. Order is the z-order: the mode
        // toggle sits inside the header, so a header-first test would swallow every press
        // on it — the same "last overlapping widget wins" trap the board library hit.
        for (rect, part) in [
            (self.mode, AgentPart::ModeToggle),
            (self.run, AgentPart::Run),
            (self.status, AgentPart::Status),
            (self.role, AgentPart::Role),
            (self.prompt, AgentPart::Prompt),
            (self.transcript, AgentPart::Transcript),
            (self.header, AgentPart::Header),
        ] {
            if !rect.is_empty() && rect.contains(x, y) {
                return Some(part);
            }
        }
        None
    }
}

/// A pressable part of an agent node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentPart {
    /// The role label — a press puts the caret in it, so a role is edited on the node
    /// exactly as feature 5 asks.
    Role,
    /// The raw/clean toggle.
    ModeToggle,
    /// Start a turn, or stop the one running.
    Run,
    /// The status dot: pressing it shows what the agent is waiting for.
    Status,
    /// The prompt row — a press puts the caret there.
    Prompt,
    /// The transcript body. A press here can land on an option card or an image; which one
    /// is a question about the transcript's own contents, resolved by the painter's run
    /// list rather than by this layout.
    Transcript,
    /// Bare header, away from any control.
    Header,
}

/// Where every part of an agent node sits, given the item's size in world units.
///
/// `width` and `height` are the item's own, before the placement's scale — the painter
/// applies that, as it does for every other structured widget.
pub fn layout(width: f64, height: f64) -> AgentLayout {
    let bounds = Rect::new(0.0, 0.0, width.max(0.0), height.max(0.0));
    let empty = Rect::default();

    if width < MIN_SIZE.0 || height < MIN_SIZE.1 {
        return AgentLayout {
            bounds,
            header: bounds,
            role: bounds.inset(PAD / 2.0),
            status: empty,
            mode: empty,
            run: empty,
            transcript: empty,
            prompt: empty,
            too_small: true,
        };
    }

    let inner = bounds.inset(PAD);
    let header = Rect::new(inner.x, inner.y, inner.width, HEADER_HEIGHT);

    // The header, laid out from both ends: the status dot at the left, the two controls at
    // the right, and the role taking whatever is between them. Laying the role out first
    // and clipping it afterwards is the version that puts a control on top of a word.
    let dot_y = header.y + (header.height - DOT) / 2.0;
    let status = Rect::new(header.x, dot_y, DOT, DOT);
    let control_y = header.y + (header.height - CONTROL) / 2.0;
    let run = Rect::new(header.x + header.width - CONTROL, control_y, CONTROL, CONTROL);
    let mode = Rect::new(run.x - CONTROL - PAD / 2.0, control_y, CONTROL, CONTROL);

    let role_x = status.x + DOT + PAD / 2.0;
    let role_width = (mode.x - PAD / 2.0 - role_x).max(0.0);
    let role = Rect::new(role_x, header.y, role_width, header.height);

    // The prompt sits at the foot and the transcript takes the rest. The transcript is the
    // one that flexes, because it is the one whose content has no natural height.
    let prompt = Rect::new(
        inner.x,
        inner.y + inner.height - PROMPT_HEIGHT,
        inner.width,
        PROMPT_HEIGHT,
    );
    let transcript_top = header.y + header.height + PAD / 2.0;
    let transcript = Rect::new(
        inner.x,
        transcript_top,
        inner.width,
        (prompt.y - PAD / 2.0 - transcript_top).max(0.0),
    );

    AgentLayout { bounds, header, role, status, mode, run, transcript, prompt, too_small: false }
}

/// The one-line description under an agent's role: what it runs on, and how it is billed.
///
/// Built here rather than in the painter because the *inspector* shows the same line, and
/// two derivations of "what is this agent running on" is exactly how a panel and a node
/// come to disagree in front of the user.
pub fn subtitle(model: &AgentModel) -> String {
    let provider = model
        .provider
        .as_ref()
        .map_or_else(|| "Board default".to_string(), vellum_agent::ProviderChoice::summary);
    match model.role_kind {
        RoleKind::Worker => provider,
        kind => format!("{} · {provider}", kind.label()),
    }
}

/// The region a newly placed orchestrator or meta agent owns.
///
/// # Why a default territory exists at all
///
/// `vellum_agent::orchestrator` refuses every spawn from a node with no territory, and it is
/// right to: a territory arrived at by omission is an unbounded one, which is exactly what
/// the cap exists to prevent. But an orchestrator created with `None` cannot spawn until the
/// user has discovered that it needs a rectangle drawn, which makes the feature dead on
/// arrival — the same shape as `opens_context_menu` being written, tested and callerless.
///
/// So a new one is born owning a bounded region **derived from its own box**, which the user
/// can then redraw. Five node-widths by three node-heights, centred on the node: enough for
/// [`AgentModel::DEFAULT_SPAWN_CAP`] children laid out around their parent with room to
/// spare, and — the property that matters — **independent of the zoom**. Taking the viewport
/// instead was the obvious alternative and is worse: the same gesture would produce a
/// territory of wildly different size depending on how far out the board happened to be
/// scrolled, which is not something the user could predict or later reason about.
pub fn default_territory(placement: &vellum_doc::Placement) -> vellum_agent::Territory {
    let width = (placement.width * placement.scale).abs().max(DEFAULT_SIZE.0);
    let height = (placement.height * placement.scale).abs().max(DEFAULT_SIZE.1);
    vellum_agent::Territory::new(placement.x, placement.y, width * 5.0, height * 3.0)
}

/// The display mode this node actually draws in, resolving `None` against the app-wide
/// default the user set in Preferences.
///
/// A free function taking the default rather than a method on `AgentModel`, because the
/// default lives in the library sidecar and `vellum-agent` must not know about it.
pub const fn display_mode(model: &AgentModel, fallback: DisplayMode) -> DisplayMode {
    match model.display {
        Some(mode) => mode,
        None => fallback,
    }
}

/// The chat theme this node actually draws in, resolving `None` against the app-wide default.
///
/// [`display_mode`]'s twin, in the same shape and for the same reason: the default lives in
/// the library sidecar and `vellum-agent` must not know about it.
pub const fn chat_theme(
    model: &AgentModel,
    fallback: ChatTheme,
) -> ChatTheme {
    match model.chat_theme {
        Some(theme) => theme,
        None => fallback,
    }
}

/// How see-through this node's paper is, as a multiplier.
///
/// **Floored well above zero.** A transcript at zero opacity is a node you cannot find, let
/// alone select — the same argument `Library::persist`'s transparency slider makes for the
/// chrome, and sharper here, because the thing that would vanish is the only handle on a
/// running process. The words are never faded by this at all; see
/// [`vellum_agent::AgentModel::chat_opacity`].
pub fn chat_opacity(model: &AgentModel) -> f32 {
    let raw = model.chat_opacity.map_or(1.0, |a| f32::from(a) / 255.0);
    raw.clamp(MIN_CHAT_OPACITY, 1.0)
}

/// The floor under [`chat_opacity`]. A fifth is still plainly a card.
pub const MIN_CHAT_OPACITY: f32 = 0.2;

/// The background picture's hash, read straight out of the token.
///
/// # Why this reads the JSON rather than decoding
///
/// The painter asks once per visible agent node per frame, and `decode` builds a whole
/// `AgentModel` — a provider, a rule set, a schedule, a vector of context sources — to
/// answer a question about one optional string that is usually absent. That is the idle cost
/// `docs/07-agent-canvas.md` §0 forbids, and it is the defect feedback 34 found twenty lines
/// from a correct use of the R-tree.
///
/// The key is [`vellum_agent::AgentModel::chat_background`]'s serde name.
/// `the_background_key_is_the_one_serde_writes` pins the two together, which is the trap
/// `inspect.rs`'s `THEME_BORDER` fell into once already.
pub fn background_hash(token: &str) -> Option<&str> {
    let needle = "\"chat_background\":\"";
    let start = token.find(needle)? + needle.len();
    let rest = token.get(start..)?;
    let end = rest.find('"')?;
    // A hash is hex, so an escape cannot appear inside one and a plain scan to the next
    // quote is exact. Non-empty, because a zero-length hash names no blob and would send
    // `Assets::texture` looking for one every frame.
    rest.get(..end).filter(|hash| !hash.is_empty())
}

/// What a connector between two nodes *means*, when at least one end is an agent.
///
/// # Why this is derived and not stored
///
/// Nothing is added to [`ItemKind::Connector`]. A stored "this is an agent link" flag would
/// be a second source of truth that can disagree with the endpoints it describes, and this
/// repo has already paid for that twice — the grid's two controls, one of which was
/// permanently inert, and `inspect.rs`'s hardcoded `locked: false`. Derivation cannot drift,
/// needs no migration, and makes an agent link out of every connector the user has already
/// drawn between two agents.
///
/// **Direction is the arrowhead**, which is a control the user already has on the context
/// bar, rather than a new one that would have to be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// An ordinary connector. Neither end is an agent, or one end is unbound.
    Plain,
    /// Agent to agent: messages may pass. This is the wire feature 3 asks for.
    Message(Direction),
    /// An agent and something it can read — a note, or a file tree. The agent takes it as
    /// context. Drawn differently from a message link, because "reads this" and "talks to
    /// this" are not the same relationship and a board where they look alike is one you
    /// cannot follow.
    Context,
}

/// Which way messages flow along a message link.
///
/// ⚠ **This is a description of the drawn line, not the gate on delivery.** Routing is
/// `vellum_agent::Topology`'s, through the `LinkDirection` `sync_agent_wiring` derives from
/// the same arrowheads — so a message that may not travel is refused by the bus, in one place,
/// against the topology the bus itself holds. There used to be `allows_forward` /
/// `allows_backward` here as well: a second, *unused* copy of the same rule with its own tests
/// passing beside a painter that matches `Message(_)` with a wildcard. A tested rule nothing
/// calls is worse than no rule, because it reads as the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Start to end.
    Forward,
    /// End to start.
    Backward,
    /// Both ways. What an undecorated line means: a connector with no arrowhead says the
    /// two are related without saying who leads, so both may speak.
    Both,
}

/// Whether a kind is one an agent can be wired to as a *peer* — i.e. an agent.
pub const fn is_agent(kind: &ItemKind) -> bool {
    matches!(kind, ItemKind::Agent { .. })
}

/// Whether a kind is something an agent can be wired to as *context* to read.
///
/// A note and a file tree, and deliberately not a sticky: a sticky's words are document
/// content that only Velm can write, while a note is a file on disk an agent can edit. The
/// distinction is the entire reason notes are their own kind, and blurring it here would
/// make a context link promise a write the agent cannot perform.
pub const fn is_context_source(kind: &ItemKind) -> bool {
    matches!(kind, ItemKind::AgentNote { .. } | ItemKind::FileTree { .. })
}

/// What a connector between `start_kind` and `end_kind` means.
///
/// `None` for either kind is an endpoint pinned to the canvas rather than to an item — an
/// unbound end cannot be an agent, so such a connector is always [`LinkKind::Plain`].
pub fn link_kind(
    start_kind: Option<&ItemKind>,
    end_kind: Option<&ItemKind>,
    start_arrow: ArrowKind,
    end_arrow: ArrowKind,
) -> LinkKind {
    let (Some(start), Some(end)) = (start_kind, end_kind) else {
        return LinkKind::Plain;
    };

    if is_agent(start) && is_agent(end) {
        // The arrowhead sits on the end it points *at*, so a head on `end` means
        // start → end. A head on both, or on neither, is a line that does not choose.
        let direction = match (start_arrow != ArrowKind::None, end_arrow != ArrowKind::None) {
            (false, true) => Direction::Forward,
            (true, false) => Direction::Backward,
            _ => Direction::Both,
        };
        return LinkKind::Message(direction);
    }

    // Context is symmetric: an agent joined to a note reads that note whichever way the
    // line was drawn. Requiring the user to draw it "the right way round" would be a rule
    // nothing on screen states.
    if (is_agent(start) && is_context_source(end)) || (is_context_source(start) && is_agent(end)) {
        return LinkKind::Context;
    }

    LinkKind::Plain
}

impl LinkKind {
    /// Whether this link is one the Agent Canvas draws differently from an ordinary
    /// connector.
    pub const fn is_agent_link(self) -> bool {
        !matches!(self, Self::Plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_agent::{Provider, ProviderChoice};

    fn agent() -> ItemKind {
        ItemKind::Agent { model: String::new(), label: vellum_doc::StyledText::plain("a") }
    }

    fn note() -> ItemKind {
        ItemKind::AgentNote { model: String::new(), title: vellum_doc::StyledText::plain("n") }
    }

    fn sticky() -> ItemKind {
        ItemKind::Sticky { text: vellum_doc::StyledText::plain("s"), background: None }
    }

    const NONE: ArrowKind = ArrowKind::None;
    const HEAD: ArrowKind = ArrowKind::FilledTriangle;

    /// The whole derivation: two agents joined by a line may talk, and the arrowhead — a
    /// control the user already has — says which way.
    #[test]
    fn two_agents_joined_by_a_line_can_talk_and_the_arrowhead_says_which_way() {
        let (a, b) = (agent(), agent());
        assert_eq!(
            link_kind(Some(&a), Some(&b), NONE, HEAD),
            LinkKind::Message(Direction::Forward)
        );
        assert_eq!(
            link_kind(Some(&a), Some(&b), HEAD, NONE),
            LinkKind::Message(Direction::Backward)
        );
        // No head, or a head at both ends, is a line that does not choose — so both may
        // speak. An undecorated connector is the commonest thing anyone draws, and making
        // it mean "nobody may speak" would leave the feature switched off by default.
        assert_eq!(link_kind(Some(&a), Some(&b), NONE, NONE), LinkKind::Message(Direction::Both));
        assert_eq!(link_kind(Some(&a), Some(&b), HEAD, HEAD), LinkKind::Message(Direction::Both));
    }

    /// An agent joined to a note reads it, whichever way round the line was drawn —
    /// requiring a direction here would be a rule nothing on screen states.
    #[test]
    fn an_agent_joined_to_a_note_reads_it_either_way_round() {
        let (a, n) = (agent(), note());
        assert_eq!(link_kind(Some(&a), Some(&n), NONE, HEAD), LinkKind::Context);
        assert_eq!(link_kind(Some(&n), Some(&a), NONE, HEAD), LinkKind::Context);

        let tree = ItemKind::FileTree { model: String::new() };
        assert_eq!(link_kind(Some(&a), Some(&tree), NONE, NONE), LinkKind::Context);
    }

    /// A sticky is deliberately not a context source. Its words are document content only
    /// Velm can write, and a context link to one would promise an agent a write it cannot
    /// perform — which is the whole reason notes are a separate kind.
    #[test]
    fn a_sticky_is_not_context_however_it_is_wired() {
        let (a, s) = (agent(), sticky());
        assert_eq!(link_kind(Some(&a), Some(&s), NONE, HEAD), LinkKind::Plain);
        assert_eq!(link_kind(Some(&s), Some(&a), NONE, HEAD), LinkKind::Plain);
        assert!(!is_context_source(&sticky()));
    }

    /// An unbound end cannot be an agent, so a half-attached connector stays ordinary. The
    /// case matters: dragging a connector out of an agent and dropping it on bare canvas is
    /// something people do constantly, and it must not produce a live message wire to
    /// nothing.
    #[test]
    fn a_connector_with_a_free_end_is_never_an_agent_link() {
        let a = agent();
        assert_eq!(link_kind(Some(&a), None, NONE, HEAD), LinkKind::Plain);
        assert_eq!(link_kind(None, Some(&a), NONE, HEAD), LinkKind::Plain);
        assert_eq!(link_kind(None, None, NONE, NONE), LinkKind::Plain);
    }

    #[test]
    fn only_agent_links_are_drawn_differently() {
        assert!(!LinkKind::Plain.is_agent_link());
        assert!(LinkKind::Message(Direction::Both).is_agent_link());
        assert!(LinkKind::Context.is_agent_link());
    }

    /// Two notes joined to each other are not an agent link — note chaining is a property
    /// of the markdown inside them, not of a line on the board. Deriving it from a
    /// connector as well would give a note two ways to link with different semantics.
    #[test]
    fn two_notes_joined_to_each_other_are_an_ordinary_connector() {
        assert_eq!(link_kind(Some(&note()), Some(&note()), NONE, HEAD), LinkKind::Plain);
    }

    /// An unreadable token must cost the node's settings and never the node. A decode that
    /// could fail would put an item on the board that cannot be drawn, selected or deleted.
    #[test]
    fn an_unreadable_token_degrades_to_a_default_worker() {
        assert_eq!(decode("not json at all"), AgentModel::worker());
        assert_eq!(decode(""), AgentModel::worker());
        assert_eq!(decode("[1, 2, 3]"), AgentModel::worker());
        // A token from a later build, with fields this one has never seen, still decodes.
        let model = decode(r#"{"role_kind":"meta","telepathy":true}"#);
        assert_eq!(model.role_kind, RoleKind::Meta);
    }

    #[test]
    fn a_model_round_trips_through_its_token() {
        let model = AgentModel {
            provider: Some(ProviderChoice::new(Provider::Local).with_model("qwen3")),
            display: Some(DisplayMode::Raw),
            ..AgentModel::orchestrator()
        };
        assert_eq!(decode(&encode(&model)), model);
    }

    /// The rule the module header states: the press path and the painter read one layout.
    /// So every control the painter draws must be reachable by [`AgentLayout::hit`] at its
    /// own centre — a control that draws and cannot be hit is the bug this shares.
    #[test]
    fn every_control_is_hittable_at_its_own_centre() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        assert!(!l.too_small);

        for (rect, expected) in [
            (l.mode, AgentPart::ModeToggle),
            (l.run, AgentPart::Run),
            (l.status, AgentPart::Status),
            (l.role, AgentPart::Role),
            (l.prompt, AgentPart::Prompt),
            (l.transcript, AgentPart::Transcript),
        ] {
            assert!(!rect.is_empty(), "{expected:?} laid out empty at the default size");
            let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
            assert_eq!(l.hit(cx, cy), Some(expected), "{expected:?} was not hittable");
        }
    }

    /// The header is laid out from both ends so a control can never sit on the role. The
    /// assertion is about the *gap*, because two rectangles that merely do not overlap
    /// would still put a word hard against a button.
    #[test]
    fn the_role_yields_to_the_controls_rather_than_being_covered() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        assert!(
            l.role.x + l.role.width <= l.mode.x,
            "the role label ran into the mode toggle: role ends at {}, toggle starts at {}",
            l.role.x + l.role.width,
            l.mode.x
        );
        assert!(l.mode.x + l.mode.width <= l.run.x, "the two header controls overlap");
        assert!(l.role.x >= l.status.x + l.status.width, "the role label sits on the status dot");
    }

    /// Every piece must stay inside the node. A part that escapes its own item is the
    /// badge-on-a-short-card failure (feedback 25) in a new place — it draws outside the
    /// item and cannot be pressed.
    #[test]
    fn no_part_escapes_the_node_at_any_size() {
        for (w, h) in [
            DEFAULT_SIZE,
            (MIN_SIZE.0, MIN_SIZE.1),
            (MIN_SIZE.0 + 1.0, MIN_SIZE.1 + 1.0),
            (2000.0, 120.0),
            (200.0, 2000.0),
            (520.0, 97.0),
        ] {
            let l = layout(w, h);
            for (name, rect) in [
                ("header", l.header),
                ("role", l.role),
                ("status", l.status),
                ("mode", l.mode),
                ("run", l.run),
                ("transcript", l.transcript),
                ("prompt", l.prompt),
            ] {
                if rect.is_empty() {
                    continue;
                }
                assert!(
                    rect.x >= -0.001
                        && rect.y >= -0.001
                        && rect.x + rect.width <= w + 0.001
                        && rect.y + rect.height <= h + 0.001,
                    "{name} escaped a {w}x{h} node: {rect:?}"
                );
            }
        }
    }

    /// A node too small for its own header reports it rather than returning rows that are
    /// silently empty — the painter draws a compact badge instead, and a control nobody can
    /// hit is never offered.
    #[test]
    fn a_tiny_node_reports_that_it_is_tiny_rather_than_laying_out_nothing() {
        let l = layout(40.0, 30.0);
        assert!(l.too_small);
        assert_eq!(l.hit(20.0, 15.0), None, "a compact node offered a control");
        assert!(!l.role.is_empty(), "even a compact node draws its role");

        // And the boundary is where it says it is.
        assert!(!layout(MIN_SIZE.0, MIN_SIZE.1).too_small);
        assert!(layout(MIN_SIZE.0 - 0.1, MIN_SIZE.1).too_small);
        assert!(layout(MIN_SIZE.0, MIN_SIZE.1 - 0.1).too_small);
    }

    /// Degenerate sizes must not produce an inverted rectangle. An inverted box contains
    /// nothing and draws as nothing, so the symptom is a piece of the node silently missing.
    #[test]
    fn a_degenerate_size_never_produces_an_inverted_rectangle() {
        for (w, h) in [(0.0, 0.0), (-10.0, -10.0), (1.0, 1.0)] {
            let l = layout(w, h);
            for rect in [l.bounds, l.header, l.role, l.transcript, l.prompt] {
                assert!(rect.width >= 0.0 && rect.height >= 0.0, "{rect:?} inverted at {w}x{h}");
            }
        }
    }

    /// The subtitle is shown on the node *and* in the inspector, from this one function —
    /// two derivations is how the two come to disagree in front of the user.
    #[test]
    fn the_subtitle_names_the_provider_and_how_it_is_billed() {
        let inherited = AgentModel::worker();
        assert_eq!(subtitle(&inherited), "Board default");

        let claude = AgentModel {
            provider: Some(ProviderChoice::new(Provider::Claude)),
            ..AgentModel::worker()
        };
        assert!(subtitle(&claude).contains("subscription"), "{}", subtitle(&claude));

        let kimi = AgentModel {
            provider: Some(ProviderChoice::new(Provider::Kimi)),
            ..AgentModel::orchestrator()
        };
        let line = subtitle(&kimi);
        assert!(line.starts_with("Orchestrator · "), "{line}");
        assert!(line.ends_with("API"), "{line}");
    }

    /// An orchestrator placed with no territory can never spawn — `vellum_agent`'s own
    /// refusal, and correct. So one is born owning a region, and the region has to be big
    /// enough for the cap it is also born with, or the feature is refusals all the way down.
    #[test]
    fn a_new_orchestrator_owns_a_region_big_enough_for_its_own_cap() {
        let placement = vellum_doc::Placement::new(0.0, 0.0, DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        let region = default_territory(&placement);
        assert!(!region.is_empty());
        assert!(region.contains_box(0.0, 0.0, DEFAULT_SIZE.0, DEFAULT_SIZE.1), "no room for itself");

        // Room for the default cap's worth of children — asked of the **real packer**
        // rather than recomputed here. The first version of this assertion did the
        // arithmetic itself, assumed a single row, and failed a territory that was in fact
        // large enough: 2600 wide against a row needing 2720, when `place_in` packs
        // row-major and would have used two rows. A test that reimplements the thing it is
        // checking is a test that can be wrong on its own account, which is exactly what
        // happened.
        let cap = AgentModel::DEFAULT_SPAWN_CAP as usize;
        let room = vellum_agent::orchestrator::capacity(&region, DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        assert!(
            room >= cap,
            "a default territory holds {room} children but the cap it ships with is {cap}"
        );
    }

    /// The property that ruled out using the viewport: the same gesture must produce the
    /// same territory however far out the board is scrolled. A zoom-dependent default is
    /// one the user cannot predict and cannot later reason about.
    #[test]
    fn a_default_territory_does_not_depend_on_the_zoom() {
        let a = default_territory(&vellum_doc::Placement::new(10.0, 20.0, 520.0, 400.0));
        let b = default_territory(&vellum_doc::Placement::new(10.0, 20.0, 520.0, 400.0));
        assert_eq!(a, b);
        // And it follows the node, so two orchestrators far apart do not overlap.
        let far = default_territory(&vellum_doc::Placement::new(9000.0, 20.0, 520.0, 400.0));
        assert!(!far.contains_point(10.0, 20.0), "two distant orchestrators claim the same ground");
    }

    /// A tiny node must not get a tiny territory — an orchestrator dragged out at 60x40
    /// would otherwise own a region no child could fit in, and every spawn would come back
    /// `RegionFull`, which reads as broken rather than as full.
    #[test]
    fn a_small_orchestrator_still_gets_a_usable_region() {
        let tiny = default_territory(&vellum_doc::Placement::new(0.0, 0.0, 60.0, 40.0));
        assert!(
            tiny.contains_box(0.0, 0.0, DEFAULT_SIZE.0, DEFAULT_SIZE.1),
            "a small orchestrator's region cannot hold one default-sized child"
        );
    }

    /// A node that never chose a mode follows the app-wide default, so changing that
    /// default in Preferences moves every such node — feature 2's second half.
    #[test]
    fn a_node_that_never_chose_a_mode_follows_the_global_default() {
        let inherited = AgentModel::worker();
        assert_eq!(display_mode(&inherited, DisplayMode::Raw), DisplayMode::Raw);
        assert_eq!(display_mode(&inherited, DisplayMode::Clean), DisplayMode::Clean);

        let chosen = AgentModel { display: Some(DisplayMode::Raw), ..AgentModel::worker() };
        assert_eq!(
            display_mode(&chosen, DisplayMode::Clean),
            DisplayMode::Raw,
            "a node's own choice was overridden by the global default"
        );
    }

    /// The same, for the theme a transcript is dressed in.
    #[test]
    fn a_node_that_never_chose_a_theme_follows_the_global_default() {
        let inherited = AgentModel::worker();
        assert_eq!(chat_theme(&inherited, ChatTheme::Claude), ChatTheme::Claude);

        let chosen = AgentModel { chat_theme: Some(ChatTheme::Kimi), ..AgentModel::worker() };
        assert_eq!(
            chat_theme(&chosen, ChatTheme::Claude),
            ChatTheme::Kimi,
            "a node's own theme was overridden by the global default"
        );
    }

    /// [`background_hash`] finds the key **serde actually writes**.
    ///
    /// It scans the token's JSON for a string literal rather than decoding, because the
    /// painter asks once per visible agent node per frame and `decode` builds a whole
    /// `AgentModel` to answer a question about one usually-absent field. The cost of that
    /// shortcut is a **hardcoded field name**, and this is the join that keeps it honest:
    /// rename `AgentModel::chat_background` — or give it a `#[serde(rename)]` — and every
    /// picture on every board silently stops being drawn, with nothing failing to compile
    /// and no test failing either.
    ///
    /// That is precisely the trap `inspect.rs`'s `THEME_BORDER` and the old `locked: false`
    /// constant both fell into: a value hand-copied across a boundary, made wrong by an edit
    /// in a different crate that breaks nothing. The test is written from the **encoder**,
    /// never from a literal, so it cannot agree with a stale reader.
    #[test]
    fn the_background_key_is_the_one_serde_writes() {
        let hash = "d0a1f0beef";
        let model = AgentModel {
            chat_background: Some(hash.to_owned()),
            ..AgentModel::worker()
        };
        let token = encode(&model);
        assert_eq!(
            background_hash(&token),
            Some(hash),
            "the painter cannot find the picture serde just wrote into {token}"
        );

        // A node with no picture, and one whose picture was cleared, must both answer
        // `None` — an empty hash names no blob and would send `Assets::texture` looking for
        // one on every frame for the life of the board.
        assert_eq!(background_hash(&encode(&AgentModel::worker())), None);
        let empty = AgentModel { chat_background: Some(String::new()), ..AgentModel::worker() };
        assert_eq!(background_hash(&encode(&empty)), None);

        // And it must not match a *different* field that happens to contain the words. A
        // node's working directory is free text the user chose.
        let decoy = AgentModel {
            working_dir: Some("/tmp/\"chat_background\":\"nope".to_owned()),
            ..AgentModel::worker()
        };
        assert_eq!(
            background_hash(&encode(&decoy)),
            None,
            "a path was read as a picture hash"
        );
    }
}
