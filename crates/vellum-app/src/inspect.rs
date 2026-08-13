//! The properties panel, in both directions.
//!
//! Out: a selection of document items becomes the flattened [`SelectionItem`]s
//! `vellum-ui` folds into a panel, including the mixed-value rule that shows *Mixed*
//! where a multi-selection disagrees.
//!
//! In: the panel's [`StyleEdit`]s and [`TransformEdit`]s become document mutations,
//! grouped so that one control is one undo step and one autosave record.
//!
//! Neither direction needs a window, which is the point of having them here rather
//! than inside the frame loop: the whole round trip is an ordinary unit test.
//!
//! # What the document can and cannot carry
//!
//! `vellum-doc`'s `Style` is Miro's widget-level key set — family, size, colour,
//! alignment, line height, opacity, fill — and no more. So the panel's controls
//! divide into three:
//!
//! - **Applied.** Fill, opacity, font family, font size, text colour, alignment, line
//!   height, and — for the two kinds that are made of a stroke — colour, width and
//!   dash. Routing and arrowheads on a connector.
//! - **Shown but inert in the document.** Font *weight* and *vertical* alignment: the
//!   panel offers them, `vellum_doc::Style` has no field, and Miro carries weight
//!   inside the rich-text spans rather than on the widget. Reported by
//!   [`apply_style`] as unsupported rather than silently dropped.
//! - **Absent.** A **border** on a sticky. A frame's edge is chrome drawn from the theme
//!   rather than a property the document carries, so the border section appears only for
//!   the kinds that have an outline to edit.
//!
//! The **lock** flag used to be in that last group and no longer is:
//! `vellum_doc::Style::locked` exists, so [`describe`] reports it and [`apply_style`]
//! writes it. Reporting a constant `false` here is not a cosmetic inaccuracy — it is
//! read back by `vellum_ui::chrome` as `any_locked`/`all_locked`, which gate the
//! Object menu's Lock/Unlock pair, so a hardcoded `false` left **Unlock permanently
//! disabled** and locking an item was a one-way door.
//!
//! # Stroke is the border section
//!
//! An ink stroke and a connector have a colour, a width and a dash pattern; a sticky has
//! none of the three. Mapping the panel's border controls onto the stroke for the first
//! two, and hiding the section for the rest, is what makes the section true rather than
//! decorative — and it is why selecting a drawing and a sticky together shows one fill and
//! one stroke without either lying.
//!
//! A **shape** is the third case and it lives somewhere else, which is why
//! [`stroke_home`] exists to name the split once rather than at each of the four border
//! arms. An ink stroke and a connector *are* a stroke, so colour and width sit on
//! [`ItemKind`] beside the geometry; a shape merely *has* an outline, so its colour and
//! width are `Style::stroke` and `Style::stroke_width`, exactly like a fill. Both are
//! drawn: `draw.rs`'s shape arm reads that pair and falls back to the theme's border at a
//! hairline, so **every shape on the board already has a visible outline**. This section
//! was hidden for shapes anyway, which made the colour and the width unreachable while
//! the same panel edited the shape's fill one row above. The asymmetry was the tell.
//!
//! What a shape still cannot have is a **dash**: `Style` carries no pattern and the SDF
//! border is a single coverage band, so [`apply_style`] reports `BorderStyle` on a
//! shape-only selection as unsupported instead of dropping it.

use vellum_connect::{Anchor, AnchorSide, Arrowhead, LineStyle, RoutingMode};
use vellum_doc::{
    ArrowKind, Board, Color, ConnectorEnd, Dash, ItemId, ItemKind, Placement, Routing, Style,
};
use vellum_scene::ItemId as SceneId;
use vellum_ui::{
    Align, Border, ConnectorSummary, ItemFacet, SelectionItem, StyleEdit, TextSummary,
    TransformEdit,
};

use crate::connector;
use crate::project::Projection;

/// Flattens the current selection into what the properties panel reads.
///
/// Order follows the selection, which follows paint order, so the panel's headline
/// and its single-item case describe what the user sees on top.
pub fn selection_items(
    projection: &Projection,
    selection: &[SceneId],
    agents: &AgentFacts<'_>,
) -> Vec<SelectionItem> {
    selection
        .iter()
        .filter_map(|id| projection.get(*id))
        .map(|projected| describe(projected.doc_id, &projected.item, agents))
        .collect()
}

/// The things about an agent node that are **not in the document** and that the panel still
/// has to show.
///
/// A resolved rule cascade reads three files, "is it running" is a question for the session
/// pool, and "which agents can this one hand off to" is a walk of the connectors. None of
/// them can be answered from an [`Item`](vellum_doc::Item), and all of them are needed before
/// the chrome can draw a single agent row.
///
/// Carried as borrowed closures rather than as data because the answers are wanted for the
/// **selected** items only — usually one — while the data behind them is board-sized. Building
/// a map of every agent's resolved rules to describe the one that is selected would read every
/// rules file on the machine to draw one panel.
///
/// Every closure has a defensible answer when the app has nothing to say, so
/// [`AgentFacts::unknown`] gives a panel that is honest rather than absent — which is what
/// lets `inspect.rs`'s own tests, and any future fixture, describe an agent node without
/// standing up a session pool.
pub struct AgentFacts<'a> {
    /// The board's project directory, which is what an unset working directory means.
    pub project_dir: Option<String>,
    /// What a node that never chose a display mode resolves to.
    pub inherited_display: vellum_agent::DisplayMode,
    /// What a node that never chose a provider resolves to.
    pub inherited_provider: vellum_agent::ProviderChoice,
    /// Whether browser nodes are permitted app-wide. Distinct from a node's own `live`.
    pub browser_nodes_allowed: bool,
    /// Whether this **binary** can capture audio at all — `vellum-agent`'s `voice` feature,
    /// forwarded by `vellum-app` and off by default.
    ///
    /// A build fact rather than a preference, which is why it is here beside
    /// [`Self::browser_nodes_allowed`] rather than on the node: `cfg!(feature = "voice")` is
    /// the answer and only the crate that is compiled with the flag can read it. The panel
    /// draws the control either way and names the feature when it cannot act — a control
    /// that is simply missing tells the user nothing about why.
    pub voice_available: bool,
    /// Whether this node has a live session right now.
    pub running: &'a dyn Fn(ItemId) -> bool,
    /// The agents this one may hand off to, along a connector.
    pub connected: &'a dyn Fn(ItemId) -> Vec<vellum_ui::AgentLink>,
    /// The three-layer cascade, resolved for this node.
    pub rules: &'a dyn Fn(ItemId, &vellum_agent::AgentModel) -> vellum_agent::ResolvedRules,
    /// A readable name for an item id, for a private note's owner and a tree's agent.
    ///
    /// Without this a row prints `42@7` at somebody, which tells them nothing.
    pub label_of: &'a dyn Fn(&str) -> Option<String>,
    /// Whether a note's file exists, and whether it is in conflict.
    ///
    /// Both are questions about the filesystem rather than about the token, which is why they
    /// are asked through a closure at all. ⚠ **The second is answered `false` unconditionally
    /// today**, and the caller in `app.rs` says why: a conflict file is written by
    /// `NoteStore::save`, nothing in this application calls `save` yet, so no note on any
    /// board can be in conflict. That makes the constant *correct* rather than a stand-in —
    /// but it is correct for a reason outside this type, which is the shape of the
    /// `locked: false` trap. It has to be answered for real the day a note's body becomes
    /// editable on the canvas.
    pub note_state: &'a dyn Fn(&str) -> (bool, bool),
}

