//! The configuration a node carries in the document.
//!
//! These are the opaque tokens `vellum-doc` stores as strings and never looks inside —
//! [`ItemKind::Agent`], [`ItemKind::FileTree`], [`ItemKind::AgentNote`] and
//! [`ItemKind::Browser`] in that crate. `vellum-app` encodes and decodes them; the
//! document holds the bytes.
//!
//! # Every field defaults, and that is a file-format decision
//!
//! Each struct here is `#[serde(default)]` and skips its empty fields on the way out. Two
//! consequences, both deliberate:
//!
//! - **A token written by a later build still parses in an earlier one.** A field that did
//!   not exist reads as its default rather than failing the decode. That is RULE ZERO's
//!   posture applied one level down: an unknown value degrades, it never aborts a load.
//! - **A freshly placed agent writes almost nothing** — `{"provider":…}` and little else —
//!   so a board of agent nodes stays small and diffable.
//!
//! The one thing that is *not* here is anything the agent produced. Transcripts live in a
//! sidecar outside the document; see `docs/07-agent-canvas.md` §4.

use serde::{Deserialize, Serialize};

/// What an agent node is for.
///
/// All three are `ItemKind::Agent` on the canvas — they are told apart here, not by the
/// document, because they differ in what they are allowed to do rather than in what they
/// are. A worker and an orchestrator are both a box that runs an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    /// Does the work: writes code, researches, answers. The default.
    #[default]
    Worker,
    /// Manages other agents rather than working itself. Owns a region of the board and a
    /// hard cap on how many agents it may spawn — see [`Territory`] and [`AgentModel::spawn_cap`].
    Orchestrator,
    /// The board's control plane. May read and rewrite *other* nodes' configuration on the
    /// user's behalf, which is a power no other role has — see [`RoleKind::may_configure_others`].
    Meta,
}

impl RoleKind {
    pub const ALL: [Self; 3] = [Self::Worker, Self::Orchestrator, Self::Meta];

    /// The on-disk tag. Stable: it is written into board files.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Orchestrator => "orchestrator",
            Self::Meta => "meta",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Worker => "Agent",
            Self::Orchestrator => "Orchestrator",
            Self::Meta => "Meta agent",
        }
    }

    /// Whether this role may rewrite another node's configuration.
    ///
    /// **The meta agent alone**, and it is enforced at the IPC boundary rather than by
    /// asking the model nicely in a prompt. A capability that lives only in an instruction
    /// is not a capability boundary — the same reasoning that puts an orchestrator's spawn
    /// cap in Velm rather than in its system context.
    pub const fn may_configure_others(self) -> bool {
        matches!(self, Self::Meta)
    }

    /// Whether this role may spawn sub-agents at all.
    ///
    /// An orchestrator's whole purpose, and the meta agent's when asked to set a board up.
    /// A worker may not: unbounded recursive spawning is the failure mode the cap exists to
    /// prevent, and a worker has no cap because it has no territory to spawn into.
    pub const fn may_spawn(self) -> bool {
        matches!(self, Self::Orchestrator | Self::Meta)
    }
}

/// How much of an agent's working the node shows.
///
/// Per node, with a global default new nodes inherit — feature 2, both halves. The split
/// itself lives on the event, not here: see [`TranscriptEvent::visible_in_clean_mode`].
///
/// [`TranscriptEvent::visible_in_clean_mode`]: crate::TranscriptEvent::visible_in_clean_mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    /// Every tool call, shell command, file read and reasoning step — what the Claude Code
    /// VS Code extension shows. Useful for debugging and for trusting the thing; noisy for
    /// daily use.
    Raw,
    /// The polished answer and nothing else — the claude.ai experience. The default,
    /// because the noisy one should be the one you ask for.
    #[default]
    Clean,
}

impl DisplayMode {
    pub const ALL: [Self; 2] = [Self::Clean, Self::Raw];

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Clean => "clean",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Raw => "Raw",
            Self::Clean => "Clean",
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Raw => Self::Clean,
            Self::Clean => Self::Raw,
        }
    }
}

