//! What the properties panel shows, derived from what is selected.
//!
//! This is the chrome's one real state machine, and it is deliberately separated
//! from the widgets that draw it: given a slice of [`SelectionItem`]s it produces a
//! [`PanelModel`] with no `egui` involvement at all, so the rules — which sections
//! appear, which values are shared, which read *Mixed* — are testable without a
//! window.
//!
//! # The mixed-value rule
//!
//! Miro shows a property's value when every selected object agrees and a placeholder
//! when they do not, and editing a mixed property assigns to all of them. [`Field`]
//! is that rule, made explicit rather than reconstructed at each control:
//!
//! - [`Field::Absent`] — no selected object *has* this property. The control is not
//!   drawn. A selection of pen strokes has no font size.
//! - [`Field::Uniform`] — every object that has the property agrees.
//! - [`Field::Mixed`] — they disagree.
//!
//! Objects that do not have a property are skipped rather than counted as
//! disagreement: selecting a sticky and an ink stroke still shows one fill colour,
//! because the stroke never had one to conflict with.

use vellum_agent::{
    AgentRules, ContextSource, DisplayMode, NoteScope, ProviderChoice, ResolvedRules, RoleKind,
    Schedule, Territory,
};
use vellum_connect::{AnchorSide, Arrowhead, LineStyle, RoutingMode};
use vellum_doc::{Align, CardMode, Color, ItemId, Placement};

/// A property's value across a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Field<T> {
    /// Nothing in the selection carries this property.
    #[default]
    Absent,
    /// Everything that carries it agrees.
    Uniform(T),
    /// They disagree. The panel shows a placeholder, and an edit assigns to all.
    Mixed,
}

impl<T> Field<T> {
    /// Folds the values of every object that has the property.
    pub fn collect(values: impl IntoIterator<Item = T>) -> Self
    where
        T: PartialEq,
    {
        let mut iter = values.into_iter();
        let Some(first) = iter.next() else { return Self::Absent };
        if iter.all(|v| v == first) { Self::Uniform(first) } else { Self::Mixed }
    }

    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Uniform(v) => Some(v),
            Self::Absent | Self::Mixed => None,
        }
    }

    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub const fn is_mixed(&self) -> bool {
        matches!(self, Self::Mixed)
    }

    /// The value to show in a control that must display *something*, falling back to
    /// `default` when the field is mixed or absent. Editing from that fallback still
    /// assigns to the whole selection, which is exactly Miro's behaviour.
    pub fn or(&self, default: T) -> T
    where
        T: Clone,
    {
        self.value().cloned().unwrap_or(default)
    }
}

/// What an item is, for the purpose of deciding which controls apply.
///
/// There is no comment facet: `docs/features/README.md` §11 cuts comment threads with
/// the rest of collaboration, so nothing on a board can be one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemFacet {
    Sticky,
    Text,
    Shape,
    /// A link or oEmbed card. Its own facet rather than [`ItemFacet::Image`] — which is what
    /// it used to report, so the panel called a link "Image" and offered it a picture's
    /// controls. A card's editable properties are its display mode and where it points.
    Link,
    Table,
    Chart,
    MindMap,
    Kanban,
    Ink,
    Image,
    Frame,
    Connector,
    Group,
    /// A live agent node — a worker, an orchestrator or the meta agent. One facet for all
    /// three: they carry the same controls (provider, display mode, role, rules) and differ
    /// in which of them are *offered*, which is a question the controls answer for
    /// themselves rather than one the facet should fork over.
    Agent,
    /// A project's file structure, scoped to one agent.
    FileTree,
    /// A markdown note that is a real `.md` file on disk.
    Note,
    /// A live web page on the canvas.
    Browser,
}

impl ItemFacet {
    /// The singular noun used in the panel header.
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Sticky => "Sticky note",
            Self::Text => "Text",
            Self::Shape => "Shape",
            Self::Link => "Link",
            Self::Table => "Table",
            Self::Chart => "Chart",
            Self::MindMap => "Mind map",
            Self::Kanban => "Kanban board",
            Self::Ink => "Drawing",
            Self::Image => "Image",
            Self::Frame => "Frame",
            Self::Connector => "Connector",
            Self::Group => "Group",
            Self::Agent => "Agent",
            Self::FileTree => "File tree",
            Self::Note => "Note",
            Self::Browser => "Browser",
        }
    }
}

/// How a shape or sticky's outline is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Border {
    pub color: Color,
    /// Width in world units.
    pub width: f64,
    pub style: LineStyle,
}

/// Text weight, as offered in the panel.
///
/// Three steps rather than the full CSS 100–900 range: Miro offers exactly these,
/// and a weight with no matching face in the chosen family is a synthesised bold,
/// which looks worse than not offering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontWeight {
    #[default]
    Regular,
    Medium,
    Bold,
}

impl FontWeight {
    pub const ALL: [Self; 3] = [Self::Regular, Self::Medium, Self::Bold];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Regular => "Regular",
            Self::Medium => "Medium",
            Self::Bold => "Bold",
        }
    }

    /// The CSS numeric weight, which is what a font query needs.
    pub const fn css(self) -> u16 {
        match self {
            Self::Regular => 400,
            Self::Medium => 500,
            Self::Bold => 700,
        }
    }
}

/// Vertical placement of a text block inside its box.
///
/// `vellum-doc` models horizontal alignment only, because that is all Miro's `ta`
/// key carries; vertical alignment is a layout parameter the renderer applies, so it
/// lives here until the document grows a field for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VerticalAlign {
    Top,
    #[default]
    Middle,
    Bottom,
}

impl VerticalAlign {
    pub const ALL: [Self; 3] = [Self::Top, Self::Middle, Self::Bottom];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Top => "Top",
            Self::Middle => "Middle",
            Self::Bottom => "Bottom",
        }
    }
}