impl AgentFacts<'_> {
    /// The facts as they are when nobody has any: nothing running, nothing connected, an
    /// empty cascade, no project.
    ///
    /// Honest rather than absent — a panel drawn from these says "no rules set" and "not
    /// running", both of which are true of a board that has just been opened.
    pub fn unknown() -> Self {
        Self {
            project_dir: None,
            inherited_display: vellum_agent::DisplayMode::default(),
            inherited_provider: vellum_agent::ProviderChoice::new(
                vellum_agent::Provider::default(),
            ),
            browser_nodes_allowed: false,
            // A build fact, read from the crate that carries the flag rather than assumed:
            // `unknown()` describes an app with nothing to say, and what this binary was
            // compiled with is not one of the things it does not know.
            voice_available: cfg!(feature = "voice"),
            running: &|_| false,
            connected: &|_| Vec::new(),
            // Built through the real resolver with three empty layers rather than a
            // hand-made empty value: `resolve` is the only thing that knows what an
            // unset cascade looks like, and a second answer here could disagree with it.
            rules: &|_, _| {
                vellum_agent::rules::resolve(
                    &vellum_agent::RuleFile::default(),
                    &vellum_agent::RuleFile::default(),
                    &vellum_agent::AgentRules::default(),
                    "",
                )
            },
            label_of: &|_| None,
            note_state: &|_| (false, false),
        }
    }
}

/// One item, as the panel sees it.
pub fn describe(
    id: ItemId,
    item: &vellum_doc::Item,
    agents: &AgentFacts<'_>,
) -> SelectionItem {
    SelectionItem {
        agent: agent_of(id, item, agents),
        // Both take the item's id now, and both need it for the same reason: what a note or
        // a tree *belongs to* is the agent on the other end of a connector, and that is a
        // question about this node's place on the board rather than about its token.
        note: note_of(id, &item.kind, agents),
        file_tree: file_tree_of(id, &item.kind, agents),
        browser: browser_of(&item.kind, agents),
        id,
        facet: facet_of(&item.kind),
        placement: item.placement,
        // Read from the document, not assumed. `vellum_ui::chrome` counts these to build
        // `any_locked`/`all_locked`, and those gate Unlock — so a constant here disables
        // the only control that can undo a lock.
        locked: item.style.locked,
        fill: fill_of(item),
        border: stroke_of(item),
        opacity: Some(item.style.opacity.unwrap_or(1.0)),
        text: text_of(item),
        connector: connector_of(&item.kind),
        link: link_of(&item.kind),
    }
}

/// An agent node, for the panel's Agent section.
///
/// The **role comes from the item's own text**, not from the token — that is where it lives,
/// which is what makes it searchable and editable through the paths that already exist. A
/// node nobody has named reports an empty role rather than the placeholder it was born with,
/// so a row can offer to name it instead of pretending it is named.
fn agent_of(
    id: ItemId,
    item: &vellum_doc::Item,
    agents: &AgentFacts<'_>,
) -> Option<vellum_ui::AgentSummary> {
    let ItemKind::Agent { model, label } = &item.kind else { return None };
    let config = crate::agent::decode(model);
    let rules = (agents.rules)(id, &config);
    Some(vellum_ui::AgentSummary {
        role: label.to_plain(),
        role_kind: config.role_kind,
        provider: config.provider.clone(),
        inherited_provider: agents.inherited_provider.clone(),
        display: config.display,
        inherited_display: agents.inherited_display,
        working_dir: config.working_dir.clone(),
        project_dir: agents.project_dir.clone(),
        // Three states rather than a bool and a path, because the fourth combination —
        // "no worktree, but here is its path" — means nothing. See `WorktreeState`.
        worktree: match (config.worktree, config.worktree_path.as_deref()) {
            (false, _) => vellum_ui::WorktreeState::Off,
            (true, None) => vellum_ui::WorktreeState::Pending,
            (true, Some(path)) => vellum_ui::WorktreeState::At(path.to_owned()),
        },
        schedule: config.schedule.clone(),
        territory: config.territory,
        // The cap already in force, resolved here so the panel never has to turn `None`
        // into a number and therefore cannot turn it into a different one.
        spawn_cap: config.effective_spawn_cap(),
        context: config.context.clone(),
        running: (agents.running)(id),
        rules,
        own_rules: config.rules.clone(),
        connected: (agents.connected)(id),
        accepts_messages: config.accepts_messages,
        voice: config.voice,
        voice_available: agents.voice_available,
    })
}

/// A note node, for the panel's Note section.
///
/// Two fields here exist to make a gesture reachable rather than to describe the token.
/// **`title`** is what the file name is proposed from, so the panel can offer
/// `engine-bay.md` instead of demanding a name — and it is the node's own words, which is
/// where a note's title lives. **`connected`** is the agents joined to it by a connector,
/// which is the only way a note can be made *private*: the scope names an owner, and which
/// agent owns a note is the line the user drew.
fn note_of(
    id: ItemId,
    kind: &ItemKind,
    agents: &AgentFacts<'_>,
) -> Option<vellum_ui::NoteSummary> {
    let ItemKind::AgentNote { model, title } = kind else { return None };
    let note = crate::note::decode(model);
    let (on_disk, conflicted) = (agents.note_state)(&note.path);
    Some(vellum_ui::NoteSummary {
        title: title.to_plain(),
        connected: (agents.connected)(id),
        // The owner is resolved to a *label* here. The scope carries an item id, and a row
        // that prints `42@7` at somebody has told them nothing.
        owner: match &note.scope {
            vellum_agent::NoteScope::Private { agent } => (agents.label_of)(agent),
            vellum_agent::NoteScope::Shared => None,
        },
        scope: note.scope.clone(),
        links: note.links.len(),
        path: note.path,
        conflicted,
        on_disk,
    })
}

/// A file tree, for the panel's File tree section.
///
/// The owner is reported **twice on purpose**: `agent` is the label a row shows and
/// `agent_id` is what a write has to carry. Two nodes may both be called *Reviewer*, so a
/// picker that emitted the label would scope the tree to whichever one the app looked up
/// first — and the two would disagree the moment somebody renamed a node.
fn file_tree_of(
    id: ItemId,
    kind: &ItemKind,
    agents: &AgentFacts<'_>,
) -> Option<vellum_ui::FileTreeSummary> {
    let ItemKind::FileTree { model } = kind else { return None };
    let tree = crate::filetree::decode(model);
    Some(vellum_ui::FileTreeSummary {
        agent: tree.agent.as_deref().and_then(|id| (agents.label_of)(id)),
        agent_id: tree.agent.clone(),
        connected: (agents.connected)(id),
        project_dir: agents.project_dir.clone(),
        root: tree.root,
        show_ignored: tree.show_ignored,
    })
}

/// A browser node. **Both** switches are reported, because they are different statements —
/// one is a permission the app grants and the other is an instruction this page was given.
fn browser_of(kind: &ItemKind, agents: &AgentFacts<'_>) -> Option<vellum_ui::BrowserSummary> {
    let ItemKind::Browser { model } = kind else { return None };
    let page = crate::browser::decode(model);
    Some(vellum_ui::BrowserSummary {
        url: page.url,
        title: page.title,
        live: page.live,
        allowed: agents.browser_nodes_allowed,
    })
}

/// The link properties of a card, for the panel's Link section.
fn link_of(kind: &ItemKind) -> Option<vellum_ui::LinkSummary> {
    let (url, provider, thumbnail, mode) = match kind {
        ItemKind::LinkPreview { url, provider, thumbnail, mode, .. }
        | ItemKind::Embed { url, provider, thumbnail, mode, .. } => {
            (url.clone(), provider.clone(), thumbnail.clone(), *mode)
        }
        _ => return None,
    };
    Some(vellum_ui::LinkSummary { url, provider, mode, has_image: thumbnail.is_some() })
}