/// A rectangle of the board an orchestrator is responsible for, in world units.
///
/// Stored as a centre and an extent, matching `vellum_doc::Placement`, so the region and an
/// item's box are compared without a coordinate conversion in between.
///
/// **The cap and the territory are enforced in Velm, not in the prompt.** An orchestrator
/// told in words to stay inside a rectangle is an orchestrator that will eventually not.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Territory {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Territory {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    /// Whether a point is inside. Half-open on the far edges so two abutting territories
    /// cannot both claim the seam between them.
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        let (left, top) = (self.x - self.width / 2.0, self.y - self.height / 2.0);
        x >= left && x < left + self.width && y >= top && y < top + self.height
    }

    /// Whether a box centred at `(x, y)` fits entirely inside.
    ///
    /// Containment of the whole box rather than of its centre: an agent spawned half
    /// outside its orchestrator's region is an agent the user has to tidy up, and "spawn
    /// inside my territory" is not a statement about a centre point.
    pub fn contains_box(&self, x: f64, y: f64, width: f64, height: f64) -> bool {
        let (left, top) = (self.x - self.width / 2.0, self.y - self.height / 2.0);
        let (bx, by) = (x - width / 2.0, y - height / 2.0);
        bx >= left
            && by >= top
            && bx + width <= left + self.width
            && by + height <= top + self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// Where an agent's own rule overrides live, and what they say.
///
/// The bottom layer of the three-layer cascade in [`crate::rules`]. Held here rather than in
/// a file because it belongs to *this node* — a per-agent override that lived on disk would
/// need a path, a lifetime and a cleanup story, and it is three lines of text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentRules {
    /// Free-form instructions for this agent alone. Appended last, so it wins.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Fields this node sets explicitly rather than inheriting. Named so the inspector can
    /// show *inherited* against *set here* from what the agent actually got, rather than
    /// re-deriving it and being able to disagree.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<String>,
    /// Whether the project's and the global rules apply at all.
    ///
    /// `false` — inherit — is the default and very nearly always right. `true` is for the
    /// one agent that must not be told the house style, and it is a deliberate cliff rather
    /// than a slope: partially inheriting a rule set is not a thing anybody can reason about.
    #[serde(skip_serializing_if = "is_false")]
    pub ignore_inherited: bool,
}

/// One file, page or media item an agent has been given as context.
///
/// The *extracted* form is cached in the sidecar, not here: this records what the user
/// attached, so re-opening a board re-establishes the context without re-attaching, and a
/// re-extraction is possible when the extractor improves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContextSource {
    /// A filesystem path or a URL, verbatim as the user gave it.
    pub source: String,
    /// What the ingester decided this is — `pdf`, `docx`, `audio`, `video`, `youtube`,
    /// `web`, `text`. A tag rather than an enum so a source ingested by a later build with
    /// more kinds still round-trips through this one.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// BLAKE3 hash of the extracted text in the sidecar cache, when it has been extracted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    /// What to show the user: a filename, a page title.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// Everything a node needs to run an agent, and nothing it produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentModel {
    /// Worker, orchestrator or meta. See [`RoleKind`].
    #[serde(skip_serializing_if = "is_default")]
    pub role_kind: RoleKind,

    /// The provider and model this node runs on — **per node**, which is the whole point:
    /// one board can have one agent on Claude, one on a local GPU and one on Kimi.
    ///
    /// `None` inherits the board's default provider, and that is a different statement from
    /// naming the same provider explicitly: an inherited node follows when the board's
    /// default changes, a named one does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<crate::ProviderChoice>,

    /// The working directory, for a coding agent. `None` means the board's project root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,

    /// Raw or clean. `None` inherits the app-wide default, so changing that default moves
    /// every node that never chose — feature 2's "new agents inherit your preferred mode".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayMode>,

    /// This agent's own rule overrides. See [`AgentRules`] and [`crate::rules`].
    #[serde(skip_serializing_if = "is_default")]
    pub rules: AgentRules,

    /// When this agent runs by itself, and what it does afterwards. `None` is an agent that
    /// runs only when asked, which is every agent until the user says otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<crate::Schedule>,

    /// The region of the board an orchestrator owns. Meaningless — and ignored — for a
    /// worker, which is why it is not on a separate orchestrator type: a node's role is
    /// editable, and a user who switches a worker to an orchestrator and back should not
    /// lose the territory they drew.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub territory: Option<Territory>,

    /// The most sub-agents this orchestrator may have running at once.
    ///
    /// `None` means the crate default rather than "unbounded" — there is no unbounded here,
    /// deliberately. An orchestrator that can spawn without limit is one API bill and one
    /// unusable board away from being the reason this feature gets turned off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_cap: Option<u32>,

    /// The orchestrator that spawned this agent, as an item id string. `None` for one the
    /// user placed. Kept so a cap can count what it is responsible for, and so an
    /// orchestrator's children can be cleaned up with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<String>,

    /// Files, pages and media this agent has been given. See [`ContextSource`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextSource>,

    /// Whether this agent gets its own git worktree.
    ///
    /// Set from the **project's** toggle when the node is created rather than chosen per
    /// node, which is feature 4's "a single clear toggle in the project's settings rather
    /// than something enabled per-agent inconsistently" — but recorded here because the
    /// worktree is this agent's, and turning the project setting off later must not orphan
    /// a worktree that already has work in it.
    #[serde(skip_serializing_if = "is_false")]
    pub worktree: bool,

    /// The worktree's path, once one has been created. `None` until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,

    /// Whether push-to-talk is offered on this node. Off by default: a microphone that
    /// turns itself on is not a feature.
    #[serde(skip_serializing_if = "is_false")]
    pub voice: bool,

    /// Whether this agent may be messaged by agents it is connected to.
    ///
    /// On by default — a connector drawn between two agents *is* the request. The switch
    /// exists so a noisy neighbour can be muted without deleting the line that documents
    /// the relationship.
    #[serde(skip_serializing_if = "is_true", default = "yes")]
    pub accepts_messages: bool,
}

