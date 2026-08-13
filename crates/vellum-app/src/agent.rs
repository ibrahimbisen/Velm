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

use vellum_agent::{AgentModel, DisplayMode, RoleKind};

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

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_agent::{Provider, ProviderChoice};

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
}