/// Which set of controls an item answers to.
///
/// A **PDF** presents as [`ItemFacet::Image`] — its page really is a picture, and the panel
/// cannot edit it. A link or embed card has its own facet, because it has its own two
/// properties: how much of it to draw, and the page it points at.
pub const fn facet_of(kind: &ItemKind) -> ItemFacet {
    match kind {
        ItemKind::Sticky { .. } => ItemFacet::Sticky,
        ItemKind::Text { .. } => ItemFacet::Text,
        ItemKind::Ink { .. } => ItemFacet::Ink,
        // A card is a `Link`, not an `Image`. Reporting `Image` is what made the panel call a
        // link "Image" and offer it a picture's controls, so the display mode and the page it
        // points at — the only two properties a card really has — were unreachable.
        ItemKind::LinkPreview { .. } | ItemKind::Embed { .. } => ItemFacet::Link,
        // A PDF stays an image: its page really is a picture, and nothing about it is a link.
        ItemKind::Image { .. } | ItemKind::Document { .. } => ItemFacet::Image,
        ItemKind::Connector { .. } => ItemFacet::Connector,
        ItemKind::Frame { .. } => ItemFacet::Frame,
        ItemKind::Shape { .. } => ItemFacet::Shape,
        ItemKind::Table { .. } => ItemFacet::Table,
        ItemKind::Chart { .. } => ItemFacet::Chart,
        ItemKind::MindMap { .. } => ItemFacet::MindMap,
        ItemKind::Kanban { .. } => ItemFacet::Kanban,
        ItemKind::Group => ItemFacet::Group,
        // The Agent Canvas kinds. A worker, an orchestrator and the meta agent share one
        // facet: they carry the same controls and differ only in which are offered, which
        // the controls decide for themselves — see `ItemFacet::Agent`.
        ItemKind::Agent { .. } => ItemFacet::Agent,
        ItemKind::FileTree { .. } => ItemFacet::FileTree,
        ItemKind::AgentNote { .. } => ItemFacet::Note,
        ItemKind::Browser { .. } => ItemFacet::Browser,
    }
}

/// `None` for a kind with no fill at all; `Some(None)` for "no fill", which a frame
/// can say and a sticky cannot.
fn fill_of(item: &vellum_doc::Item) -> Option<Option<Color>> {
    match &item.kind {
        ItemKind::Sticky { background, .. } => Some(Some(background.unwrap_or(MIRO_YELLOW))),
        // A shape and a frame both take their interior from the style, and both can
        // say "no fill" — which is a statement, not an absence. See `Style::fill`.
        ItemKind::Frame { .. } | ItemKind::Shape { .. } => Some(item.style.fill),
        _ => None,
    }
}

/// Miro's canonical sticky yellow — 43 of the reference board's 44 notes. Shown as a
/// sticky's fill when the document says nothing, so the swatch is never blank.
const MIRO_YELLOW: Color = Color::rgb(0xFF, 0xF7, 0x9E);

/// Where an item's outline is stored, which is not the same place for every kind.
///
/// Named once here rather than re-decided at each of [`apply_style`]'s four border arms,
/// because getting it wrong in one arm and right in the others is how a control ends up
/// half-working. See the module header for why the split is what it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StrokeHome {
    /// [`ItemKind::Ink`] and [`ItemKind::Connector`]: the stroke *is* the geometry, so
    /// colour, width and dash sit on the kind next to the points.
    Kind,
    /// [`ItemKind::Shape`]: an outline the item merely has, carried by `Style` like a
    /// fill. No dash — see [`apply_style`].
    Style,
    /// A sticky, a frame, an image, a card: nothing this panel can edit.
    Nowhere,
}

const fn stroke_home(kind: &ItemKind) -> StrokeHome {
    match kind {
        ItemKind::Ink { .. } | ItemKind::Connector { .. } => StrokeHome::Kind,
        ItemKind::Shape { .. } => StrokeHome::Style,
        _ => StrokeHome::Nowhere,
    }
}

/// The outline, wherever this kind keeps it.
///
/// Takes the whole [`vellum_doc::Item`] rather than its kind, which is the change that
/// let a shape answer at all: a shape's outline is in its *style*, so a function given
/// only the kind could not see one and the panel hid its border section for the 41 forms
/// the app can place.
fn stroke_of(item: &vellum_doc::Item) -> Option<Border> {
    match &item.kind {
        ItemKind::Ink { color, thickness, .. } => Some(Border {
            color: color.unwrap_or(DEFAULT_STROKE),
            width: *thickness,
            style: LineStyle::Solid,
        }),
        ItemKind::Connector { color, thickness, dash, .. } => Some(Border {
            color: color.unwrap_or(DEFAULT_STROKE),
            width: *thickness,
            style: connector::line_style(*dash),
        }),
        // The fallbacks are the painter's, not invented here: `draw.rs`'s shape arm
        // resolves an absent colour to `theme.border` and an absent width to `HAIRLINE`,
        // so this is what is on screen. `Solid` because the document has no pattern to
        // report and claiming otherwise would put a wrong value in the Line row.
        ItemKind::Shape { .. } => Some(Border {
            color: item.style.stroke.unwrap_or(THEME_BORDER),
            width: item.style.stroke_width.unwrap_or(HAIRLINE),
            style: LineStyle::Solid,
        }),
        _ => None,
    }
}

/// The `ink` token of `docs/05-design-language.md` §1, which is what
/// `crate::theme::Theme::LIGHT.stroke` resolves to. Shown when the document carries
/// no colour of its own.
const DEFAULT_STROKE: Color = Color::rgb(0x1A, 0x1D, 0x1F);

/// `frost`, `#E5EAED` — `crate::theme::Theme::LIGHT.border`, which is what a shape with
/// no stroke of its own is drawn with.
///
/// A duplicated constant, and deliberately pinned by a test against the theme it copies:
/// this file cannot reach a `const` through `Rgba`'s float channels, and a hardcoded
/// stand-in for another crate's value going stale silently is precisely the trap that left
/// `locked` reporting `false` for a field that existed. The test is the join.
const THEME_BORDER: Color = Color::rgb(0xE5, 0xEA, 0xED);

/// One device pixel at 100% zoom — `draw.rs`'s `HAIRLINE`, the width a shape draws its
/// outline at when the document names none.
const HAIRLINE: f64 = 1.0;

fn text_of(item: &vellum_doc::Item) -> Option<TextSummary> {
    // Only the kinds that hold editable text get a typography section. A link
    // preview's title is fetched metadata, not something the panel should restyle.
    if !matches!(
        item.kind,
        ItemKind::Sticky { .. } | ItemKind::Text { .. } | ItemKind::Frame { .. }
    ) {
        return None;
    }
    Some(TextSummary {
        // Flattened: the panel's field is plain text, and `Board::set_text` writes
        // the whole value back, so anything the field cannot show is anything a save
        // would drop. Keeping the two in step is what makes that honest.
        content: item.kind.text().map(vellum_doc::StyledText::to_plain).unwrap_or_default(),
        family: item.style.font_family.clone(),
        size: item.style.font_size,
        // Neither weight nor vertical alignment has a document field; both default
        // rather than being guessed from the text's spans, which would make the
        // control disagree with itself the moment one word were bold.
        weight: vellum_ui::FontWeight::default(),
        color: item.style.text_color.unwrap_or(DEFAULT_STROKE),
        align: item.style.align.unwrap_or(Align::Left),
        vertical_align: vellum_ui::VerticalAlign::default(),
        // Miro's default, and `docs/05-design-language.md` §5's canvas line height.
        line_height: item.style.line_height.unwrap_or(1.36),
    })
}