impl AgentModel {
    /// The default cap on an orchestrator's simultaneous sub-agents.
    ///
    /// Five, which is the number the feature request itself used — *"spawn up to 5 agents as
    /// needed"* — and small enough that the first surprise is affordable.
    pub const DEFAULT_SPAWN_CAP: u32 = 5;

    /// A worker with everything inherited. What the agent tool places.
    pub fn worker() -> Self {
        Self { accepts_messages: true, ..Self::default() }
    }

    pub fn orchestrator() -> Self {
        Self {
            role_kind: RoleKind::Orchestrator,
            spawn_cap: Some(Self::DEFAULT_SPAWN_CAP),
            accepts_messages: true,
            ..Self::default()
        }
    }

    pub fn meta() -> Self {
        Self { role_kind: RoleKind::Meta, accepts_messages: true, ..Self::default() }
    }

    /// The cap actually applied, resolving `None` to the default rather than to no limit.
    pub fn effective_spawn_cap(&self) -> u32 {
        match self.role_kind {
            RoleKind::Worker => 0,
            _ => self.spawn_cap.unwrap_or(Self::DEFAULT_SPAWN_CAP),
        }
    }
}

/// Who may read and write a note.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "scope")]
pub enum NoteScope {
    /// Every agent on this board. The board's shared memory.
    #[default]
    Shared,
    /// One agent alone, named by its item id. The agent's own memory.
    ///
    /// The restriction is enforced at the IPC boundary, **not** with file permissions: the
    /// user must keep full access to their own files, and a note they cannot open in an
    /// editor would defeat the entire reason notes are files.
    Private { agent: String },
}

impl NoteScope {
    pub const fn is_private(&self) -> bool {
        matches!(self, Self::Private { .. })
    }

    pub const fn label(&self) -> &'static str {
        match self {
            Self::Shared => "Shared with every agent",
            Self::Private { .. } => "Private to one agent",
        }
    }
}

/// A note node: where its file is, who may touch it, and what it links to.
///
/// The content is **not** here. It is a `.md` file on disk, which is the entire feature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NoteModel {
    /// The `.md` file, relative to the project root where there is one and absolute
    /// otherwise. Relative by preference so a project that moves keeps working.
    pub path: String,
    #[serde(skip_serializing_if = "is_default")]
    pub scope: NoteScope,
    /// Other notes this one points at, as paths, so an agent can follow a chain of context.
    /// Derived from the file's own markdown links on each read, and cached here so a chain
    /// is traversable without opening every note on the board.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// The file's modification time in seconds when it was last read, so an external edit
    /// can be noticed without reading the whole file every frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_mtime: Option<u64>,
    /// The file's length when last read. Paired with the mtime because a filesystem with
    /// one-second mtime granularity cannot distinguish two edits in the same second, and a
    /// note is exactly the file an agent rewrites twice quickly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_len: Option<u64>,
}