/// The text properties of one item.
#[derive(Debug, Clone, PartialEq)]
pub struct TextSummary {
    /// The words themselves, flattened to plain text.
    ///
    /// Plain rather than styled because the panel's own field is plain. The **canvas** caret
    /// is not — it preserves an item's runs across an edit — so this is what the panel shows
    /// and edits, not the whole of what the item can hold. Typing here still flattens nothing:
    /// the app splices this string into the item's existing runs, exactly as a keystroke on the
    /// canvas does.
    pub content: String,
    /// `None` is the board default rather than a named family.
    pub family: Option<String>,
    /// `None` is auto-fit — Miro's `fs: 0, fsa: 1`.
    pub size: Option<f64>,
    pub weight: FontWeight,
    pub color: Color,
    pub align: Align,
    pub vertical_align: VerticalAlign,
    /// A multiple of the font size, matching Miro's `lh`.
    pub line_height: f64,
}

/// The link properties of one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSummary {
    /// Where the card points. `None` for a card whose URL was never recorded — Miro serves
    /// a few — which is why Open is disabled rather than absent for those.
    pub url: Option<String>,
    /// The site's name, for the panel to show beside the mode.
    pub provider: Option<String>,
    pub mode: CardMode,
    /// Whether a preview image has been fetched. Decides whether *Large* has anything more to
    /// show than *Card*, which the panel says rather than leaving the user to guess.
    pub has_image: bool,
}

/// The connector properties of one item.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnectorSummary {
    pub routing: RoutingMode,
    pub start_arrow: Arrowhead,
    pub end_arrow: Arrowhead,
    /// Where each end attaches. See [`AnchorSide`].
    pub start_anchor: AnchorSide,
    pub end_anchor: AnchorSide,
}

/// Another agent this one can hand work to.
///
/// **Reachability is the app's answer, not ours.** A hand-off is only legal along a
/// connector the user drew (`docs/07-agent-canvas.md` §3), and the chrome cannot see the
/// board's connectors — so the app resolves the list and the pickers offer nothing else.
/// That is what makes the schedule editor able to refuse an unreachable target *when the
/// schedule is saved* rather than silently at six in the evening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLink {
    /// The node's item id **as a string**, because that is how `vellum-agent` spells one:
    /// [`vellum_agent::Completion::HandOff`] stores exactly this, and converting on the way
    /// in and out is two places for the two spellings to disagree.
    pub id: String,
    /// What to show — the node's role label, falling back to its kind.
    pub label: String,
}

/// Whether an agent has a git worktree of its own, and where it is.
///
/// Three states rather than a `bool` plus an `Option<String>`, because the pair has a
/// fourth combination — *no worktree, but here is its path* — that means nothing and would
/// have to be handled anyway. Feature 4's toggle is per project; this is what one node
/// ended up with, which is why turning the project setting off cannot orphan it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WorktreeState {
    /// Works in the project directory itself.
    #[default]
    Off,
    /// Asked for; not created yet.
    Pending,
    /// Created, at this path.
    At(String),
    /// The switch is off and a checkout is still on disk at this path.
    ///
    /// The fourth combination, which used to be dismissed as meaningless — and it is the one
    /// state in which a worktree most needs removing. Turning the per-node switch off does
    /// not delete a directory (a checkout is not something a toggle may throw away), so
    /// without this state the recorded path became invisible the moment the switch flipped
    /// and *Remove worktree...* greyed itself out over a directory that was still there.
    Orphaned(String),
}

impl WorktreeState {
    pub const fn is_on(&self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn label(&self) -> String {
        match self {
            Self::Off => "Works in the project directory".to_owned(),
            Self::Orphaned(path) => format!("Switched off; its checkout is still at {path}"),
            Self::Pending => "Requested — created on the first run".to_owned(),
            Self::At(path) => path.clone(),
        }
    }
}

/// Everything the inspector needs about one agent node.
///
/// Config only, exactly as [`vellum_agent::AgentModel`] is: nothing the agent produced
/// reaches the chrome, because nothing it produced reaches the document either
/// (`docs/07-agent-canvas.md` §4). The three fields that are *not* on the model —
/// [`Self::rules`], [`Self::connected`] and [`Self::running`] — are the app's answers to
/// questions this crate cannot ask, since resolving a rule cascade reads files and
/// resolving reachability reads the board.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSummary {
    /// The free-text role label — feature 5. Empty for a node nobody has named.
    pub role: String,
    pub role_kind: RoleKind,
    /// `None` inherits the board's default, which is **not** the same statement as naming
    /// the same provider explicitly: an inherited node follows when the default moves.
    pub provider: Option<ProviderChoice>,
    /// What inheriting currently resolves to, so a row reading *Inherit* can say what it
    /// inherits without the reader opening Preferences.
    pub inherited_provider: ProviderChoice,
    /// `None` inherits the app-wide default — feature 2's second half.
    pub display: Option<DisplayMode>,
    pub inherited_display: DisplayMode,
    /// This node's own chat theme, or `None` when it inherits the app-wide default.
    ///
    /// The **stored** value, not the resolved one: a tick has to tell *"this node is set to
    /// Claude"* from *"this node inherits, and the default happens to be Claude"*, and only
    /// the stored value can.
    pub chat_theme: Option<vellum_agent::ChatTheme>,
    /// How see-through its paper is, 0-255, resolved so the slider always has a value.
    pub chat_opacity: u8,
    /// Whether it already has a picture behind it, so *No picture* is offered only when
    /// there is one to take off.
    pub has_chat_background: bool,
    /// `None` is the board's project root.
    pub working_dir: Option<String>,
    /// What that root is, for the same reason [`Self::inherited_provider`] is here.
    pub project_dir: Option<String>,
    pub worktree: WorktreeState,
    /// `None` is an agent that runs only when asked.
    pub schedule: Option<Schedule>,
    /// An orchestrator's region. Meaningless for a worker and kept anyway, so switching a
    /// node's role and switching it back does not lose the rectangle the user drew.
    pub territory: Option<Territory>,
    /// The cap actually in force — [`vellum_agent::AgentModel::effective_spawn_cap`], so the
    /// panel never has to resolve `None` and cannot resolve it differently.
    pub spawn_cap: u32,
    pub context: Vec<ContextSource>,
    pub running: bool,
    /// What this agent was actually resolved with, **including where each part came from**.
    ///
    /// The rules section reads its provenance off this rather than comparing the three
    /// layers itself, which is the only arrangement in which the display cannot disagree
    /// with what the agent got — `docs/07-agent-canvas.md` §7 states it, and this repo has
    /// paid for a second derivation twice.
    pub rules: ResolvedRules,
    /// The node's own layer, for the editor to write back into.
    pub own_rules: AgentRules,
    /// Agents reachable along a connector. See [`AgentLink`].
    pub connected: Vec<AgentLink>,
    pub accepts_messages: bool,
    pub voice: bool,
    /// Whether this **build** can record at all — `vellum-agent`'s `voice` feature, forwarded
    /// through `vellum-app` and off by default.
    ///
    /// Reported rather than assumed, and separate from [`Self::voice`] for
    /// [`BrowserSummary::allowed`]'s reason: one is a capability the binary either has or has
    /// not, the other is an instruction this node was given. Without the split a default
    /// build would either hide the control — leaving the user with no way to find out why
    /// push-to-talk is missing — or offer one that does nothing.
    pub voice_available: bool,
}