fn connector_of(kind: &ItemKind) -> Option<ConnectorSummary> {
    let ItemKind::Connector { start, end, routing, .. } = kind else { return None };
    Some(ConnectorSummary {
        routing: connector::routing_mode(*routing),
        start_arrow: connector::arrowhead(start.arrowhead),
        end_arrow: connector::arrowhead(end.arrowhead),
        start_anchor: anchor_side(start),
        end_anchor: anchor_side(end),
    })
}

/// Which named point an end is tied to, for the panel's anchor picker.
///
/// A **free** end reports [`AnchorSide::Free`] rather than whichever fraction of the
/// connector's own box it happens to hold: `ConnectorEnd::anchor` is measured against a
/// different rectangle depending on `target`, so reading an unbound end's fraction as a
/// side would report "Left" for an endpoint that is not on anything's left.
fn anchor_side(end: &ConnectorEnd) -> AnchorSide {
    if end.target.is_none() {
        return AnchorSide::Free;
    }
    Anchor::new(end.anchor.0, end.anchor.1).side()
}

// ---------------------------------------------------------------------------
// Panel → document
// ---------------------------------------------------------------------------

/// What happened to an edit, so the caller can say so rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// At least one item changed.
    Changed,
    /// Nothing in the selection carries this property. Not a failure — selecting a
    /// drawing and asking for a font size is a legitimate no-op.
    NotApplicable,
    /// The panel offers it and the document has nowhere to put it. The caller says
    /// so; see the module header for the list.
    Unsupported(&'static str),
}

/// Applies one styling control to every selected item, as a single undo step.
///
/// One group per control rather than one per item: `docs/06-mouse-controls.md` §4
/// makes "one gesture is one undo step" a rule for dragging, and a colour applied to
/// forty stickies has exactly the same claim on it.
pub fn apply_style(board: &mut Board, ids: &[ItemId], edit: &StyleEdit) -> vellum_doc::Result<Applied> {
    if ids.is_empty() {
        return Ok(Applied::NotApplicable);
    }
    if let StyleEdit::FontWeight(_) = edit {
        return Ok(Applied::Unsupported(
            "Font weight lives in the text's own spans, which are not editable yet",
        ));
    }
    if let StyleEdit::VerticalAlign(_) = edit {
        return Ok(Applied::Unsupported(
            "Vertical alignment is a layout parameter the document does not carry yet",
        ));
    }
    // A shape's border section is real — colour and width both land — but the Line row
    // beside them has nowhere to go: `Style` carries no dash pattern and the SDF border is
    // one coverage band. Reported only when *nothing* in the selection can be dashed, so a
    // connector selected alongside a shape still dashes and says nothing.
    if let StyleEdit::BorderStyle(_) = edit {
        let kinds = ids.iter().filter_map(|id| board.item(*id).ok()).map(|item| item.kind);
        let (mut shapes, mut dashable) = (0_usize, 0_usize);
        for kind in kinds {
            match kind {
                ItemKind::Shape { .. } => shapes += 1,
                ItemKind::Connector { .. } => dashable += 1,
                _ => {}
            }
        }
        if shapes > 0 && dashable == 0 {
            return Ok(Applied::Unsupported(
                "A shape's outline is solid — only a connector's line carries a dash",
            ));
        }
    }
    board.begin_undo_group()?;
    let mut changed = 0;
    for id in ids {
        let Ok(item) = board.item(*id) else { continue };
        if apply_one(board, *id, &item, edit)? {
            changed += 1;
        }
    }
    board.end_undo_group();
    Ok(if changed > 0 { Applied::Changed } else { Applied::NotApplicable })
}

fn apply_one(
    board: &mut Board,
    id: ItemId,
    item: &vellum_doc::Item,
    edit: &StyleEdit,
) -> vellum_doc::Result<bool> {
    let mut style = item.style.clone();
    let mut kind = item.kind.clone();
    let mut touched_style = false;
    let mut touched_kind = false;

    match edit {
        StyleEdit::Fill(fill) => match &mut kind {
            // A sticky's colour is the sticky, so it lives on the kind rather than
            // the style — `vellum_doc::Style::fill` says so explicitly. Clearing it
            // means "the board's default", which is the closest a note has to no fill.
            ItemKind::Sticky { background, .. } => {
                *background = *fill;
                touched_kind = true;
            }
            ItemKind::Frame { .. } => {
                style.fill = *fill;
                touched_style = true;
            }
            _ => {}
        },
        StyleEdit::Opacity(opacity) => {
            style.opacity = Some(opacity.clamp(0.0, 1.0));
            touched_style = true;
        }
        StyleEdit::FontFamily(family) => {
            style.font_family = family.clone();
            touched_style = true;
        }
        StyleEdit::FontSize(size) => {
            style.font_size = *size;
            touched_style = true;
        }
        StyleEdit::TextColor(color) => {
            style.text_color = Some(*color);
            touched_style = true;
        }
        StyleEdit::Align(align) => {
            style.align = Some(*align);
            touched_style = true;
        }
        StyleEdit::LineHeight(height) => {
            style.line_height = Some(*height);
            touched_style = true;
        }
        // The border controls reach whichever half of the item holds the outline. See
        // `stroke_home`: for ink and a connector that is the kind, for a shape the style.
        StyleEdit::BorderColor(color) => match stroke_home(&kind) {
            StrokeHome::Kind => touched_kind = set_stroke_color(&mut kind, Some(*color)),
            StrokeHome::Style => {
                style.stroke = Some(*color);
                touched_style = true;
            }
            StrokeHome::Nowhere => {}
        },
        StyleEdit::BorderWidth(width) => match stroke_home(&kind) {
            StrokeHome::Kind => touched_kind = set_stroke_width(&mut kind, *width),
            // Not clamped away from zero the way a stroke's own width is: a shape is hit
            // by its *fill*, so a borderless shape is still selectable, and refusing zero
            // would make "no border" reachable only through the ✕ button.
            StrokeHome::Style => {
                style.stroke_width = Some(width.max(0.0));
                touched_style = true;
            }
            StrokeHome::Nowhere => {}
        },
        StyleEdit::BorderStyle(line) => touched_kind = set_dash(&mut kind, *line),
        StyleEdit::BorderCleared => match stroke_home(&kind) {
            StrokeHome::Kind => touched_kind = set_stroke_color(&mut kind, None),
            // A *transparent* stroke, not an absent one, and the difference is the whole
            // point — `Style::stroke`'s doc comment draws it: `None` inherits the theme's
            // border, which is still a visible hairline, while a zero alpha refuses one.
            // Both shape paths multiply by the border's alpha (`fill_and_border` in the
            // SDF shader, `push_shape_stroke` for the tessellated forms), so this is
            // no border rather than a fainter one.
            StrokeHome::Style => {
                style.stroke = Some(Color::rgba(0, 0, 0, 0));
                touched_style = true;
            }
            StrokeHome::Nowhere => {}
        },
        StyleEdit::Routing(mode) => {
            if let ItemKind::Connector { routing, .. } = &mut kind {
                *routing = routing_of(*mode);
                touched_kind = true;
            }
        }
        StyleEdit::StartArrow(head) => touched_kind = set_arrow(&mut kind, true, *head),
        StyleEdit::EndArrow(head) => touched_kind = set_arrow(&mut kind, false, *head),
        // The card's display mode. On the kind rather than the style — see `CardMode` — so
        // this is a `set_kind`, and one click is one undo step like every other control.
        StyleEdit::CardMode(next) => {
            touched_kind = match &mut kind {
                ItemKind::LinkPreview { mode, .. } | ItemKind::Embed { mode, .. } => {
                    let changed = *mode != *next;
                    *mode = *next;
                    changed
                }
                _ => false,
            };
        }
        StyleEdit::StartAnchor(side) => touched_kind = set_anchor(&mut kind, true, *side),
        StyleEdit::EndAnchor(side) => touched_kind = set_anchor(&mut kind, false, *side),
        // The panel's padlock. Written through the same `set_style` as everything else,
        // so one click is one undo step and a lock survives a reload like any other
        // style. `vellum-doc` deliberately does not *enforce* it — see `Style::locked` —
        // which is exactly what lets this write unlock an item again.
        StyleEdit::Locked(locked) => {
            style.locked = *locked;
            touched_style = true;
        }
        // Handled before the loop, where they can be reported once rather than per
        // item.
        StyleEdit::FontWeight(_) | StyleEdit::VerticalAlign(_) => {}
    }

    if touched_kind {
        board.set_kind(id, kind)?;
    }
    if touched_style {
        board.set_style(id, style)?;
    }
    Ok(touched_kind || touched_style)
}