/// A file-tree node: which directory, and whose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FileTreeModel {
    /// The directory shown. Empty means the board's project root.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub root: String,
    /// The agent this tree belongs to, as an item id string.
    ///
    /// Feature 7's hard requirement: *scoped per agent*, so two trees on a board can show
    /// two different subtrees rather than two copies of the same project. `None` is a tree
    /// the user placed for themselves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Directories the user has opened, as paths relative to `root`. A tree is read from
    /// disk, so this is the only part of it worth storing — and it is what makes a board
    /// reopen looking the way it was left.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<String>,
    /// Whether files ignored by git are shown. Off by default: a `target/` directory with
    /// 40,000 entries in it is not a project structure.
    #[serde(skip_serializing_if = "is_false")]
    pub show_ignored: bool,
}

/// A browser node: which page, and how it behaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrowserModel {
    pub url: String,
    /// The page's title, once known, so the node reads as something before — or without —
    /// an engine ever being instantiated.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Whether this node has been allowed to run a real engine.
    ///
    /// Distinct from the app-wide opt-in and *additional* to it: the setting says browser
    /// nodes are permitted at all, this says the user asked *this* page to load. Both, or
    /// the node draws as a card that offers to open the page in the real browser.
    #[serde(skip_serializing_if = "is_false")]
    pub live: bool,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

const fn is_true(value: &bool) -> bool {
    *value
}