impl AgentSummary {
    /// The provider actually used, resolving *inherit* through the board's default.
    pub fn effective_provider(&self) -> ProviderChoice {
        self.provider.clone().unwrap_or_else(|| self.inherited_provider.clone())
    }

    /// The display mode actually used.
    pub const fn effective_display(&self) -> DisplayMode {
        match self.display {
            Some(mode) => mode,
            None => self.inherited_display,
        }
    }

    /// The provider row's one line: which model, and **who pays**.
    ///
    /// [`ProviderChoice::summary`] already ends in *subscription* or *API*, which is the
    /// single most useful thing to know before starting a long run, so this adds only
    /// whether the choice was inherited. Composing the billing word here instead would be a
    /// second derivation of the one fact this row exists to state.
    pub fn provider_summary(&self) -> String {
        let summary = self.effective_provider().summary();
        if self.provider.is_some() { summary } else { format!("{summary} · inherited") }
    }

    /// Whether this role has a territory and a cap at all. A worker has neither: it has
    /// nothing to spawn into, so the rows would be controls over a value nothing reads.
    pub const fn manages(&self) -> bool {
        self.role_kind.may_spawn()
    }
}

/// A note node: which file, whose, and whether the disk and the board have diverged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSummary {
    /// The `.md` file, as stored — relative to the project where there is one.
    pub path: String,
    pub scope: NoteScope,
    /// The owning agent's *label*, for a private note. The scope carries an item id, and a
    /// row that prints `42@7` at somebody has told them nothing.
    pub owner: Option<String>,
    /// Changed on disk **and** on the board. The node says so and both are kept — see
    /// `docs/07-agent-canvas.md` §8.
    pub conflicted: bool,
    /// How many sibling notes it links to.
    pub links: usize,
    /// Whether the file exists yet. A note that has never been written is not an error;
    /// it is a note nobody has typed in.
    pub on_disk: bool,
    /// The note's own title, as it is on the board.
    ///
    /// Here so the panel can *offer* a file name rather than asking for one: a note called
    /// "Engine bay" proposes `engine-bay.md` through [`vellum_agent::notes::slug`], which is
    /// the same function the store would use. Naming it here and slugging it there would be
    /// two answers to what the file is called, and the user would watch the name they
    /// accepted turn into a different one.
    pub title: String,
    /// The agents this note is joined to by a connector, by id and label.
    ///
    /// This is what makes *private* reachable. A private note belongs to exactly one agent,
    /// which agent that is is a fact about the **board** — the line the user drew — and the
    /// panel cannot walk connectors. Empty means the honest answer is *connect it to an
    /// agent first*, which is a gesture that exists.
    pub connected: Vec<AgentLink>,
}

impl NoteSummary {
    pub const fn is_private(&self) -> bool {
        self.scope.is_private()
    }

    /// The file name this note would get, if one were made for it now.
    ///
    /// [`vellum_agent::notes::slug`]'s answer, never a second spelling of it. The store
    /// appends `-2` for a collision (`NoteStore::free_stem`) and this cannot know about
    /// files, so it is a *proposal* — which is exactly what a hint in a text field is.
    pub fn proposed_stem(&self) -> String {
        vellum_agent::notes::slug(&self.title)
    }
}

/// A file-tree node: which directory, and whose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeSummary {
    /// Empty means the board's project root.
    pub root: String,
    /// What that root resolves to, so a tree showing the project can say *which* project
    /// rather than leaving the reader to guess. `None` when the board has no folder.
    pub project_dir: Option<String>,
    /// The agent this tree is scoped to, by label. Feature 7's hard requirement.
    pub agent: Option<String>,
    /// The same agent by **item id**, which is what a write has to carry: two nodes may
    /// share a label and only one of them owns this tree. The label is what is shown, the
    /// id is what is emitted — the arrangement a private note's owner already uses.
    pub agent_id: Option<String>,
    /// The agents this tree is joined to by a connector, by id and label. What the owner
    /// picker offers; empty means *draw a line to an agent first*.
    pub connected: Vec<AgentLink>,
    pub show_ignored: bool,
}