fn set_stroke_color(kind: &mut ItemKind, value: Option<Color>) -> bool {
    match kind {
        ItemKind::Ink { color, .. } | ItemKind::Connector { color, .. } => {
            *color = value;
            true
        }
        _ => false,
    }
}

fn set_stroke_width(kind: &mut ItemKind, width: f64) -> bool {
    // A zero-width stroke is invisible and un-hittable, which is a way to lose a
    // drawing without deleting it.
    let width = width.max(0.25);
    match kind {
        ItemKind::Ink { thickness, .. } | ItemKind::Connector { thickness, .. } => {
            *thickness = width;
            true
        }
        _ => false,
    }
}

fn set_dash(kind: &mut ItemKind, line: LineStyle) -> bool {
    if let ItemKind::Connector { dash, .. } = kind {
        *dash = dash_of(line);
        true
    } else {
        false
    }
}

fn set_arrow(kind: &mut ItemKind, start: bool, head: Arrowhead) -> bool {
    let ItemKind::Connector { start: s, end: e, .. } = kind else { return false };
    let target: &mut ConnectorEnd = if start { s } else { e };
    target.arrowhead = arrow_of(head);
    true
}

/// Moves one end of a connector to a named attachment point.
///
/// Refuses a **free** end. The anchor of an unbound end is a fraction of the connector's
/// own rectangle, so writing `LEFT` onto one would not attach it to anything — it would
/// drag the visible endpoint to the middle of the connector's own left edge and leave it
/// there. The panel already disables the control for those, and this is the second half of
/// the same rule, in the place a preset or a script would also come through.
fn set_anchor(kind: &mut ItemKind, start: bool, side: AnchorSide) -> bool {
    let ItemKind::Connector { start: s, end: e, .. } = kind else { return false };
    let Some(anchor) = side.anchor() else { return false };
    let target: &mut ConnectorEnd = if start { s } else { e };
    if target.target.is_none() {
        return false;
    }
    let next = (anchor.x, anchor.y);
    if target.anchor == next {
        return false;
    }
    target.anchor = next;
    true
}

/// Applies a committed position or size field.
///
/// For a multi-selection the values describe the selection's bounding box, matching
/// [`TransformEdit`]'s contract: `X` moves every item so the box starts there, and
/// width and height are not emitted at all.
pub fn apply_transform(
    board: &mut Board,
    ids: &[ItemId],
    edit: TransformEdit,
) -> vellum_doc::Result<Applied> {
    if ids.is_empty() {
        return Ok(Applied::NotApplicable);
    }
    let placements: Vec<(ItemId, Placement)> = ids
        .iter()
        .filter_map(|id| board.item(*id).ok().map(|item| (*id, item.placement)))
        .collect();
    if placements.is_empty() {
        return Ok(Applied::NotApplicable);
    }

    let min_x = placements.iter().map(|(_, p)| p.x).fold(f64::INFINITY, f64::min);
    let min_y = placements.iter().map(|(_, p)| p.y).fold(f64::INFINITY, f64::min);

    board.begin_undo_group()?;
    for (id, placement) in &placements {
        let mut next = *placement;
        match edit {
            TransformEdit::X(x) => next.x = placement.x + (x - min_x),
            TransformEdit::Y(y) => next.y = placement.y + (y - min_y),
            // Guarded rather than clamped silently at zero: a zero-sized item cannot
            // be hit-tested, so it becomes unselectable and therefore unrecoverable.
            TransformEdit::Width(w) => next.width = w.max(1.0) / next.scale.max(f64::EPSILON),
            TransformEdit::Height(h) => next.height = h.max(1.0) / next.scale.max(f64::EPSILON),
            TransformEdit::Rotation(degrees) => next.rotation = degrees.rem_euclid(360.0),
        }
        if next != *placement {
            board.set_placement(*id, next)?;
        }
    }
    board.end_undo_group();
    Ok(Applied::Changed)
}

// The reverse of `crate::connector`'s document-to-geometry mappings. They are here
// rather than there because that module is on the frame path and only ever reads.

pub const fn routing_of(mode: RoutingMode) -> Routing {
    match mode {
        RoutingMode::Straight => Routing::Straight,
        RoutingMode::Curved => Routing::Curved,
        RoutingMode::Orthogonal => Routing::Orthogonal,
    }
}

pub const fn dash_of(style: LineStyle) -> Dash {
    match style {
        LineStyle::Solid => Dash::Solid,
        LineStyle::Dashed => Dash::Dashed,
        LineStyle::Dotted => Dash::Dotted,
    }
}

pub const fn arrow_of(head: Arrowhead) -> ArrowKind {
    match head {
        Arrowhead::None => ArrowKind::None,
        Arrowhead::LineArrow => ArrowKind::LineArrow,
        Arrowhead::FilledTriangle => ArrowKind::FilledTriangle,
        Arrowhead::OpenTriangle => ArrowKind::OpenTriangle,
        Arrowhead::Circle => ArrowKind::Circle,
        Arrowhead::FilledCircle => ArrowKind::FilledCircle,
        Arrowhead::Diamond => ArrowKind::Diamond,
        Arrowhead::FilledDiamond => ArrowKind::FilledDiamond,
    }
}