const fn yes() -> bool {
    true
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens are the file format. A freshly placed agent must write as close to
    /// nothing as possible — a board of agent nodes should stay small and diffable — and
    /// every field must survive a round trip.
    #[test]
    fn a_default_worker_serialises_to_almost_nothing() {
        let json = serde_json::to_string(&AgentModel::worker()).unwrap();
        assert_eq!(json, "{}", "a default worker wrote fields it did not need");

        let back: AgentModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AgentModel::worker());
        assert!(back.accepts_messages, "accepts_messages must default to on, not off");
    }

    /// The RULE ZERO posture one level down: a token from a later build, carrying fields
    /// this one has never heard of, must still decode rather than costing the node.
    #[test]
    fn a_token_from_a_later_build_still_decodes() {
        let future = r#"{"role_kind":"worker","telepathy":true,"provider":null,
                         "unknown_nested":{"a":[1,2,3]}}"#;
        let model: AgentModel = serde_json::from_str(future).expect("an unknown field aborted");
        assert_eq!(model.role_kind, RoleKind::Worker);
    }

    /// And the other direction: every field this build writes must come back the same.
    #[test]
    fn a_fully_configured_agent_round_trips() {
        let model = AgentModel {
            role_kind: RoleKind::Orchestrator,
            provider: None,
            working_dir: Some("/tmp/project".into()),
            display: Some(DisplayMode::Raw),
            rules: AgentRules {
                text: "talk to me formally".into(),
                overrides: vec!["tone".into()],
                ignore_inherited: true,
            },
            schedule: None,
            territory: Some(Territory::new(0.0, 0.0, 1000.0, 800.0)),
            spawn_cap: Some(3),
            spawned_by: Some("42@7".into()),
            context: vec![ContextSource {
                source: "spec.pdf".into(),
                kind: "pdf".into(),
                extract: Some("hash".into()),
                label: "spec.pdf".into(),
            }],
            worktree: true,
            worktree_path: Some("/tmp/wt".into()),
            voice: true,
            accepts_messages: false,
        };
        let json = serde_json::to_string(&model).unwrap();
        assert_eq!(serde_json::from_str::<AgentModel>(&json).unwrap(), model);
    }

    /// A worker has no territory to spawn into, so it gets no cap — and `None` on an
    /// orchestrator resolves to the default rather than to "as many as it likes". There is
    /// no unbounded value here on purpose.
    #[test]
    fn a_cap_is_never_unbounded() {
        assert_eq!(AgentModel::worker().effective_spawn_cap(), 0);
        assert_eq!(
            AgentModel::orchestrator().effective_spawn_cap(),
            AgentModel::DEFAULT_SPAWN_CAP
        );

        let uncapped = AgentModel { spawn_cap: None, ..AgentModel::orchestrator() };
        assert_eq!(uncapped.effective_spawn_cap(), AgentModel::DEFAULT_SPAWN_CAP);
    }

    /// Only the meta agent may rewrite another node, and only the two managing roles may
    /// spawn. Both are enforced at the IPC boundary; this is the definition they read.
    #[test]
    fn only_the_meta_agent_may_reconfigure_others() {
        assert!(RoleKind::Meta.may_configure_others());
        assert!(!RoleKind::Orchestrator.may_configure_others());
        assert!(!RoleKind::Worker.may_configure_others());

        assert!(RoleKind::Orchestrator.may_spawn());
        assert!(RoleKind::Meta.may_spawn());
        assert!(!RoleKind::Worker.may_spawn());
    }

    /// A box that hangs over the edge is not inside, even when its centre is. An agent
    /// spawned half outside its orchestrator's region is one the user has to tidy up.
    #[test]
    fn a_territory_contains_a_whole_box_not_just_its_centre() {
        let region = Territory::new(0.0, 0.0, 100.0, 100.0);
        assert!(region.contains_point(0.0, 0.0));
        assert!(!region.contains_point(60.0, 0.0));

        assert!(region.contains_box(0.0, 0.0, 40.0, 40.0));
        assert!(!region.contains_box(40.0, 0.0, 40.0, 40.0), "a box hanging over the edge fitted");
        assert!(region.contains_box(0.0, 0.0, 100.0, 100.0), "an exactly-fitting box was refused");
    }

    /// Half-open on the far edges, so two abutting territories cannot both claim the seam.
    #[test]
    fn abutting_territories_do_not_share_their_seam() {
        let left = Territory::new(-50.0, 0.0, 100.0, 100.0);
        let right = Territory::new(50.0, 0.0, 100.0, 100.0);
        assert!(left.contains_point(-50.0, 0.0) && !right.contains_point(-50.0, 0.0));
        // The seam at x = 0 belongs to exactly one of them.
        assert_ne!(left.contains_point(0.0, 0.0), right.contains_point(0.0, 0.0));
    }

    #[test]
    fn note_and_tree_tokens_round_trip() {
        let note = NoteModel {
            path: ".velm/notes/plan.md".into(),
            scope: NoteScope::Private { agent: "42@7".into() },
            links: vec![".velm/notes/context.md".into()],
            seen_mtime: Some(1_700_000_000),
            seen_len: Some(2048),
        };
        let json = serde_json::to_string(&note).unwrap();
        assert_eq!(serde_json::from_str::<NoteModel>(&json).unwrap(), note);
        assert!(note.scope.is_private());

        let tree = FileTreeModel {
            root: "crates/vellum-agent".into(),
            agent: Some("1@2".into()),
            expanded: vec!["src".into()],
            show_ignored: false,
        };
        let json = serde_json::to_string(&tree).unwrap();
        assert_eq!(serde_json::from_str::<FileTreeModel>(&json).unwrap(), tree);

        // A default note writes only its path — the scope tag is the default and is skipped.
        let bare = serde_json::to_string(&NoteModel::default()).unwrap();
        assert_eq!(bare, r#"{"path":""}"#);
    }

    /// A browser node needs *two* yeses: the app-wide opt-in and this page. One is a
    /// permission, the other is an instruction, and collapsing them would make enabling the
    /// setting load every browser node on every board at once.
    #[test]
    fn a_browser_node_is_not_live_until_it_is_asked_to_be() {
        let node = BrowserModel { url: "https://example.com".into(), ..BrowserModel::default() };
        assert!(!node.live);
        assert_eq!(serde_json::to_string(&node).unwrap(), r#"{"url":"https://example.com"}"#);
    }

    #[test]
    fn display_mode_toggles_and_tags_round_trip() {
        assert_eq!(DisplayMode::default(), DisplayMode::Clean);
        assert_eq!(DisplayMode::Clean.toggled(), DisplayMode::Raw);
        assert_eq!(DisplayMode::Raw.toggled(), DisplayMode::Clean);
        for mode in DisplayMode::ALL {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<DisplayMode>(&json).unwrap(), mode);
        }
        for role in RoleKind::ALL {
            let json = serde_json::to_string(&role).unwrap();
            assert_eq!(serde_json::from_str::<RoleKind>(&json).unwrap(), role);
        }
    }
}