/// A browser node: which page, and whether it may run an engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSummary {
    pub url: String,
    /// The page's title once known, so the node reads as something before — or without —
    /// an engine ever being instantiated.
    pub title: String,
    /// Whether *this page* has been asked to load.
    pub live: bool,
    /// Whether browser nodes are permitted at all, app-wide. Both are needed, and they are
    /// different statements: one is a permission and the other is an instruction. Without
    /// the split, turning the preference on would load every browser node on every board.
    pub allowed: bool,
}

/// One selected item, as the app describes it to the chrome.
///
/// A flattened view rather than a document handle: the chrome never reads the CRDT,
/// so a panel can be rendered from a test fixture and the document can change shape
/// without the UI following it.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionItem {
    pub id: ItemId,
    pub facet: ItemFacet,
    pub placement: Placement,
    pub locked: bool,
    /// `None` for kinds with no fill at all; `Some(None)` for "no fill", which is a
    /// choice a shape can make and a sticky cannot.
    pub fill: Option<Option<Color>>,
    pub border: Option<Border>,
    /// Whole-item opacity, 0.0–1.0.
    pub opacity: Option<f64>,
    pub text: Option<TextSummary>,
    pub connector: Option<ConnectorSummary>,
    /// Present for a link or embed card.
    pub link: Option<LinkSummary>,
    /// Present for an agent node — worker, orchestrator or meta.
    pub agent: Option<AgentSummary>,
    pub note: Option<NoteSummary>,
    pub file_tree: Option<FileTreeSummary>,
    pub browser: Option<BrowserSummary>,
}

impl SelectionItem {
    /// A minimal item, for tests and for kinds whose properties the app has not
    /// filled in yet.
    pub fn new(id: ItemId, facet: ItemFacet, placement: Placement) -> Self {
        Self {
            id,
            facet,
            placement,
            locked: false,
            fill: None,
            border: None,
            opacity: None,
            text: None,
            connector: None,
            link: None,
            agent: None,
            note: None,
            file_tree: None,
            browser: None,
        }
    }
}

/// An axis-aligned rectangle in world units, as shown in the position fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The properties panel's contents for a given selection.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelModel {
    pub count: usize,
    /// "Sticky note", "3 shapes", "4 objects".
    pub headline: String,
    pub fill: Field<Option<Color>>,
    pub border_color: Field<Color>,
    pub border_width: Field<f64>,
    pub border_style: Field<LineStyle>,
    pub opacity: Field<f64>,
    pub font_family: Field<Option<String>>,
    pub font_size: Field<Option<f64>>,
    pub font_weight: Field<FontWeight>,
    pub text_color: Field<Color>,
    pub align: Field<Align>,
    pub vertical_align: Field<VerticalAlign>,
    pub line_height: Field<f64>,
    pub routing: Field<RoutingMode>,
    pub start_arrow: Field<Arrowhead>,
    pub end_arrow: Field<Arrowhead>,
    pub start_anchor: Field<AnchorSide>,
    pub end_anchor: Field<AnchorSide>,
    /// The display mode of every selected card, folded. Absent when nothing selected is one.
    pub card_mode: Field<CardMode>,
    /// The one selected card's URL, when exactly one card is selected. Single-selection only,
    /// because *Open* acts on one page and opening forty tabs is not a thing anybody means.
    pub link_url: Option<String>,
    /// Whether every selected card already has a preview image.
    pub link_has_image: Field<bool>,

    // ----- the agent family -------------------------------------------------
    //
    // Folded like every other property, so *which* controls an agent node gets is decided
    // by the properties it has and never by a `match` on [`ItemFacet::Agent`]. Each of
    // these four is `Absent` for everything that is not one of its kind, which makes it the
    // honest test for "this selection is an agent" — the same shape `card_mode` already has
    // for a link card. A fifth node kind arriving with no bar is what a kind-match buys.
    /// Whether every selected agent is running.
    pub agent_running: Field<bool>,
    pub agent_role_kind: Field<RoleKind>,
    /// The **stored** choice, so `Uniform(None)` is "every one of them inherits" and is
    /// genuinely distinct from every one of them naming the same mode.
    pub agent_display: Field<Option<DisplayMode>>,
    /// Likewise stored rather than resolved. See [`AgentSummary::provider`].
    pub agent_provider: Field<Option<ProviderChoice>>,
    /// The one selected agent, when exactly one is. The rules, the schedule, the territory
    /// and the context list are all single-selection: none of them folds into a meaningful
    /// shared value, and offering to write one schedule into forty nodes is the shape of
    /// edit that loses forty configurations at once.
    pub agent: Option<AgentSummary>,
    /// Whether every selected note is private.
    pub note_private: Field<bool>,
    pub note: Option<NoteSummary>,
    pub tree_show_ignored: Field<bool>,
    pub file_tree: Option<FileTreeSummary>,
    /// Whether every selected browser node is loading its page.
    pub browser_live: Field<bool>,
    pub browser: Option<BrowserSummary>,
    /// Absent only when nothing is selected — every item has a lock state.
    pub locked: Field<bool>,
    pub rotation: Field<f64>,
    /// The selection's bounding box, or `None` when nothing is selected.
    pub bounds: Option<Bounds>,
    /// Whether width and height may be typed into.
    ///
    /// False for a multi-selection: a single number cannot describe several
    /// differently-sized items, and silently resizing them all to it is a data-loss
    /// gesture disguised as a text field. Position stays editable because moving a
    /// group by its bounding box has one obvious meaning.
    pub size_editable: bool,
    /// The one selected item's id, when exactly one is selected. What
    /// [`Self::text_content`] belongs to.
    pub single_id: Option<ItemId>,
    /// The words of the one selected item, when exactly one is selected and it holds
    /// text at all.
    ///
    /// Single-selection only, and that is the whole rule rather than a `Field`:
    /// typing one paragraph into forty stickies at once is not an edit anybody makes
    /// on purpose, and offering it is how you lose forty notes with one keystroke.
    pub text_content: Option<String>,
}