/// A default [`Style`] carrying nothing, for a newly placed item. Named so the
/// intent — *inherit everything* — is visible at the call site.
pub fn inherited_style() -> Style {
    Style::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{NewItem, StyledText};
    use vellum_ui::PanelModel;

    fn board_with(kinds: Vec<ItemKind>) -> (Board, Vec<ItemId>) {
        let mut board = Board::new();
        let ids = kinds
            .into_iter()
            .enumerate()
            .map(|(i, kind)| {
                board
                    .add(NewItem::new(
                        kind,
                        Placement::new(i as f64 * 300.0, 0.0, 200.0, 200.0),
                    ))
                    .unwrap()
            })
            .collect();
        (board, ids)
    }

    fn sticky(text: &str) -> ItemKind {
        ItemKind::Sticky { text: StyledText::plain(text), background: None }
    }

    fn ink() -> ItemKind {
        ItemKind::Ink { points: vec![], color: None, thickness: 4.0 }
    }

    fn wire() -> ItemKind {
        ItemKind::Connector {
            start: ConnectorEnd::free((0.0, 0.0)),
            end: ConnectorEnd::free((1.0, 1.0)),
            routing: Routing::Straight,
            dash: Dash::Solid,
            thickness: 2.0,
            color: None,
            captions: Vec::new(),
        }
    }

    fn items(board: &Board, ids: &[ItemId]) -> Vec<SelectionItem> {
        ids.iter()
            .map(|id| describe(*id, &board.item(*id).unwrap(), &AgentFacts::unknown()))
            .collect()
    }

    #[test]
    fn every_document_kind_maps_to_a_facet() {
        assert_eq!(facet_of(&sticky("a")), ItemFacet::Sticky);
        assert_eq!(facet_of(&ink()), ItemFacet::Ink);
        assert_eq!(facet_of(&wire()), ItemFacet::Connector);
        assert_eq!(facet_of(&ItemKind::Group), ItemFacet::Group);
        assert_eq!(
            facet_of(&ItemKind::Image { asset_id: "h".into(), crop: None }),
            ItemFacet::Image
        );
    }

    fn shape() -> ItemKind {
        ItemKind::Shape { form: "rect".into(), text: StyledText::plain("") }
    }

    /// The panel's border section belongs to the kinds that have an outline, and must be
    /// absent everywhere else. Getting that wrong either shows an inert control on a
    /// sticky or hides a live one from a shape.
    #[test]
    fn only_kinds_with_an_outline_carry_a_border() {
        let (board, ids) = board_with(vec![sticky("a"), ink(), wire(), shape()]);
        let described = items(&board, &ids);
        assert!(described[0].border.is_none(), "a sticky has no border in the model");
        assert_eq!(described[1].border.unwrap().width, 4.0);
        assert_eq!(described[2].border.unwrap().style, LineStyle::Solid);

        // The shape's outline is drawn from the *style*, which is why `stroke_of` needed
        // the whole item: asked only for the kind it could not see one and the section
        // was hidden for all 41 forms while the fill row above it worked.
        let outline = described[3].border.expect("a shape has an editable outline");
        assert_eq!(outline.color, THEME_BORDER, "what the painter falls back to");
        assert_eq!(outline.width, HAIRLINE);
        assert_eq!(outline.style, LineStyle::Solid, "the document carries no pattern");
    }

    /// The stand-in for `theme::Theme::LIGHT.border`, pinned to the value it copies.
    ///
    /// A constant in one crate standing for a value in another is the exact shape of the
    /// bug that left `locked` hardcoded `false` after the field landed. This test is the
    /// join that a comment cannot be.
    #[test]
    fn the_border_fallback_is_the_theme_it_claims_to_be() {
        let theme = crate::theme::Theme::LIGHT.border.pack();
        assert_eq!([THEME_BORDER.r, THEME_BORDER.g, THEME_BORDER.b, THEME_BORDER.a], theme);
    }

    /// Colour and width on a shape have to reach the *style*, and reach it in both
    /// directions, or the panel shows a control that argues with the canvas.
    #[test]
    fn a_shapes_outline_is_editable_through_the_style() {
        let (mut board, ids) = board_with(vec![shape()]);
        let navy = Color::rgb(0x11, 0x33, 0x77);

        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::BorderColor(navy)).unwrap(),
            Applied::Changed
        ));
        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::BorderWidth(6.0)).unwrap(),
            Applied::Changed
        ));
        let item = board.item(ids[0]).unwrap();
        assert_eq!(item.style.stroke, Some(navy), "the painter reads this field");
        assert_eq!(item.style.stroke_width, Some(6.0));
        // Round trip: what was written is what the panel reads back.
        let described = describe(ids[0], &item, &AgentFacts::unknown()).border.unwrap();
        assert_eq!((described.color, described.width), (navy, 6.0));

        // "No border" is a transparent stroke, not an absent one — an absent one inherits
        // the theme's hairline, which is still a visible border.
        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::BorderCleared).unwrap(),
            Applied::Changed
        ));
        let cleared = board.item(ids[0]).unwrap().style.stroke.expect("stated, not absent");
        assert_eq!(cleared.a, 0, "both shape paths multiply by the border's alpha");
    }

    /// A dash is the one border control a shape cannot honour, so it must say so — and
    /// must *not* say so when something in the selection can be dashed.
    #[test]
    fn a_dash_on_a_shape_reports_rather_than_vanishing() {
        let (mut board, ids) = board_with(vec![shape()]);
        assert!(
            matches!(
                apply_style(&mut board, &ids, &StyleEdit::BorderStyle(LineStyle::Dashed)),
                Ok(Applied::Unsupported(_))
            ),
            "a shape-only selection has nowhere to put a pattern"
        );

        let (mut mixed, mixed_ids) = board_with(vec![shape(), wire()]);
        assert!(matches!(
            apply_style(&mut mixed, &mixed_ids, &StyleEdit::BorderStyle(LineStyle::Dashed)).unwrap(),
            Applied::Changed
        ));
        let dashed = mixed.item(mixed_ids[1]).unwrap();
        assert!(
            matches!(dashed.kind, ItemKind::Connector { dash: Dash::Dashed, .. }),
            "the connector still dashes, silently, alongside the shape"
        );
    }

    /// An ink stroke and a connector keep their outline on the *kind*. This is the half
    /// `stroke_home` exists to keep separate: routing a shape's border to the style must
    /// not have moved theirs.
    #[test]
    fn a_stroke_that_is_the_geometry_still_lives_on_the_kind() {
        let (mut board, ids) = board_with(vec![ink()]);
        let red = Color::rgb(0xE0, 0x3C, 0x3C);
        apply_style(&mut board, &ids, &StyleEdit::BorderColor(red)).unwrap();
        apply_style(&mut board, &ids, &StyleEdit::BorderWidth(9.0)).unwrap();

        let item = board.item(ids[0]).unwrap();
        assert!(
            matches!(item.kind, ItemKind::Ink { color: Some(c), thickness, .. }
                if c == red && (thickness - 9.0).abs() < f64::EPSILON),
            "ink keeps colour and thickness beside its points"
        );
        assert_eq!(item.style.stroke, None, "and nothing leaked into the style");
        assert_eq!(item.style.stroke_width, None);
    }

    /// The mixed-value rule, end to end: two stickies that disagree show *Mixed*, and
    /// a kind with no fill is skipped rather than counted as disagreement.
    #[test]
    fn a_mixed_selection_reports_mixed_without_counting_absent_properties() {
        let mut board = Board::new();
        let red = board
            .add(NewItem::new(
                ItemKind::Sticky {
                    text: StyledText::plain("a"),
                    background: Some(Color::rgb(0xFF, 0x9E, 0x9E)),
                },
                Placement::new(0.0, 0.0, 200.0, 200.0),
            ))
            .unwrap();
        let yellow = board
            .add(NewItem::new(sticky("b"), Placement::new(300.0, 0.0, 200.0, 200.0)))
            .unwrap();
        let stroke = board
            .add(NewItem::new(ink(), Placement::new(600.0, 0.0, 200.0, 200.0)))
            .unwrap();

        let one = PanelModel::derive(&items(&board, &[red]));
        assert_eq!(one.fill.value(), Some(&Some(Color::rgb(0xFF, 0x9E, 0x9E))));

        let two = PanelModel::derive(&items(&board, &[red, yellow]));
        assert!(two.fill.is_mixed(), "two different stickies must read Mixed");

        // The ink stroke has no fill, so it must not turn a uniform fill into a
        // mixed one.
        let with_ink = PanelModel::derive(&items(&board, &[yellow, stroke]));
        assert_eq!(with_ink.fill.value(), Some(&Some(MIRO_YELLOW)));
    }

    #[test]
    fn a_fill_edit_reaches_a_sticky_and_a_frame_by_different_routes() {
        let mut board = Board::new();
        let note = board
            .add(NewItem::new(sticky("a"), Placement::new(0.0, 0.0, 200.0, 200.0)))
            .unwrap();
        let frame = board
            .add(NewItem::new(
                ItemKind::Frame {
                    title: StyledText::plain("f"),
                    order: None,
                    speaker_notes: None,
                },
                Placement::new(0.0, 0.0, 900.0, 600.0),
            ))
            .unwrap();

        let blue = Color::rgb(0x6F, 0xD6, 0xE6);
        assert_eq!(
            apply_style(&mut board, &[note, frame], &StyleEdit::Fill(Some(blue))).unwrap(),
            Applied::Changed
        );

        match board.item(note).unwrap().kind {
            ItemKind::Sticky { background, .. } => assert_eq!(background, Some(blue)),
            other => panic!("{other:?}"),
        }
        assert_eq!(board.item(frame).unwrap().style.fill, Some(blue));
    }

    /// A colour applied to a whole selection has to be **one** undo step, not one per
    /// item, or a single click costs forty presses of Cmd+Z to take back.
    #[test]
    fn one_control_is_one_undo_step_however_many_items() {
        let (mut board, ids) = board_with(vec![sticky("a"), sticky("b"), sticky("c")]);
        let blue = Color::rgb(0x6F, 0xD6, 0xE6);
        apply_style(&mut board, &ids, &StyleEdit::Fill(Some(blue))).unwrap();

        assert!(board.undo().unwrap());
        for id in &ids {
            match board.item(*id).unwrap().kind {
                ItemKind::Sticky { background, .. } => assert_eq!(background, None),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn the_border_controls_edit_a_connectors_stroke() {
        let (mut board, ids) = board_with(vec![wire()]);
        apply_style(&mut board, &ids, &StyleEdit::BorderWidth(6.0)).unwrap();
        apply_style(&mut board, &ids, &StyleEdit::BorderStyle(LineStyle::Dashed)).unwrap();
        apply_style(&mut board, &ids, &StyleEdit::EndArrow(Arrowhead::FilledTriangle)).unwrap();
        apply_style(&mut board, &ids, &StyleEdit::Routing(RoutingMode::Orthogonal)).unwrap();

        match board.item(ids[0]).unwrap().kind {
            ItemKind::Connector { thickness, dash, routing, end, .. } => {
                assert_eq!(thickness, 6.0);
                assert_eq!(dash, Dash::Dashed);
                assert_eq!(routing, Routing::Orthogonal);
                assert_eq!(end.arrowhead, ArrowKind::FilledTriangle);
            }
            other => panic!("{other:?}"),
        }
    }

    /// The anchor picker, both ways. This is the whole of "choosing an attachment": a
    /// drawn connector takes the edge facing the other end, which is computed and could
    /// not be argued with, and an imported one uses the centre, which a drag never
    /// produces and so had no other way of being changed.
    #[test]
    fn the_anchor_picker_moves_a_bound_end_to_a_named_side() {
        // Bound to real items, which is the only way an end is ever bound in practice —
        // and `set_anchor` reads `target` to decide whether a side means anything.
        let mut board = Board::new();
        let from = board
            .add(NewItem::new(sticky("from"), Placement::new(0.0, 0.0, 200.0, 200.0)))
            .unwrap();
        let to = board
            .add(NewItem::new(sticky("to"), Placement::new(600.0, 0.0, 200.0, 200.0)))
            .unwrap();
        let ids = vec![
            board
                .add(NewItem::new(
                    ItemKind::Connector {
                        start: ConnectorEnd::bound(from, ConnectorEnd::CENTER),
                        end: ConnectorEnd::bound(to, ConnectorEnd::LEFT),
                        routing: Routing::Straight,
                        dash: Dash::Solid,
                        thickness: 2.0,
                        color: None,
                        captions: Vec::new(),
                    },
                    Placement::new(300.0, 0.0, 400.0, 10.0),
                ))
                .unwrap(),
        ];

        // Reported before anything is touched: the two ends read differently, which is
        // what makes the picker's two rows independent.
        let described = describe(ids[0], &board.item(ids[0]).unwrap(), &AgentFacts::unknown()).connector.unwrap();
        assert_eq!(described.start_anchor, AnchorSide::Centre);
        assert_eq!(described.end_anchor, AnchorSide::Left);

        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::StartAnchor(AnchorSide::Top)).unwrap(),
            Applied::Changed
        ));
        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::EndAnchor(AnchorSide::Bottom)).unwrap(),
            Applied::Changed
        ));
        match board.item(ids[0]).unwrap().kind {
            ItemKind::Connector { start, end, .. } => {
                assert_eq!(start.anchor, ConnectorEnd::TOP);
                assert_eq!(end.anchor, ConnectorEnd::BOTTOM, "each row moved only its own end");
            }
            other => panic!("{other:?}"),
        }

        // Idempotent: picking the side an end is already on is not a change, so it does
        // not become an undo step that undoes nothing.
        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::StartAnchor(AnchorSide::Top)).unwrap(),
            Applied::NotApplicable
        ));
    }

    /// A **free** end has no side, and must refuse to be given one.
    ///
    /// Its anchor is a fraction of the *connector's own* rectangle rather than of a
    /// target's, so writing `LEFT` onto one would not attach it to anything — it would
    /// silently drag the visible endpoint to the middle of the connector's own left edge.
    /// The panel disables the control; this is the same rule at the door a preset would
    /// come through.
    #[test]
    fn an_unattached_end_reports_free_and_refuses_a_side() {
        let (mut board, ids) = board_with(vec![wire()]);
        let described = describe(ids[0], &board.item(ids[0]).unwrap(), &AgentFacts::unknown()).connector.unwrap();
        assert_eq!(described.start_anchor, AnchorSide::Free);
        assert_eq!(described.end_anchor, AnchorSide::Free);

        let before = board.item(ids[0]).unwrap().kind.clone();
        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::StartAnchor(AnchorSide::Left)).unwrap(),
            Applied::NotApplicable
        ));
        assert_eq!(board.item(ids[0]).unwrap().kind, before, "the endpoint did not move");
    }

    /// A card is a **Link**, and its display mode round-trips through the panel.
    ///
    /// Both halves were broken together: `facet_of` answered `Image`, so the panel called a
    /// link "Image" and drew a picture's controls, and there was no `StyleEdit` that could
    /// change a mode — so the three modes existed in the document and on screen and were
    /// unreachable by clicking.
    #[test]
    fn a_card_is_a_link_and_its_mode_round_trips() {
        use vellum_doc::CardMode;

        let (mut board, ids) = board_with(vec![
            ItemKind::link_preview(
                Some("Model 33".into()),
                Some("https://www.aliexpress.us/item/1.html".into()),
                None,
            ),
            ItemKind::embed(None, Some("https://youtu.be/abc".into()), None, None, None),
        ]);

        for id in &ids {
            let described = describe(*id, &board.item(*id).unwrap(), &AgentFacts::unknown());
            assert_eq!(described.facet, ItemFacet::Link, "a card is not an Image");
            let link = described.link.expect("a card has link properties");
            assert_eq!(link.mode, CardMode::Card, "the default");
            assert!(!link.has_image, "nothing fetched yet");
            assert!(link.url.is_some());
        }

        // The picker writes it, on both kinds, and it is one undo step.
        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::CardMode(CardMode::Large)).unwrap(),
            Applied::Changed
        ));
        for id in &ids {
            assert_eq!(
                describe(*id, &board.item(*id).unwrap(), &AgentFacts::unknown()).link.unwrap().mode,
                CardMode::Large
            );
        }

        // Choosing the mode it is already in is not a change, so it does not become an undo
        // step that undoes nothing.
        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::CardMode(CardMode::Large)).unwrap(),
            Applied::NotApplicable
        ));

        // And the panel's Link section appears for them, with one URL when one is selected.
        let items: Vec<vellum_ui::SelectionItem> =
            ids.iter().map(|id| describe(*id, &board.item(*id).unwrap(), &AgentFacts::unknown())).collect();
        let both = PanelModel::derive(&items);
        assert!(both.has_link());
        assert_eq!(both.link_url, None, "Open acts on one page, not forty");
        let one = PanelModel::derive(&items[..1]);
        assert_eq!(one.link_url.as_deref(), Some("https://www.aliexpress.us/item/1.html"));
    }

    /// A sticky has no Link section — the section has to be absent, not empty.
    #[test]
    fn a_non_card_has_no_link_section() {
        let (board, ids) = board_with(vec![sticky("a")]);
        let described = describe(ids[0], &board.item(ids[0]).unwrap(), &AgentFacts::unknown());
        assert!(described.link.is_none());
        assert!(!PanelModel::derive(&[described]).has_link());
    }

    /// A stroke of zero width is invisible *and* un-hittable, which loses the drawing
    /// without deleting it.
    #[test]
    fn a_stroke_cannot_be_widened_down_to_nothing() {
        let (mut board, ids) = board_with(vec![ink()]);
        apply_style(&mut board, &ids, &StyleEdit::BorderWidth(0.0)).unwrap();
        match board.item(ids[0]).unwrap().kind {
            ItemKind::Ink { thickness, .. } => assert!(thickness > 0.0, "{thickness}"),
            other => panic!("{other:?}"),
        }
    }

    /// The two controls the document cannot carry must say so. A silent no-op is
    /// exactly the "clicked it and nothing happened" failure the chrome's disabled
    /// reasons exist to prevent.
    ///
    /// `Locked` used to be the third and is not any more — see the round trip below.
    #[test]
    fn the_unsupported_controls_report_rather_than_do_nothing() {
        let (mut board, ids) = board_with(vec![sticky("a")]);
        for edit in [
            StyleEdit::FontWeight(vellum_ui::FontWeight::Bold),
            StyleEdit::VerticalAlign(vellum_ui::VerticalAlign::Top),
        ] {
            assert!(
                matches!(
                    apply_style(&mut board, &ids, &edit).unwrap(),
                    Applied::Unsupported(_)
                ),
                "{edit:?}"
            );
        }
    }

    /// The panel's padlock has to work in **both** directions, which is the whole point:
    /// it is one of the only two controls that can undo a lock.
    ///
    /// It was neither. `apply_style` reported `Locked` as unsupported and dropped it, so
    /// the padlock did nothing at all, in either direction.
    #[test]
    fn the_padlock_locks_and_unlocks_through_the_document() {
        let (mut board, ids) = board_with(vec![sticky("a")]);
        assert!(!board.item(ids[0]).unwrap().style.locked, "starts unlocked");

        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::Locked(true)).unwrap(),
            Applied::Changed
        ));
        assert!(board.item(ids[0]).unwrap().style.locked, "the padlock locked it");

        assert!(matches!(
            apply_style(&mut board, &ids, &StyleEdit::Locked(false)).unwrap(),
            Applied::Changed
        ));
        assert!(!board.item(ids[0]).unwrap().style.locked, "and unlocked it again");
    }

    /// `describe` is what the chrome counts to decide whether Unlock is reachable, so
    /// reporting a constant here is not cosmetic — it disables the control.
    ///
    /// The regression this pins: `locked` was hardcoded `false`, so `any_locked` was
    /// always false, `Command::Unlock` was always `Disabled("Nothing in the selection is
    /// locked")`, and the padlock always drew "Lock". Locking was a one-way door.
    #[test]
    fn a_locked_item_is_described_as_locked_so_unlock_is_reachable() {
        let (mut board, ids) = board_with(vec![sticky("a"), sticky("b")]);
        board.set_style(ids[0], Style { locked: true, ..Default::default() }).unwrap();

        let items: Vec<vellum_ui::SelectionItem> = ids
            .iter()
            .map(|id| describe(*id, &board.item(*id).unwrap(), &AgentFacts::unknown()))
            .collect();
        assert!(items[0].locked, "the locked one reports locked");
        assert!(!items[1].locked, "the other does not");

        // Through the same aggregation `vellum_ui::chrome` performs, to the same gate.
        let mixed = PanelModel::derive(&items);
        assert!(mixed.any_locked(), "a mixed selection offers Unlock");
        assert!(!mixed.all_locked(), "but is not wholly locked");

        let one = PanelModel::derive(&items[..1]);
        assert!(one.all_locked() && one.any_locked());
    }

    /// `TransformEdit::X` on a multi-selection moves the **bounding box**, keeping the
    /// items' relative positions. Setting them all to the same x would collapse the
    /// selection into a stack.
    #[test]
    fn moving_a_multi_selection_moves_the_box_not_each_item() {
        let (mut board, ids) = board_with(vec![sticky("a"), sticky("b"), sticky("c")]);
        let before: Vec<f64> = ids
            .iter()
            .map(|id| board.item(*id).unwrap().placement.x)
            .collect();
        assert_eq!(before, vec![0.0, 300.0, 600.0]);

        apply_transform(&mut board, &ids, TransformEdit::X(1_000.0)).unwrap();
        let after: Vec<f64> = ids
            .iter()
            .map(|id| board.item(*id).unwrap().placement.x)
            .collect();
        assert_eq!(after, vec![1_000.0, 1_300.0, 1_600.0]);
    }

    #[test]
    fn a_size_edit_respects_the_items_scale_and_never_reaches_zero() {
        let mut board = Board::new();
        let id = board
            .add(NewItem::new(
                sticky("a"),
                Placement { scale: 2.0, ..Placement::new(0.0, 0.0, 100.0, 100.0) },
            ))
            .unwrap();

        apply_transform(&mut board, &[id], TransformEdit::Width(400.0)).unwrap();
        let placement = board.item(id).unwrap().placement;
        assert_eq!(placement.scaled_size().0, 400.0, "the typed width is the drawn width");

        apply_transform(&mut board, &[id], TransformEdit::Width(0.0)).unwrap();
        assert!(board.item(id).unwrap().placement.width > 0.0);
    }

    #[test]
    fn rotation_wraps_rather_than_growing_without_bound() {
        let (mut board, ids) = board_with(vec![sticky("a")]);
        apply_transform(&mut board, &ids, TransformEdit::Rotation(450.0)).unwrap();
        assert_eq!(board.item(ids[0]).unwrap().placement.rotation, 90.0);
        apply_transform(&mut board, &ids, TransformEdit::Rotation(-90.0)).unwrap();
        assert_eq!(board.item(ids[0]).unwrap().placement.rotation, 270.0);
    }

    #[test]
    fn editing_nothing_is_not_an_error() {
        let mut board = Board::new();
        assert_eq!(
            apply_style(&mut board, &[], &StyleEdit::Opacity(0.5)).unwrap(),
            Applied::NotApplicable
        );
        assert_eq!(
            apply_transform(&mut board, &[], TransformEdit::X(0.0)).unwrap(),
            Applied::NotApplicable
        );
    }

    /// The connector mappings are written in both directions in two different files.
    /// A round trip is the only thing that catches a pair that drifted.
    #[test]
    fn the_connector_mappings_round_trip() {
        for routing in [Routing::Straight, Routing::Curved, Routing::Orthogonal] {
            assert_eq!(routing_of(connector::routing_mode(routing)), routing);
        }
        for dash in [Dash::Solid, Dash::Dashed, Dash::Dotted] {
            assert_eq!(dash_of(connector::line_style(dash)), dash);
        }
        for arrow in [
            ArrowKind::None,
            ArrowKind::LineArrow,
            ArrowKind::FilledTriangle,
            ArrowKind::OpenTriangle,
            ArrowKind::Circle,
            ArrowKind::FilledCircle,
            ArrowKind::Diamond,
            ArrowKind::FilledDiamond,
        ] {
            assert_eq!(arrow_of(connector::arrowhead(arrow)), arrow);
        }
    }

    #[test]
    fn a_new_item_inherits_everything() {
        assert!(inherited_style().is_default());
    }
}