impl PanelModel {
    pub fn empty() -> Self {
        Self::derive(&[])
    }

    /// Folds a selection into everything the panel needs.
    pub fn derive(selection: &[SelectionItem]) -> Self {
        let text = || selection.iter().filter_map(|i| i.text.as_ref());
        let border = || selection.iter().filter_map(|i| i.border.as_ref());
        let connector = || selection.iter().filter_map(|i| i.connector.as_ref());
        let link = || selection.iter().filter_map(|i| i.link.as_ref());
        let agent = || selection.iter().filter_map(|i| i.agent.as_ref());
        let note = || selection.iter().filter_map(|i| i.note.as_ref());
        let tree = || selection.iter().filter_map(|i| i.file_tree.as_ref());
        let browser = || selection.iter().filter_map(|i| i.browser.as_ref());

        Self {
            count: selection.len(),
            headline: headline(selection),
            fill: Field::collect(selection.iter().filter_map(|i| i.fill)),
            border_color: Field::collect(border().map(|b| b.color)),
            border_width: Field::collect(border().map(|b| b.width)),
            border_style: Field::collect(border().map(|b| b.style)),
            opacity: Field::collect(selection.iter().filter_map(|i| i.opacity)),
            font_family: Field::collect(text().map(|t| t.family.clone())),
            font_size: Field::collect(text().map(|t| t.size)),
            font_weight: Field::collect(text().map(|t| t.weight)),
            text_color: Field::collect(text().map(|t| t.color)),
            align: Field::collect(text().map(|t| t.align)),
            vertical_align: Field::collect(text().map(|t| t.vertical_align)),
            line_height: Field::collect(text().map(|t| t.line_height)),
            routing: Field::collect(connector().map(|c| c.routing)),
            start_arrow: Field::collect(connector().map(|c| c.start_arrow)),
            end_arrow: Field::collect(connector().map(|c| c.end_arrow)),
            start_anchor: Field::collect(connector().map(|c| c.start_anchor)),
            end_anchor: Field::collect(connector().map(|c| c.end_anchor)),
            card_mode: Field::collect(link().map(|l| l.mode)),
            link_url: match selection {
                [only] => only.link.as_ref().and_then(|l| l.url.clone()),
                _ => None,
            },
            link_has_image: Field::collect(link().map(|l| l.has_image)),
            agent_running: Field::collect(agent().map(|a| a.running)),
            agent_role_kind: Field::collect(agent().map(|a| a.role_kind)),
            agent_display: Field::collect(agent().map(|a| a.display)),
            agent_provider: Field::collect(agent().map(|a| a.provider.clone())),
            agent: match selection {
                [only] => only.agent.clone(),
                _ => None,
            },
            note_private: Field::collect(note().map(NoteSummary::is_private)),
            note: match selection {
                [only] => only.note.clone(),
                _ => None,
            },
            tree_show_ignored: Field::collect(tree().map(|t| t.show_ignored)),
            file_tree: match selection {
                [only] => only.file_tree.clone(),
                _ => None,
            },
            browser_live: Field::collect(browser().map(|b| b.live)),
            browser: match selection {
                [only] => only.browser.clone(),
                _ => None,
            },
            locked: Field::collect(selection.iter().map(|i| i.locked)),
            rotation: Field::collect(selection.iter().map(|i| i.placement.rotation)),
            bounds: bounding_box(selection),
            size_editable: selection.len() == 1,
            single_id: match selection {
                [one] => Some(one.id),
                _ => None,
            },
            text_content: match selection {
                [one] => one.text.as_ref().map(|t| t.content.clone()),
                _ => None,
            },
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether the fill / border / opacity block has anything to show.
    pub const fn has_appearance(&self) -> bool {
        !(self.fill.is_absent()
            && self.border_color.is_absent()
            && self.opacity.is_absent())
    }

    /// Whether the typography block has anything to show.
    pub const fn has_text(&self) -> bool {
        !self.font_family.is_absent()
    }

    /// Whether the selection holds a link or embed card.
    pub const fn has_link(&self) -> bool {
        !self.card_mode.is_absent()
    }

    pub const fn has_connector(&self) -> bool {
        !self.routing.is_absent()
    }

    /// Whether the selection holds an agent node — worker, orchestrator or meta.
    ///
    /// Asked of the *properties* an agent has rather than of [`ItemFacet::Agent`], for the
    /// reason [`Self::agent_running`] records: a facet match is a decision that has to be
    /// repeated at every control, and the one that gets forgotten draws a blank bar.
    pub const fn has_agent(&self) -> bool {
        !self.agent_running.is_absent()
    }

    pub const fn has_note(&self) -> bool {
        !self.note_private.is_absent()
    }

    pub const fn has_file_tree(&self) -> bool {
        !self.tree_show_ignored.is_absent()
    }

    pub const fn has_browser(&self) -> bool {
        !self.browser_live.is_absent()
    }

    /// Whether anything in the selection belongs to the Agent Canvas layer.
    ///
    /// What the inspector's agent section and the context bar's agent band both ask, so a
    /// board with none of these is byte-for-byte the interface it was — `docs/07-agent-canvas.md`
    /// §0's second rule, applied to the chrome.
    pub const fn has_agent_family(&self) -> bool {
        self.has_agent() || self.has_note() || self.has_file_tree() || self.has_browser()
    }

    /// The one selected browser node's address, when exactly one is selected and it has one.
    ///
    /// Single-selection for the reason [`Self::link_url`] is: *open* acts on one page.
    pub fn browser_url(&self) -> Option<&str> {
        self.browser.as_ref().map(|b| b.url.as_str()).filter(|url| !url.is_empty())
    }

    /// The one selected note's file, when exactly one is selected.
    pub fn note_path(&self) -> Option<&str> {
        self.note.as_ref().map(|n| n.path.as_str()).filter(|path| !path.is_empty())
    }

    /// True when every selected item is locked, which is what flips the panel's lock
    /// control and the Object menu's Lock/Unlock pair.
    pub fn all_locked(&self) -> bool {
        matches!(self.locked, Field::Uniform(true))
    }

    pub fn any_locked(&self) -> bool {
        matches!(self.locked, Field::Uniform(true) | Field::Mixed)
    }
}

/// "Sticky note" for one, "3 sticky notes" when they agree, "4 objects" when they
/// do not — the same escalation Miro uses, and the reason the panel header is worth
/// deriving rather than hard-coding to "Selection".
fn headline(selection: &[SelectionItem]) -> String {
    match selection {
        [] => "Nothing selected".to_owned(),
        [one] => one.facet.noun().to_owned(),
        many => match Field::collect(many.iter().map(|i| i.facet)) {
            // Lower-cased because the count now leads the phrase: "2 sticky notes",
            // not "2 Sticky notes".
            Field::Uniform(facet) => {
                format!("{} {}", many.len(), plural(facet.noun()).to_lowercase())
            }
            _ => format!("{} objects", many.len()),
        },
    }
}

/// English plurals for the nine nouns in [`ItemFacet`]. None of them is irregular,
/// so a suffix rule is enough and a table would only be a place for them to drift.
fn plural(noun: &str) -> String {
    if noun.ends_with('s') || noun.ends_with("ch") || noun.ends_with('x') {
        format!("{noun}es")
    } else {
        format!("{noun}s")
    }
}

/// The union of every item's placement, ignoring rotation.
///
/// Rotation is deliberately not accounted for: the numeric fields edit an item's
/// *stored* x/y/width/height, and expanding the box to cover a rotated item's corners
/// would mean typing a width that the item does not then have.
fn bounding_box(selection: &[SelectionItem]) -> Option<Bounds> {
    let mut items = selection.iter();
    let first = items.next()?;
    let (w, h) = first.placement.scaled_size();
    let (mut min_x, mut min_y) = (first.placement.x, first.placement.y);
    let (mut max_x, mut max_y) = (min_x + w, min_y + h);

    for item in items {
        let (w, h) = item.placement.scaled_size();
        min_x = min_x.min(item.placement.x);
        min_y = min_y.min(item.placement.y);
        max_x = max_x.max(item.placement.x + w);
        max_y = max_y.max(item.placement.y + h);
    }

    Some(Bounds { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: i32) -> ItemId {
        format!("{n}@1").parse().expect("well-formed item id")
    }

    fn sticky(n: i32, fill: Color) -> SelectionItem {
        SelectionItem {
            fill: Some(Some(fill)),
            opacity: Some(1.0),
            text: Some(TextSummary {
                content: "radiator fan".to_owned(),
                family: Some("Noto Sans".to_owned()),
                size: Some(14.0),
                weight: FontWeight::Regular,
                color: Color::rgb(0, 0, 0),
                align: Align::Center,
                vertical_align: VerticalAlign::Middle,
                line_height: 1.2,
            }),
            ..SelectionItem::new(
                id(n),
                ItemFacet::Sticky,
                Placement::new(f64::from(n) * 100.0, 0.0, 200.0, 200.0),
            )
        }
    }

    fn ink(n: i32) -> SelectionItem {
        SelectionItem::new(id(n), ItemFacet::Ink, Placement::new(0.0, 0.0, 50.0, 50.0))
    }

    #[test]
    fn an_empty_selection_has_no_bounds_and_no_sections() {
        let model = PanelModel::empty();
        assert!(model.is_empty());
        assert_eq!(model.headline, "Nothing selected");
        assert!(model.bounds.is_none());
        assert!(!model.has_appearance());
        assert!(!model.has_text());
        assert!(!model.has_connector());
        assert!(model.locked.is_absent());
    }

    #[test]
    fn a_single_item_reports_every_value_as_uniform() {
        let yellow = Color::rgb(0xFF, 0xF7, 0x9E);
        let model = PanelModel::derive(&[sticky(1, yellow)]);
        assert_eq!(model.headline, "Sticky note");
        assert_eq!(model.fill, Field::Uniform(Some(yellow)));
        assert_eq!(model.align, Field::Uniform(Align::Center));
        assert!(model.size_editable);
    }

    #[test]
    fn disagreeing_values_read_as_mixed_while_agreeing_ones_do_not() {
        let model = PanelModel::derive(&[
            sticky(1, Color::rgb(0xFF, 0xF7, 0x9E)),
            sticky(2, Color::rgb(0xFF, 0x9E, 0x9E)),
        ]);
        assert!(model.fill.is_mixed());
        assert_eq!(model.font_size, Field::Uniform(Some(14.0)));
        assert_eq!(model.headline, "2 sticky notes");
    }

    /// The rule that makes mixed selections usable: an object without a property
    /// abstains rather than voting against.
    #[test]
    fn items_lacking_a_property_do_not_make_it_mixed() {
        let yellow = Color::rgb(0xFF, 0xF7, 0x9E);
        let model = PanelModel::derive(&[sticky(1, yellow), ink(2)]);
        assert_eq!(model.fill, Field::Uniform(Some(yellow)));
        assert!(model.has_text(), "the sticky still contributes typography");
        assert_eq!(model.headline, "2 objects");
    }

    #[test]
    fn a_selection_of_only_ink_offers_no_fill_and_no_typography() {
        let model = PanelModel::derive(&[ink(1), ink(2)]);
        assert!(model.fill.is_absent());
        assert!(!model.has_text());
        assert_eq!(model.headline, "2 drawings");
    }

    #[test]
    fn no_fill_is_a_value_that_can_be_shared_rather_than_an_absence() {
        let mut a = sticky(1, Color::rgb(0, 0, 0));
        let mut b = sticky(2, Color::rgb(0, 0, 0));
        a.fill = Some(None);
        b.fill = Some(None);
        let model = PanelModel::derive(&[a, b]);
        assert_eq!(model.fill, Field::Uniform(None), "both agree on 'no fill'");
        assert!(model.has_appearance());
    }

    #[test]
    fn bounds_are_the_union_and_size_is_locked_for_a_multi_selection() {
        let yellow = Color::rgb(0xFF, 0xF7, 0x9E);
        let model = PanelModel::derive(&[sticky(1, yellow), sticky(3, yellow)]);
        let bounds = model.bounds.expect("two items have bounds");
        assert_eq!(bounds.x, 100.0);
        assert_eq!(bounds.width, 400.0, "100..500");
        assert_eq!(bounds.height, 200.0);
        assert!(!model.size_editable);
    }

    /// `Placement::scale` multiplies the stored size, so the box has to use the
    /// scaled one or a scaled item hangs outside its own selection rectangle.
    #[test]
    fn bounds_account_for_placement_scale() {
        let mut item = sticky(0, Color::rgb(0, 0, 0));
        item.placement.scale = 2.0;
        let bounds = PanelModel::derive(&[item]).bounds.unwrap();
        assert_eq!(bounds.width, 400.0);
    }

    #[test]
    fn lock_state_drives_the_all_locked_flag() {
        let mut a = sticky(1, Color::rgb(0, 0, 0));
        let mut b = sticky(2, Color::rgb(0, 0, 0));
        assert!(!PanelModel::derive(&[a.clone(), b.clone()]).all_locked());

        a.locked = true;
        let half = PanelModel::derive(&[a.clone(), b.clone()]);
        assert!(!half.all_locked());
        assert!(half.any_locked());
        assert!(half.locked.is_mixed());

        b.locked = true;
        assert!(PanelModel::derive(&[a, b]).all_locked());
    }

    #[test]
    fn connector_properties_appear_only_for_connectors() {
        let mut wire = SelectionItem::new(
            id(9),
            ItemFacet::Connector,
            Placement::new(0.0, 0.0, 10.0, 10.0),
        );
        wire.connector = Some(ConnectorSummary {
            routing: RoutingMode::Straight,
            start_arrow: Arrowhead::None,
            end_arrow: Arrowhead::FilledTriangle,
            start_anchor: AnchorSide::Right,
            end_anchor: AnchorSide::Left,
        });
        wire.border = Some(Border {
            color: Color::rgb(0x33, 0x33, 0x33),
            width: 2.0,
            style: LineStyle::Solid,
        });

        let model = PanelModel::derive(&[wire]);
        assert!(model.has_connector());
        assert_eq!(model.end_arrow, Field::Uniform(Arrowhead::FilledTriangle));
        assert_eq!(model.border_width, Field::Uniform(2.0));
        assert!(!model.has_text());
        // The anchor picker's two rows come through the same fold as the arrowheads.
        assert_eq!(model.start_anchor, Field::Uniform(AnchorSide::Right));
        assert_eq!(model.end_anchor, Field::Uniform(AnchorSide::Left));

        let drawing = ink(1);
        let drawing = std::slice::from_ref(&drawing);
        assert!(!PanelModel::derive(drawing).has_connector());
        assert!(
            PanelModel::derive(drawing).start_anchor.is_absent(),
            "a drawing has no ends to attach, so the rows are not drawn at all"
        );
    }

    #[test]
    fn field_falls_back_only_when_it_has_no_agreed_value() {
        assert_eq!(Field::Uniform(3_u8).or(9), 3);
        assert_eq!(Field::<u8>::Mixed.or(9), 9);
        assert_eq!(Field::<u8>::Absent.or(9), 9);
    }

    #[test]
    fn font_weights_map_onto_the_numbers_a_font_query_needs() {
        assert_eq!(
            FontWeight::ALL.map(FontWeight::css),
            [400, 500, 700],
            "the panel offers three weights, in order"
        );
        assert!(FontWeight::ALL.iter().all(|w| !w.label().is_empty()));
        assert_eq!(FontWeight::default(), FontWeight::Regular);
    }

    /// A worker with everything inherited, which is what the agent tool places.
    fn agent_summary(role: &str) -> AgentSummary {
        AgentSummary {
            chat_theme: None,
            chat_opacity: u8::MAX,
            has_chat_background: false,
            role: role.to_owned(),
            role_kind: RoleKind::Worker,
            provider: None,
            inherited_provider: ProviderChoice::new(vellum_agent::Provider::Claude),
            display: None,
            inherited_display: DisplayMode::Clean,
            working_dir: None,
            project_dir: Some("/tmp/project".to_owned()),
            worktree: WorktreeState::Off,
            schedule: None,
            territory: None,
            spawn_cap: 0,
            context: Vec::new(),
            running: false,
            rules: vellum_agent::rules::resolve(
                &vellum_agent::RuleFile::default(),
                &vellum_agent::RuleFile::default(),
                &AgentRules::default(),
                role,
            ),
            own_rules: AgentRules::default(),
            connected: Vec::new(),
            accepts_messages: true,
            voice: false,
            voice_available: false,
        }
    }

    fn agent(n: i32, role: &str) -> SelectionItem {
        SelectionItem {
            agent: Some(agent_summary(role)),
            opacity: Some(1.0),
            ..SelectionItem::new(
                id(n),
                ItemFacet::Agent,
                Placement::new(0.0, 0.0, 400.0, 300.0),
            )
        }
    }

    /// The agent rows appear because the selection *has* agent properties, and for no other
    /// reason. A sticky beside an agent must not acquire a provider, and an agent must not
    /// acquire a sticky's fill.
    #[test]
    fn agent_properties_are_present_only_where_an_agent_is() {
        let one = [agent(1, "Reviewer")];
        let model = PanelModel::derive(&one);
        assert!(model.has_agent());
        assert!(model.has_agent_family());
        assert_eq!(model.agent_running, Field::Uniform(false));
        assert_eq!(model.agent_role_kind, Field::Uniform(RoleKind::Worker));
        assert_eq!(model.agent.as_ref().map(|a| a.role.as_str()), Some("Reviewer"));

        let plain = [sticky(1, Color::rgb(0, 0, 0))];
        let model = PanelModel::derive(&plain);
        assert!(!model.has_agent(), "a sticky is not an agent");
        assert!(!model.has_agent_family());
        assert!(model.agent.is_none());
    }

    /// *Inherit* is a value, not an absence — feature 2's second half, and the distinction
    /// the whole display control exists to make. Two nodes that both inherit **agree**; a
    /// node that names Clean and a node that inherits a Clean default do not, because
    /// changing the default moves one of them and not the other.
    #[test]
    fn inheriting_a_display_mode_is_not_the_same_as_naming_it() {
        let mut inherits = agent(1, "A");
        let mut names = agent(2, "B");
        names.agent.as_mut().expect("an agent").display = Some(DisplayMode::Clean);

        let both_inherit = PanelModel::derive(&[inherits.clone(), agent(3, "C")]);
        assert_eq!(both_inherit.agent_display, Field::Uniform(None));

        let mixed = PanelModel::derive(&[inherits.clone(), names.clone()]);
        assert!(
            mixed.agent_display.is_mixed(),
            "one inherits and one has chosen; folding those together would hide the choice"
        );

        inherits.agent.as_mut().expect("an agent").display = Some(DisplayMode::Clean);
        let agreed = PanelModel::derive(&[inherits, names]);
        assert_eq!(agreed.agent_display, Field::Uniform(Some(DisplayMode::Clean)));
    }

    /// The rules, the schedule and the context list are single-selection, because none of
    /// them folds into a value that would mean anything shared.
    #[test]
    fn the_deep_agent_configuration_is_single_selection_only() {
        let many = PanelModel::derive(&[agent(1, "A"), agent(2, "B")]);
        assert!(many.has_agent(), "the folded properties still apply to both");
        assert!(
            many.agent.is_none(),
            "two agents have two rule cascades and two schedules; there is no shared one"
        );
    }

    /// A note, a file tree and a browser node are three different things and none of them
    /// is an agent. Each carries its own controls and none carries another's.
    #[test]
    fn the_three_companion_kinds_each_report_only_their_own_properties() {
        let mut note = SelectionItem::new(id(1), ItemFacet::Note, Placement::new(0.0, 0.0, 1.0, 1.0));
        note.note = Some(NoteSummary {
            path: ".velm/notes/plan.md".to_owned(),
            scope: NoteScope::Private { agent: "1@2".to_owned() },
            owner: Some("Reviewer".to_owned()),
            conflicted: false,
            links: 2,
            on_disk: true,
            title: "Plan".to_owned(),
            connected: Vec::new(),
        });
        let model = PanelModel::derive(std::slice::from_ref(&note));
        assert!(model.has_note() && model.has_agent_family());
        assert!(!model.has_agent() && !model.has_browser() && !model.has_file_tree());
        assert_eq!(model.note_private, Field::Uniform(true));
        assert_eq!(model.note_path(), Some(".velm/notes/plan.md"));

        let mut tree =
            SelectionItem::new(id(2), ItemFacet::FileTree, Placement::new(0.0, 0.0, 1.0, 1.0));
        tree.file_tree = Some(FileTreeSummary {
            root: "crates".to_owned(),
            project_dir: None,
            agent: Some("Reviewer".to_owned()),
            agent_id: Some("9@1".to_owned()),
            connected: Vec::new(),
            show_ignored: false,
        });
        let model = PanelModel::derive(std::slice::from_ref(&tree));
        assert!(model.has_file_tree() && !model.has_note());

        let mut browser =
            SelectionItem::new(id(3), ItemFacet::Browser, Placement::new(0.0, 0.0, 1.0, 1.0));
        browser.browser = Some(BrowserSummary {
            url: "https://example.test/".to_owned(),
            title: String::new(),
            live: false,
            allowed: false,
        });
        let model = PanelModel::derive(std::slice::from_ref(&browser));
        assert!(model.has_browser() && !model.has_agent());
        assert_eq!(model.browser_url(), Some("https://example.test/"));
        assert_eq!(model.browser_live, Field::Uniform(false));
    }

    /// The provider row's job is to say **who pays**, and it must not compose that word
    /// itself — `ProviderChoice::summary` already decides it, and a second derivation is
    /// how a subscription node comes to be labelled as billed.
    #[test]
    fn the_provider_line_names_the_billing_and_says_when_it_was_inherited() {
        let mut summary = agent_summary("Reviewer");
        assert!(
            summary.provider_summary().ends_with("· inherited"),
            "{}",
            summary.provider_summary()
        );
        assert!(summary.provider_summary().contains("subscription"));

        summary.provider = Some(ProviderChoice::new(vellum_agent::Provider::Kimi));
        let named = summary.provider_summary();
        assert!(!named.contains("inherited"), "{named}");
        assert!(named.ends_with("API"), "Kimi has no CLI to delegate to: {named}");
    }

    #[test]
    fn plurals_cover_every_facet_noun() {
        assert_eq!(plural("Sticky note"), "Sticky notes");
        assert_eq!(plural("Text"), "Texts");
        assert_eq!(plural("Box"), "Boxes");
        assert_eq!(plural("Sketch"), "Sketches");
    }
}
