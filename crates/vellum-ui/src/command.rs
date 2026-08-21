//! Every named action the chrome can ask the app to perform.
//!
//! The menu bar is generated from this table rather than written out, so a command
//! cannot exist with no way to reach it, appear in two menus, or carry a shortcut
//! that another command also claims — all three are asserted in this module's tests.
//! The same table is what the command palette ([`crate::command_palette`], `Cmd+K`)
//! enumerates, so it is deliberately data rather than code.
//!
//! # The menu tree is Miro's
//!
//! `docs/04-ui-reference.md` §4 transcribes the tree the user's hands already know:
//! **Board · Edit · View · Preferences**. The bar used to read File/Edit/View/
//! Object/Help, which is the tree every desktop app has and the one this user has no
//! muscle memory for. The differences from Miro's, and why:
//!
//! - **The cloud entries are dropped**, not renamed: Google Drive, Embed, Catch up
//!   and Share as presentation all need an account and a server, and
//!   `docs/features/README.md` §11 cuts everything of that kind. In their place
//!   Export offers **PNG · PDF · SVG · CSV** and a **`.velm` backup**.
//! - **Miro's Object operations live in a floating context toolbar**, which this
//!   crate does not draw. They are reachable here as **Edit ▸ Arrange** rather than
//!   as a fifth top-level menu, so the bar keeps the four names §4 records.
//! - **Miro's separate Accessibility menu folds into Preferences**, which is where
//!   the appearance and translucency controls `docs/05-design-language.md` §3a asks
//!   for already sit. Help has no menu of its own for the same reason it does not in
//!   Miro — its three entries sit at the foot of Preferences.
//!
//! # Nothing is ever inert
//!
//! Every command answers [`Command::availability`] rather than a bare boolean, and a
//! disabled one carries the *reason* it is disabled as a tooltip. A menu row that can
//! be clicked and does nothing is the failure this replaces; a greyed row that says
//! "Select at least two items" is an answer.

use crate::icon::Icon;
use egui::{Key, KeyboardShortcut, Modifiers};

/// The top-level menus, in bar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Menu {
    Board,
    Edit,
    View,
    Preferences,
}

impl Menu {
    pub const ALL: [Self; 4] = [Self::Board, Self::Edit, Self::View, Self::Preferences];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Board => "Board",
            Self::Edit => "Edit",
            Self::View => "View",
            Self::Preferences => "Preferences",
        }
    }

    /// The rows of this menu, in order.
    pub const fn entries(self) -> &'static [Entry] {
        use Command as C;
        use Entry::{Item, Separator, Sub};
        match self {
            Self::Board => const {
                &[
                    Item(C::NewBoard),
                    Item(C::OpenBoard),
                    Item(C::ImportFromMiro),
                    Separator,
                    Item(C::Save),
                    Item(C::SaveAs),
                    Separator,
                    Item(C::StarBoard),
                    Sub(Submenu::MoveToSpace),
                    Item(C::DuplicateBoard),
                    Sub(Submenu::Background),
                    Separator,
                    Sub(Submenu::Export),
                    Separator,
                    Item(C::BoardHistory),
                    Separator,
                    Item(C::DeleteBoard),
                    Item(C::CloseBoard),
                ]
            },
            Self::Edit => const {
                &[
                    Item(C::Undo),
                    Item(C::Redo),
                    Separator,
                    Item(C::CommandPalette),
                    Item(C::Find),
                    Separator,
                    Item(C::Cut),
                    Item(C::Copy),
                    Item(C::Paste),
                    Item(C::Duplicate),
                    Item(C::Delete),
                    Item(C::SelectAll),
                    Separator,
                    Sub(Submenu::Arrange),
                    Sub(Submenu::Agent),
                ]
            },
            Self::View => const {
                &[
                    Item(C::ZoomIn),
                    Item(C::ZoomOut),
                    Item(C::ZoomToFit),
                    Item(C::ZoomToSelection),
                    Item(C::ZoomActualSize),
                    Separator,
                    // The submenu is the *whole* grid control — which pattern, whether it
                    // snaps, what colour it is drawn in. There used to be a `ToggleGrid` row
                    // beside it carrying `⇧⌘G`; it is gone, because its only effect was on a
                    // board that had chosen *No grid*, so on every board that had a pattern
                    // the shortcut did nothing and read as broken. **No grid** is a row in
                    // here now, which is where an off switch belongs — beside the other three
                    // answers to the same question.
                    Sub(Submenu::Grid),
                    Item(C::ToggleMinimap),
                    Item(C::TogglePropertiesPanel),
                    Separator,
                    Item(C::FetchLinkPreviews),
                    Separator,
                    Item(C::GoToStartView),
                    Item(C::SetStartView),
                    Separator,
                    Item(C::PresentationMode),
                ]
            },
            Self::Preferences => const {
                &[
                    Item(C::ToggleTranslucency),
                    Item(C::ToggleAlignObjects),
                    Item(C::ToggleLinkPreviews),
                    Entry::Sub(Submenu::Transparency),
                    Entry::Sub(Submenu::Accent),
                    Separator,
                    // The Agent Canvas band, and it is a **named** section rather than one
                    // more anonymous run between separators — the user's own ask. Every row
                    // in it carries a `Command::note` or a hover written at its draw site,
                    // enforced by `every_agent_row_explains_itself`: these are switches whose
                    // cost or effect is invisible from the switch, which is the kind people
                    // turn on and then report as a defect.
                    Entry::Heading(AGENT_SECTION),
                    Entry::Sub(Submenu::AgentDisplay),
                    Entry::Sub(Submenu::AgentTheme),
                    Entry::Sub(Submenu::Providers),
                    Entry::Sub(Submenu::Voice),
                    Item(C::ToggleWorktrees),
                    Item(C::ToggleBrowserNodes),
                    Separator,
                    Item(C::KeyboardShortcuts),
                    Item(C::Documentation),
                    Item(C::About),
                ]
            },
        }
    }
}

/// What the Agent Canvas band is called, wherever it is drawn.
///
/// One constant, used by Preferences and by the right-button menu, so the layer cannot come
/// to be called two things in two places. **"Agents"**, matching Edit ▸ Agent — the user
/// called it *"the ai / terminal mode"*, which is what it is and not what anything else in
/// this interface is named after. Every other section here is named for the objects it acts
/// on, not for the mode you are in.
pub const AGENT_SECTION: &str = "Agents";

/// One row in a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Item(Command),
    Separator,
    Sub(Submenu),
    /// A named band, drawn as a small faint label above the rows it introduces.
    ///
    /// # Why a heading rather than one more separator
    ///
    /// A separator says *"these belong together"* and nothing else, which is enough for four
    /// clipboard verbs everybody already knows. It is not enough for a band of rows that
    /// belong to a subsystem: *"the settings that are associated to the ai / terminal mode
    /// [should] have its own section"*, said of a Preferences menu where the agent rows sat
    /// between two separators looking exactly like the appearance rows above them.
    ///
    /// # It is not a row
    ///
    /// It cannot be clicked, cannot be ticked and carries no shortcut, so it is skipped by
    /// everything that walks a menu looking for commands — the shortcut sheet, the palette,
    /// and the tests that assert every menu entry is reachable. The `Entry` matches in this
    /// crate are exhaustive, which is what makes that a compile error rather than an
    /// omission.
    Heading(&'static str),
}

/// A nested menu.
///
/// Three of the five are built from data the table cannot hold: the spaces a board can
/// be moved into are the reference folders, the background is a colour and a pattern,
/// and transparency is a continuous value rather than a set of actions. All three
/// return an empty
/// [`Submenu::entries`] and are drawn from runtime state by [`crate::menu`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Submenu {
    Export,
    MoveToSpace,
    Arrange,
    /// Board ▸ Background colour — the canvas's own colour, by name.
    Background,
    /// View ▸ Grid — none, lines or dots, and whether placement snaps to it.
    ///
    /// Split out of Background, which used to carry both. They answer different questions and
    /// Miro files them separately for that reason: a colour is what the board *is*, and a grid
    /// is a drawing aid laid over it. Under View rather than Board for the same reason — one
    /// travels with the document, the other is how you are looking at it today.
    Grid,
    /// Preferences ▸ Transparency — how see-through the floating chrome is.
    Transparency,
    /// Preferences ▸ Accent — which colour selection and the active tool wear.
    ///
    /// Built from runtime state like the three above it, and for the same kind of reason:
    /// the rows are `crate::theme::Accent`'s own values with a tick on the current one, so
    /// the command table would have to hold one command per colour and keep them in step
    /// with that enum.
    Accent,
    /// What the board's grid is drawn in. Hangs off [`Self::Grid`], not off the View
    /// group, because it answers a question only that submenu has raised.
    GridColor,
    /// How see-through the grid is — the same field as [`Self::GridColor`], written on the
    /// alpha channel rather than the other three.
    GridOpacity,
    /// Edit ▸ Agent — everything the selected agent nodes can be asked to do.
    ///
    /// A submenu under Edit rather than a fifth top-level menu: `docs/04-ui-reference.md`
    /// §4's bar is **Board · Edit · View · Preferences**, transcribed from Miro's, and the
    /// four names are the ones the user's hands already know. Its rows are ordinary
    /// commands, which is what makes every row the right button offers also reachable from
    /// the bar — the rule `crate::context_menu`'s own test enforces.
    Agent,
    /// Preferences ▸ Agent output — the app-wide default display mode new nodes inherit.
    ///
    /// Built from runtime state like [`Self::Accent`], and for the same reason: the rows
    /// are `vellum_agent::DisplayMode`'s own values with a tick on the current one, so the
    /// command table would need one command per mode and would have to be kept in step with
    /// that enum.
    AgentDisplay,
    /// Preferences ▸ Agents ▸ Chat theme — how a transcript is dressed, app-wide.
    ///
    /// Runtime state like [`Self::AgentDisplay`], and for the same reason: the rows are
    /// `vellum_agent::ChatTheme`'s own values with a tick on the one in force, so the command
    /// table would need one command per theme and would have to be kept in step with an enum
    /// in another crate.
    AgentTheme,
    /// The **selected node's** theme, on the right button and the `⋮` — *"i should be able
    /// to set individual chats with individual themes"*. The same four rows plus *Inherit*,
    /// which is the row that makes the app-wide default mean anything.
    NodeTheme,
    /// Preferences ▸ Voice — which transcriber a spoken prompt goes to.
    ///
    /// Runtime rows like [`Self::Providers`], for the same reason: the choices are values of
    /// `voice::Preference` plus what this machine happens to have installed, neither of which
    /// the command table can hold.
    Voice,
    /// Preferences ▸ Providers — which models are reachable, and how each one is paid for.
    ///
    /// Runtime state again: the list is `Provider::ALL` crossed with what the app found on
    /// the machine. **No row ever shows a key** — `docs/07-agent-canvas.md` §8a — and a
    /// provider that runs on a subscription the user already holds is drawn as a row that
    /// says so rather than as a sign-in that would take a credential it does not need.
    Providers,
}

impl Submenu {
    pub const ALL: [Self; 15] = [
        Self::Export,
        Self::MoveToSpace,
        Self::Arrange,
        Self::Background,
        Self::Grid,
        Self::Transparency,
        Self::Accent,
        Self::GridColor,
        Self::GridOpacity,
        Self::Agent,
        Self::AgentDisplay,
        Self::AgentTheme,
        Self::NodeTheme,
        Self::Providers,
        Self::Voice,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Export => "Export",
            Self::MoveToSpace => "Move to",
            Self::Arrange => "Arrange",
            Self::Background => "Background colour",
            Self::Grid => "Grid",
            Self::Transparency => "Transparency",
            Self::Accent => "Accent colour",
            Self::GridColor => "Grid colour",
            Self::GridOpacity => "Grid opacity",
            Self::Agent => "Agent",
            Self::AgentDisplay => "Agent output",
            Self::AgentTheme => "Chat theme",
            Self::NodeTheme => "Chat theme",
            Self::Providers => "Providers",
            Self::Voice => "Voice",
        }
    }

    /// The menu this submenu hangs under, which is where its commands are counted.
    pub const fn parent(self) -> Menu {
        match self {
            Self::Export | Self::MoveToSpace | Self::Background => Menu::Board,
            Self::Grid | Self::GridColor | Self::GridOpacity => Menu::View,
            Self::Arrange | Self::Agent => Menu::Edit,
            Self::Transparency
            | Self::Accent
            | Self::AgentDisplay
            | Self::AgentTheme
            | Self::Providers
            | Self::Voice => Menu::Preferences,
            // Beside the other things you do to a selected node, not beside the app-wide
            // default it overrides. Edit ▸ Agent is where a per-node verb belongs.
            Self::NodeTheme => Menu::Edit,
        }
    }

    /// The rows of this submenu, or empty for the two built from runtime state.
    pub const fn entries(self) -> &'static [Entry] {
        use Command as C;
        use Entry::{Item, Separator};
        match self {
            Self::Export => const {
                &[
                    Item(C::ExportPng),
                    Item(C::ExportPdf),
                    Item(C::ExportSvg),
                    Item(C::ExportCsv),
                    Separator,
                    Item(C::ExportBackup),
                ]
            },
            Self::Arrange => const {
                &[
                    Item(C::BringToFront),
                    Item(C::BringForward),
                    Item(C::SendBackward),
                    Item(C::SendToBack),
                    Separator,
                    Item(C::Group),
                    Item(C::Ungroup),
                    Separator,
                    Item(C::Lock),
                    Item(C::Unlock),
                    Separator,
                    Item(C::AlignLeft),
                    Item(C::AlignCenterHorizontal),
                    Item(C::AlignRight),
                    Item(C::AlignTop),
                    Item(C::AlignMiddleVertical),
                    Item(C::AlignBottom),
                    Separator,
                    Item(C::DistributeHorizontally),
                    Item(C::DistributeVertically),
                ]
            },
            // The four pattern rows are drawn from `Pattern::ALL` — runtime state, like
            // the colour lists — but everything *below* them is declared here, so the
            // menu-coverage test can find `SnapToGrid` and the two colour submenus. The
            // leading separator is what divides them from the patterns above.
            Self::Grid => const {
                &[
                    Item(C::SnapToGrid),
                    Separator,
                    Entry::Sub(Self::GridColor),
                    Entry::Sub(Self::GridOpacity),
                ]
            },
            // Ordinary rows, all of them, which is the point: every verb the right button
            // offers on an agent node is a row here too, so a user who never right-clicks
            // still finds it and the shortcut sheet still lists it.
            Self::Agent => const {
                &[
                    Item(C::RunAgent),
                    Item(C::StopAgent),
                    Separator,
                    Item(C::ToggleAgentRaw),
                    Entry::Sub(Self::NodeTheme),
                    Separator,
                    Item(C::EditAgentRules),
                    Item(C::EditAgentSchedule),
                    Item(C::SetTerritory),
                    Item(C::RemoveWorktree),
                    // Also a row inside Chat theme ▸, which is where you reach it while
                    // choosing a look. Here as well because a command reachable only from a
                    // submenu built at runtime is one the shortcut sheet cannot list and
                    // `every_command_appears_in_exactly_one_menu` cannot find — the
                    // "written, tested and unreachable" shape this repository keeps hitting.
                    Item(C::SetChatBackground),
                ]
            },
            Self::MoveToSpace
            | Self::Background
            | Self::Transparency
            | Self::Accent
            | Self::GridColor
            | Self::GridOpacity
            | Self::AgentDisplay
            | Self::AgentTheme
            | Self::NodeTheme
            | Self::Providers
            | Self::Voice => &[],
        }
    }

    /// A sentence shown on hovering the row, [`Command::note`]'s twin for a nested menu.
    ///
    /// Same contract, same reason: every Agent Canvas entry explains itself, because *Agent
    /// output* and *Providers* name concepts that exist nowhere else in this interface, and a
    /// submenu that only says what it is called makes you open it to find out what it is for.
    /// Pinned by `every_agent_menu_entry_carries_a_tooltip`.
    pub const fn note(self) -> Option<&'static str> {
        Some(match self {
            Self::Agent => {
                "Everything the agent nodes you have selected can be asked to do. The same \
                 rows are on the right button, over the node itself."
            }
            Self::AgentDisplay => {
                "What a **new** agent node shows by default — every tool call and reasoning \
                 step, or only the final answer. A node that has been switched by hand keeps \
                 its own setting and ignores this."
            }
            Self::AgentTheme => {
                "How every agent transcript is dressed, unless a node has been given a look \
                 of its own. Four palettes, each with its text contrast already checked \
                 against its own paper."
            }
            Self::NodeTheme => {
                "How this one transcript is dressed — its paper, its ink, how see-through it \
                 is, and a picture of your own behind it. Overrides the app-wide default \
                 until you set it back to Inherit."
            }
            Self::Providers => {
                "Which models can be reached from this machine, and how each one is paid for. \
                 A provider you already hold a subscription to is used through its own \
                 command-line tool and needs no key; the rest need one, stored on this \
                 machine only and never written into a board."
            }
            Self::Voice => {
                "How Velm turns a spoken prompt into words. Hold ⌥D on an agent node that has \
                 voice turned on to talk to it; this chooses what does the listening — a \
                 transcriber on this machine, which is private and needs no key, or an API."
            }
            _ => return None,
        })
    }

    /// Whether the submenu can be opened, and why not when it cannot.
    pub const fn availability(self, ctx: &CommandContext) -> Availability {
        match self {
            Self::Arrange
            | Self::Export
            | Self::Background
            | Self::Grid
            | Self::GridColor
            | Self::GridOpacity => {
                if ctx.board_open {
                    Availability::Enabled
                } else {
                    Availability::Disabled(reason::NO_BOARD)
                }
            }
            Self::MoveToSpace => {
                if !ctx.board_open {
                    Availability::Disabled(reason::NO_BOARD)
                } else if !ctx.board_saved {
                    Availability::Disabled(reason::UNSAVED_BOARD)
                } else if ctx.spaces == 0 {
                    Availability::Disabled(reason::NO_SPACES)
                } else {
                    Availability::Enabled
                }
            }
            // Needs no board — the chrome is translucent over the library too. It does
            // need the material to be in force, and says so rather than offering a
            // slider that moves nothing.
            Self::Transparency => {
                if ctx.transparency_blocked {
                    Availability::Disabled(reason::SYSTEM_OPAQUE)
                } else {
                    Availability::Enabled
                }
            }
            // Always available, and needs no board: the accent is the whole interface's,
            // not a board's, and the board library wears it too — a user who wants the
            // colour changed before opening anything should not have to open something.
            //
            // The two agent preference lists join it. The default display mode and the
            // provider roster are the application's, not a board's, and a user setting up
            // their providers before opening anything should not have to open something.
            Self::Accent
            | Self::AgentDisplay
            | Self::AgentTheme
            | Self::Providers
            | Self::Voice => {
                Availability::Enabled
            }
            // Per node, so it needs a node — and exactly one, because two agents have two
            // themes and there is no shared one to tick.
            Self::NodeTheme => {
                if !ctx.board_open {
                    Availability::Disabled(reason::NO_BOARD)
                } else if ctx.agents_selected == 0 {
                    Availability::Disabled(reason::NO_AGENT)
                } else {
                    Availability::Enabled
                }
            }
            // A submenu whose every row would be greyed out is a menu of things you cannot
            // do. Disabled as a whole, with the same reason its rows would each have given.
            Self::Agent => {
                if !ctx.board_open {
                    Availability::Disabled(reason::NO_BOARD)
                } else if ctx.agents_selected == 0 {
                    Availability::Disabled(reason::NO_AGENT)
                } else {
                    Availability::Enabled
                }
            }
        }
    }
}

/// Why a command is not available right now.
///
/// Written for a tooltip, so each one names the thing the user would have to do. A
/// disabled row that explains itself is the difference between a tool that seems
/// broken and one that is merely waiting.
pub mod reason {
    pub const NO_BOARD: &str = "Open a board first";
    pub const NOTHING_TO_SAVE: &str = "There are no unsaved changes";
    pub const NOTHING_TO_UNDO: &str = "Nothing to undo yet";
    pub const NOTHING_TO_REDO: &str = "Nothing to redo";
    pub const EMPTY_CLIPBOARD: &str = "The clipboard is empty";
    pub const NO_SELECTION: &str = "Select something first";
    pub const NEEDS_TWO: &str = "Select at least two items";
    pub const NEEDS_THREE: &str = "Select at least three items";
    pub const LOCKED: &str = "The selection is locked";
    pub const NOTHING_LOCKED: &str = "Nothing in the selection is locked";
    pub const NO_GROUP: &str = "Select a group";
    pub const UNSAVED_BOARD: &str = "Save the board first";
    pub const NO_SPACES: &str = "No folders yet — add one in the board library";
    pub const SYSTEM_OPAQUE: &str = "Turned off in the system's accessibility settings";
    pub const NO_AGENT: &str = "Select an agent node";
    pub const ONE_AGENT: &str = "Select one agent node";
    /// A worker owns no region, so there is nothing for a sweep to set. Named rather than
    /// hidden: a row that vanishes for two of the three roles reads as a missing feature.
    pub const ONE_MANAGER: &str = "Select one orchestrator or meta agent";
    pub const AGENT_RUNNING: &str = "Already running";
    pub const NO_AGENT_RUNNING: &str = "Nothing selected is running";
    /// Why a hand-off target cannot be offered. Feature 10's rule, stated where the reader
    /// is: a message travels along a connector the user drew, so an agent with no line to
    /// anywhere has nobody to hand off to.
    pub const NO_AGENT_LINK: &str = "Draw a connector to another agent first";
}

/// Whether a command can act, and why not when it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Enabled,
    /// Shown as the row's tooltip. Never empty.
    Disabled(&'static str),
}

impl Availability {
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }

    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Enabled => None,
            Self::Disabled(why) => Some(why),
        }
    }
}

/// A single named action.
///
/// Commands carry no payload: anything with a value — a colour, a zoom factor, a
/// board path — is a different [`UiEvent`](crate::UiEvent) variant. That keeps this
/// enum `Copy` and comparable, which is what makes the menu, the keymap and the
/// palette able to share one list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Command {
    // Board
    NewBoard,
    OpenBoard,
    ImportFromMiro,
    Save,
    SaveAs,
    StarBoard,
    DuplicateBoard,
    ExportPng,
    ExportPdf,
    ExportSvg,
    ExportCsv,
    ExportBackup,
    BoardHistory,
    DeleteBoard,
    CloseBoard,
    // Edit
    Undo,
    Redo,
    CommandPalette,
    Find,
    Cut,
    Copy,
    Paste,
    Duplicate,
    Delete,
    SelectAll,
    // Edit ▸ Arrange
    BringToFront,
    BringForward,
    SendBackward,
    SendToBack,
    Group,
    Ungroup,
    Lock,
    Unlock,
    AlignLeft,
    AlignCenterHorizontal,
    AlignRight,
    AlignTop,
    AlignMiddleVertical,
    AlignBottom,
    DistributeHorizontally,
    DistributeVertically,
    // View
    ZoomIn,
    ZoomOut,
    ZoomToFit,
    ZoomToSelection,
    ZoomActualSize,
    ToggleMinimap,
    /// The docked properties panel, which is **off by default**.
    ///
    /// Miro floats a small toolbar above the selected object rather than docking a
    /// column, and `crate::context_bar` is that toolbar — so the panel stopped being
    /// the only way to reach a property and became the long form of it. Kept, and kept
    /// complete, because it holds controls a horizontal bar has no room for: line
    /// height, vertical alignment, connector anchors, and typed position and size.
    TogglePropertiesPanel,
    GoToStartView,
    SetStartView,
    PresentationMode,
    // Preferences
    ToggleTranslucency,
    /// Whether link cards may fetch their own titles and preview images.
    ///
    /// Off by default, and a *preference* rather than a view toggle: it decides whether the app
    /// talks to third-party servers at all, which is the user's call and not a display choice.
    ToggleLinkPreviews,
    /// Miro's *Align objects*: relative snapping and its guide lines.
    ToggleAlignObjects,
    /// Miro's *Snap to grid*: a move, a resize or a placement lands on the grid the board
    /// is drawn with.
    ///
    /// A **board** row rather than a preference, and it lives in Board ▸ Grid beside the
    /// pattern rows. Which grid it snaps to is the board's own choice, so a switch that
    /// followed the app rather than the board would mean the same setting behaved
    /// differently on the board beside it. Off by default: it is the strict one, which is
    /// the half of Miro's snapping its own forums complain about, and `ToggleAlignObjects`
    /// is the loose one that is on.
    SnapToGrid,
    /// Fill in the selected link cards — or every card on the board when nothing is selected.
    ///
    /// User-initiated on purpose. Fetching on open would tell a third party every link on the
    /// board the moment it was opened; `crate::event::UiEvent` carries no timer for this.
    FetchLinkPreviews,
    // Agent — Edit ▸ Agent
    //
    // Under Edit rather than as a fifth top-level menu. `docs/04-ui-reference.md` §4's tree
    // is **Board · Edit · View · Preferences** and it is the one the user's hands know from
    // Miro; a fifth name is a change to the bar they read, for a submenu's worth of rows.
    /// Start the selected agents.
    RunAgent,
    /// Stop them. A separate command rather than a toggle on [`Self::RunAgent`], because a
    /// mixed selection — one running, one idle — has a sensible answer to each of them and
    /// no sensible answer to "toggle".
    StopAgent,
    /// Show every tool call and reasoning step on the selected nodes rather than only the
    /// answer — feature 2's per-node half. A toggle, so the menu ticks it.
    ToggleAgentRaw,
    /// The three-layer rule cascade for the one selected agent — feature 11.
    EditAgentRules,
    /// When it runs by itself, and what it does afterwards — feature 10.
    EditAgentSchedule,
    /// Arm the territory sweep: the next drag on bare board sets the selected orchestrator's
    /// region.
    ///
    /// A command rather than a bare drag, for feedback 29's reason: a drag on empty board is a
    /// marquee, and taking it whenever a manager happened to be selected would remove marquee
    /// selection from every board with an orchestrator on it — silently, and for the half of
    /// the gesture that gets used constantly.
    SetTerritory,
    /// Remove one agent's git worktree from disk — feature 4's other half.
    ///
    /// **This exists because the interface already claimed it did.** The inspector's own
    /// hover text on the Worktree switch read *"it is removed deliberately, and never while it
    /// has uncommitted work in it"*, describing a deliberation the application could not
    /// perform: `worktree::remove` and `worktree::dirty` were written, tested and called by
    /// nothing, so a board that had run coding agents accumulated checkouts with no way to
    /// clear them but a terminal. Never describe a gesture the user cannot perform.
    ///
    /// A command rather than a panel switch because it **deletes a directory** and therefore
    /// takes a confirmation, and because it must refuse rather than proceed when the worktree
    /// has uncommitted work in it — a decision with a sentence, not a toggle.
    RemoveWorktree,
    /// Put a picture of the user's own behind one agent node's transcript.
    ///
    /// A `Command` rather than an [`AgentEdit`](crate::AgentEdit) because it **opens a file
    /// dialog**, which only the app can do — the chrome never touches the filesystem. The
    /// resulting hash comes back as `AgentEdit::ChatBackground`, so the write itself still
    /// goes through the one path every other node edit does.
    SetChatBackground,
    // Preferences
    /// Whether a browser node may run a real web engine at all — feature 13.
    ///
    /// Off by default, and [`Self::note`] says why on the row rather than leaving the user
    /// to find out by turning it on.
    ToggleBrowserNodes,
    /// Whether each coding agent on this board gets its own `git worktree` — feature 4.
    ///
    /// One switch per project, which is the feature's own wording: *"a single clear toggle
    /// in the project's settings rather than something enabled per-agent inconsistently"*.
    ToggleWorktrees,
    KeyboardShortcuts,
    Documentation,
    About,
}

impl Command {
    /// Every command, in menu order.
    pub const ALL: &'static [Self] = &[
        Self::NewBoard,
        Self::OpenBoard,
        Self::ImportFromMiro,
        Self::Save,
        Self::SaveAs,
        Self::StarBoard,
        Self::DuplicateBoard,
        Self::ExportPng,
        Self::ExportPdf,
        Self::ExportSvg,
        Self::ExportCsv,
        Self::ExportBackup,
        Self::BoardHistory,
        Self::DeleteBoard,
        Self::CloseBoard,
        Self::Undo,
        Self::Redo,
        Self::CommandPalette,
        Self::Find,
        Self::Cut,
        Self::Copy,
        Self::Paste,
        Self::Duplicate,
        Self::Delete,
        Self::SelectAll,
        Self::BringToFront,
        Self::BringForward,
        Self::SendBackward,
        Self::SendToBack,
        Self::Group,
        Self::Ungroup,
        Self::Lock,
        Self::Unlock,
        Self::AlignLeft,
        Self::AlignCenterHorizontal,
        Self::AlignRight,
        Self::AlignTop,
        Self::AlignMiddleVertical,
        Self::AlignBottom,
        Self::DistributeHorizontally,
        Self::DistributeVertically,
        Self::ZoomIn,
        Self::ZoomOut,
        Self::ZoomToFit,
        Self::ZoomToSelection,
        Self::ZoomActualSize,
        Self::ToggleMinimap,
        Self::TogglePropertiesPanel,
        Self::GoToStartView,
        Self::SetStartView,
        Self::PresentationMode,
        Self::ToggleTranslucency,
        Self::ToggleLinkPreviews,
        Self::ToggleAlignObjects,
        Self::SnapToGrid,
        Self::FetchLinkPreviews,
        Self::RunAgent,
        Self::StopAgent,
        Self::ToggleAgentRaw,
        Self::EditAgentRules,
        Self::EditAgentSchedule,
        Self::SetTerritory,
        Self::RemoveWorktree,
        Self::SetChatBackground,
        Self::ToggleBrowserNodes,
        Self::ToggleWorktrees,
        Self::KeyboardShortcuts,
        Self::Documentation,
        Self::About,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::NewBoard => "New board",
            Self::OpenBoard => "Open…",
            Self::ImportFromMiro => "Import from Miro…",
            Self::Save => "Save",
            Self::SaveAs => "Save as…",
            Self::StarBoard => "Star this board",
            Self::DuplicateBoard => "Duplicate board",
            Self::ExportPng => "PNG image…",
            Self::ExportPdf => "PDF…",
            Self::ExportSvg => "SVG…",
            Self::ExportCsv => "Spreadsheet (CSV)…",
            Self::ExportBackup => "Board backup (.velm)…",
            Self::BoardHistory => "History…",
            Self::DeleteBoard => "Delete board…",
            Self::CloseBoard => "Close board",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::CommandPalette => "Commands…",
            Self::Find => "Find…",
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::Duplicate => "Duplicate",
            Self::Delete => "Delete",
            Self::SelectAll => "Select all",
            Self::BringToFront => "Bring to front",
            Self::BringForward => "Bring forward",
            Self::SendBackward => "Send backward",
            Self::SendToBack => "Send to back",
            Self::Group => "Group",
            Self::Ungroup => "Ungroup",
            Self::Lock => "Lock",
            Self::Unlock => "Unlock",
            Self::AlignLeft => "Align left",
            Self::AlignCenterHorizontal => "Align centre",
            Self::AlignRight => "Align right",
            Self::AlignTop => "Align top",
            Self::AlignMiddleVertical => "Align middle",
            Self::AlignBottom => "Align bottom",
            Self::DistributeHorizontally => "Distribute horizontally",
            Self::DistributeVertically => "Distribute vertically",
            Self::ZoomIn => "Zoom in",
            Self::ZoomOut => "Zoom out",
            Self::ZoomToFit => "Zoom to fit",
            Self::ZoomToSelection => "Zoom to selection",
            Self::ZoomActualSize => "Zoom to 100%",
            Self::ToggleMinimap => "Minimap",
            Self::TogglePropertiesPanel => "Properties panel",
            Self::GoToStartView => "Go to start view",
            Self::SetStartView => "Set start view here",
            Self::PresentationMode => "Presentation mode",
            Self::ToggleTranslucency => "Translucent chrome",
            Self::ToggleLinkPreviews => "Fetch link previews",
            // Miro's own wording, so a user who knows the setting there finds it here.
            Self::ToggleAlignObjects => "Align objects",
            Self::SnapToGrid => "Snap to grid",
            Self::FetchLinkPreviews => "Fill in link cards",
            Self::RunAgent => "Run",
            Self::StopAgent => "Stop",
            // Not "Raw mode": the row is a tick beside a noun, and the noun is what the
            // node shows. `DisplayMode::Raw.label()` is the same word, which is what stops
            // the menu and the inspector calling one thing two things.
            Self::ToggleAgentRaw => "Raw output",
            Self::EditAgentRules => "Rules…",
            Self::EditAgentSchedule => "Schedule…",
            Self::SetTerritory => "Set territory…",
            Self::RemoveWorktree => "Remove worktree…",
            Self::SetChatBackground => "Chat picture…",
            Self::ToggleBrowserNodes => "Browser nodes",
            Self::ToggleWorktrees => "Worktree isolation",
            Self::KeyboardShortcuts => "Keyboard shortcuts",
            Self::Documentation => "Documentation",
            Self::About => "About Velm",
        }
    }

    /// The menu the command is listed under, counting a submenu as its parent.
    pub const fn menu(self) -> Menu {
        match self {
            Self::NewBoard
            | Self::OpenBoard
            | Self::ImportFromMiro
            | Self::Save
            | Self::SaveAs
            | Self::StarBoard
            | Self::DuplicateBoard
            | Self::ExportPng
            | Self::ExportPdf
            | Self::ExportSvg
            | Self::ExportCsv
            | Self::ExportBackup
            | Self::BoardHistory
            | Self::DeleteBoard
            | Self::CloseBoard => Menu::Board,
            Self::Undo
            | Self::Redo
            | Self::CommandPalette
            | Self::Find
            | Self::Cut
            | Self::Copy
            | Self::Paste
            | Self::Duplicate
            | Self::Delete
            | Self::SelectAll
            | Self::BringToFront
            | Self::BringForward
            | Self::SendBackward
            | Self::SendToBack
            | Self::Group
            | Self::Ungroup
            | Self::Lock
            | Self::Unlock
            | Self::AlignLeft
            | Self::AlignCenterHorizontal
            | Self::AlignRight
            | Self::AlignTop
            | Self::AlignMiddleVertical
            | Self::AlignBottom
            | Self::DistributeHorizontally
            | Self::DistributeVertically
            | Self::RunAgent
            | Self::StopAgent
            | Self::ToggleAgentRaw
            | Self::EditAgentRules
            | Self::EditAgentSchedule
            | Self::SetTerritory
            | Self::RemoveWorktree
            | Self::SetChatBackground => Menu::Edit,
            Self::ZoomIn
            | Self::ZoomOut
            | Self::ZoomToFit
            | Self::ZoomToSelection
            | Self::ZoomActualSize
            | Self::ToggleMinimap
            | Self::TogglePropertiesPanel
            | Self::FetchLinkPreviews
            | Self::GoToStartView
            | Self::SetStartView
            | Self::PresentationMode => Menu::View,
            // Board, because it is the board's grid it snaps to — it reaches the user
            // through the Grid submenu, which hangs off the View group's row for it.
            Self::SnapToGrid => Menu::View,
            Self::ToggleTranslucency
            | Self::ToggleLinkPreviews
            | Self::ToggleAlignObjects
            | Self::ToggleBrowserNodes
            | Self::ToggleWorktrees
            | Self::KeyboardShortcuts
            | Self::Documentation
            | Self::About => Menu::Preferences,
        }
    }

    /// The trail shown beside a command in the palette — "Board ▸ Export".
    ///
    /// Derived rather than stored, so a command that moves menus cannot keep an old
    /// breadcrumb. Returns the submenu title where there is one.
    pub fn path(self) -> (Menu, Option<&'static str>) {
        let sub = Submenu::ALL
            .into_iter()
            .find(|s| s.entries().contains(&Entry::Item(self)))
            .map(Submenu::title);
        (self.menu(), sub)
    }

    /// The icon shown beside the command where it appears as a button rather than a
    /// menu row. Menu rows are text-only, as they are on both platforms Velm ships
    /// on.
    pub const fn icon(self) -> Option<Icon> {
        Some(match self {
            Self::ImportFromMiro => Icon::Import,
            Self::Duplicate => Icon::Duplicate,
            Self::Delete | Self::DeleteBoard => Icon::Trash,
            Self::StarBoard => Icon::Star,
            Self::Find | Self::CommandPalette => Icon::Search,
            Self::ZoomIn => Icon::ZoomIn,
            Self::ZoomOut => Icon::ZoomOut,
            Self::ZoomToFit => Icon::ZoomToFit,
            Self::ToggleMinimap => Icon::Minimap,
            Self::SnapToGrid => Icon::Grid,
            Self::PresentationMode => Icon::Present,
            Self::BringToFront => Icon::BringToFront,
            Self::BringForward => Icon::BringForward,
            Self::SendBackward => Icon::SendBackward,
            Self::SendToBack => Icon::SendToBack,
            Self::Group => Icon::Group,
            Self::Ungroup => Icon::Ungroup,
            Self::Lock => Icon::Lock,
            Self::Unlock => Icon::Unlock,
            Self::AlignLeft => Icon::AlignLeft,
            Self::AlignCenterHorizontal => Icon::AlignCenterHorizontal,
            Self::AlignRight => Icon::AlignRight,
            Self::AlignTop => Icon::AlignTop,
            Self::AlignMiddleVertical => Icon::AlignMiddleVertical,
            Self::AlignBottom => Icon::AlignBottom,
            Self::DistributeHorizontally => Icon::DistributeHorizontal,
            Self::DistributeVertically => Icon::DistributeVertical,
            Self::NewBoard => Icon::Plus,
            Self::Undo => Icon::Undo,
            Self::Redo => Icon::Redo,
            Self::RunAgent => Icon::Play,
            Self::StopAgent => Icon::Stop,
            Self::EditAgentRules
            | Self::EditAgentSchedule
            | Self::SetTerritory
            | Self::SetChatBackground => Icon::Agent,
            Self::ToggleBrowserNodes => Icon::Browser,
            _ => return None,
        })
    }

    /// A sentence shown on hovering the row **while it is enabled**.
    ///
    /// Distinct from [`Availability::reason`], which says why a row cannot be clicked. This
    /// says what a row that *can* be clicked will actually do, or why it is set the way it
    /// is. `docs/07-agent-canvas.md` §0's third rule asks for a legible explanation rather
    /// than a bare toggle; a switch with no stated cost is how somebody turns on a feature
    /// that spends 150MB a page and reports it as a memory leak.
    ///
    /// # Every Agent Canvas command carries one, and that is a contract
    ///
    /// *"for every future that is for the terminal / ai mode i want a tool tip"*. The layer
    /// is the one part of Velm whose rows do not explain themselves from their labels — every
    /// other menu in this application acts on a rectangle you are looking at, and *Rules…*,
    /// *Schedule…* and *Set territory…* each name a concept that exists nowhere else in the
    /// interface. `every_agent_command_carries_a_tooltip` pins it, so the next agent command
    /// cannot ship mute.
    ///
    /// Two rules the sentences follow, both learned the hard way:
    ///
    /// - **Never describe a gesture that does not exist** — feedback 36's worst finding was
    ///   three strings instructing the user to do things nothing implemented.
    /// - **Say what it does, not how to use it.** A tooltip is read in about a second, over
    ///   the row, by someone who has already decided to click.
    ///
    /// Deliberately **not** on every command in the application. A tooltip on *Copy* is noise
    /// that trains people to ignore tooltips.
    pub const fn note(self) -> Option<&'static str> {
        Some(match self {
            // ----- the Agent Canvas ------------------------------------------------------
            Self::RunAgent => {
                "Starts the selected agents on whatever is typed in their prompt row, or \
                 re-runs their last instruction if it is empty. The reply streams onto the \
                 node itself; nothing an agent says is written into the board file."
            }
            Self::StopAgent => {
                "Ends the turn in progress and leaves the process running, so the next prompt \
                 does not pay for a fresh start. What has already been said stays on the node."
            }
            Self::ToggleAgentRaw => {
                "Raw shows every tool call, command and reasoning step as it happens. Clean \
                 shows only what the agent finally answered. It changes what is displayed and \
                 nothing about what the agent does."
            }
            Self::EditAgentRules => {
                "The standing instructions this agent runs under. Three layers apply in order \
                 — everything on this machine, then this project, then this one agent — and a \
                 narrower layer overrides a wider one. The two outer layers are ordinary \
                 files you can also edit by hand."
            }
            Self::EditAgentSchedule => {
                "Run this agent on a timer — hourly, daily at a time you set, or on an \
                 interval — instead of only when you ask. A scheduled run behaves exactly \
                 like pressing Run, and the schedule stops when the board is closed."
            }
            Self::SetChatBackground => {
                "Choose an image to sit behind this agent's transcript. It is copied into \
                 the board rather than linked from your disk, so the board still looks \
                 right on another machine, and the text keeps its own paper over it so a \
                 picture cannot make the words unreadable."
            }
            Self::SetTerritory => {
                "Draw the region of board this orchestrator owns. It may only create agents \
                 inside that rectangle, which is what stops one spreading over work you have \
                 elsewhere. The limit is enforced by Velm, not asked of the model."
            }
            Self::RemoveWorktree => {
                "Delete this agent's own checkout of the repository. Turning worktree \
                 isolation off does not do this — the directory stays, because a checkout is \
                 not something a preference may throw away. Refused while it holds \
                 uncommitted work."
            }
            Self::ToggleBrowserNodes => {
                "Off by default. A browser node runs a real web engine, which costs roughly \
                 60–150MB of memory per page while it sits there and more while it plays. \
                 With this off, a browser node draws as a card that offers to open the page \
                 in your own browser."
            }
            Self::ToggleWorktrees => {
                "Off by default. Each coding agent on this board gets its own `git worktree` \
                 — a second checkout on disk, on its own branch, so two agents cannot edit \
                 the same file underneath each other. Removing one is deliberate: a \
                 worktree with uncommitted work in it is never force-removed."
            }
            _ => return None,
        })
    }

    /// The default binding, or `None` for commands reached only through a menu.
    ///
    /// `Modifiers::COMMAND` is ⌘ on macOS and Ctrl elsewhere, which is what makes
    /// one table serve both platforms. The bindings match Miro's wherever Miro has
    /// one, because `docs/features/README.md` §9 makes transferring muscle memory an
    /// explicit goal — including `⌘K` for the palette and `⌘F` for find, both of
    /// which `docs/04-ui-reference.md` §4 records in Miro's own Edit menu.
    pub const fn shortcut(self) -> Option<KeyboardShortcut> {
        const CMD: Modifiers = Modifiers::COMMAND;
        const SHIFT_CMD: Modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);
        const ALT_CMD: Modifiers = Modifiers::COMMAND.plus(Modifiers::ALT);

        let (modifiers, key) = match self {
            Self::NewBoard => (CMD, Key::N),
            Self::OpenBoard => (CMD, Key::O),
            Self::Save => (CMD, Key::S),
            Self::SaveAs => (SHIFT_CMD, Key::S),
            Self::CloseBoard => (CMD, Key::W),
            Self::Undo => (CMD, Key::Z),
            Self::Redo => (SHIFT_CMD, Key::Z),
            Self::CommandPalette => (CMD, Key::K),
            Self::Find => (CMD, Key::F),
            Self::Cut => (CMD, Key::X),
            Self::Copy => (CMD, Key::C),
            Self::Paste => (CMD, Key::V),
            Self::Duplicate => (CMD, Key::D),
            Self::Delete => (Modifiers::NONE, Key::Delete),
            Self::SelectAll => (CMD, Key::A),
            Self::ZoomIn => (CMD, Key::Plus),
            Self::ZoomOut => (CMD, Key::Minus),
            Self::ZoomToFit => (SHIFT_CMD, Key::Num1),
            Self::ZoomToSelection => (SHIFT_CMD, Key::Num2),
            Self::ZoomActualSize => (CMD, Key::Num0),
            Self::ToggleMinimap => (ALT_CMD, Key::M),
            Self::TogglePropertiesPanel => (ALT_CMD, Key::P),
            Self::PresentationMode => (SHIFT_CMD, Key::P),
            Self::BringToFront => (SHIFT_CMD, Key::CloseBracket),
            Self::BringForward => (CMD, Key::CloseBracket),
            Self::SendBackward => (CMD, Key::OpenBracket),
            Self::SendToBack => (SHIFT_CMD, Key::OpenBracket),
            Self::Group => (CMD, Key::G),
            Self::Ungroup => (SHIFT_CMD, Key::U),
            Self::Lock => (SHIFT_CMD, Key::L),
            Self::Unlock => (ALT_CMD, Key::L),
            _ => return None,
        };
        Some(KeyboardShortcut::new(modifiers, key))
    }

    /// A second binding that also fires the command, and that the menu does *not*
    /// show.
    ///
    /// Two commands have one, and both are cases where two populations disagree about
    /// what the key should be:
    ///
    /// - **Redo.** Miro binds `⌘Y`, which `docs/04-ui-reference.md` §4 records and
    ///   calls the odd one out; `⌘⇧Z` is the macOS convention. Binding both costs
    ///   nothing and the user's fingers already know Miro's. The menu shows the
    ///   convention, so the app does not teach a habit that works nowhere else.
    /// - **Delete.** `Delete` on a full keyboard is forward-delete, and the key a Mac
    ///   laptop actually has is `Backspace`. A user pressing the only delete key on
    ///   their machine and watching nothing happen is not a shortcut, it is a bug.
    pub const fn alternate_shortcut(self) -> Option<KeyboardShortcut> {
        let (modifiers, key) = match self {
            Self::Redo => (Modifiers::COMMAND, Key::Y),
            Self::Delete => (Modifiers::NONE, Key::Backspace),
            _ => return None,
        };
        Some(KeyboardShortcut::new(modifiers, key))
    }

    /// Whether the command can act right now, and why not when it cannot.
    pub const fn availability(self, ctx: &CommandContext) -> Availability {
        use Availability::{Disabled, Enabled};
        /// `Enabled` when `ok`, otherwise disabled with `why`.
        const fn gate(ok: bool, why: &'static str) -> Availability {
            if ok { Availability::Enabled } else { Availability::Disabled(why) }
        }

        match self {
            // Available with no board at all: the ones that make a board, find a
            // board, or explain the app.
            Self::NewBoard
            | Self::OpenBoard
            | Self::ImportFromMiro
            | Self::CommandPalette
            | Self::KeyboardShortcuts
            | Self::Documentation
            | Self::About => Enabled,

            // The material can be refused by the OS, and an in-app switch that
            // silently loses to it is worse than one that says so —
            // `docs/05-design-language.md` §3a asks for the override, not for a lie.
            Self::ToggleTranslucency => gate(!ctx.transparency_blocked, reason::SYSTEM_OPAQUE),
            // Always offerable: it is the switch that decides whether anything is fetched, so
            // it cannot be gated on a fetch being possible. `ToggleBrowserNodes` joins them
            // for the same reason — it is the permission, so it cannot need the thing it
            // permits to already exist.
            Self::ToggleLinkPreviews | Self::ToggleAlignObjects | Self::ToggleBrowserNodes => {
                Enabled
            }
            // Gated on a board, unlike the three above: there is no grid to snap to without
            // one, and worktree isolation is **per project** — the open board is the
            // project, so on the library screen there is nothing for the switch to be about.
            Self::SnapToGrid => gate(ctx.board_open, "Open a board first"),
            // **And on git existing.** `worktree::available()` was written specifically so
            // this row could grey itself out and say why — its own doc says so — and it had no
            // caller, so on a machine without git the switch was offered, accepted, stored,
            // and failed at the first Start with an error about a command the user never
            // typed. A switch that cannot work is worse than an absent one, because flipping
            // it looks like it worked.
            Self::ToggleWorktrees => match (ctx.board_open, ctx.git_available) {
                (false, _) => Disabled(reason::NO_BOARD),
                (_, false) => Disabled("git is not installed"),
                _ => Enabled,
            },
            Self::FetchLinkPreviews => gate(ctx.board_open, reason::NO_BOARD),

            _ if !ctx.board_open => Disabled(reason::NO_BOARD),

            Self::Save => gate(ctx.dirty, reason::NOTHING_TO_SAVE),
            Self::Undo => gate(ctx.can_undo, reason::NOTHING_TO_UNDO),
            Self::Redo => gate(ctx.can_redo, reason::NOTHING_TO_REDO),
            Self::Paste => gate(ctx.clipboard_has_content, reason::EMPTY_CLIPBOARD),

            // A board with no file behind it is not in the library, so it cannot be
            // starred, filed, copied or deleted there.
            Self::StarBoard | Self::DuplicateBoard | Self::DeleteBoard => {
                gate(ctx.board_saved, reason::UNSAVED_BOARD)
            }

            Self::SaveAs
            | Self::ExportPng
            | Self::ExportPdf
            | Self::ExportSvg
            | Self::ExportCsv
            | Self::ExportBackup
            | Self::BoardHistory
            | Self::CloseBoard
            | Self::Find
            | Self::SelectAll
            | Self::ZoomIn
            | Self::ZoomOut
            | Self::ZoomToFit
            | Self::ZoomActualSize
            | Self::ToggleMinimap
            | Self::TogglePropertiesPanel
            | Self::GoToStartView
            | Self::SetStartView
            | Self::PresentationMode => Enabled,

            // Anything that acts on the selection needs one, and refuses to act on a
            // locked one — a locked item that still responds to Delete is not locked.
            Self::Cut | Self::Copy | Self::Duplicate | Self::ZoomToSelection => {
                gate(ctx.selected > 0, reason::NO_SELECTION)
            }
            Self::Delete
            | Self::BringToFront
            | Self::BringForward
            | Self::SendBackward
            | Self::SendToBack
            | Self::Lock => {
                if ctx.selected == 0 {
                    Disabled(reason::NO_SELECTION)
                } else {
                    gate(!ctx.all_locked, reason::LOCKED)
                }
            }
            Self::Unlock => gate(ctx.any_locked, reason::NOTHING_LOCKED),

            // Run and Stop are two commands rather than one toggle, so each is gated on the
            // half of the selection it can actually act on: Run is available while
            // *anything* selected is idle, Stop while anything is running. A mixed selection
            // offers both, which is the only honest answer — a toggle would have to pick one
            // and be wrong about the rest.
            Self::RunAgent => {
                if ctx.agents_selected == 0 {
                    Disabled(reason::NO_AGENT)
                } else {
                    gate(!ctx.all_agents_running, reason::AGENT_RUNNING)
                }
            }
            Self::StopAgent => {
                if ctx.agents_selected == 0 {
                    Disabled(reason::NO_AGENT)
                } else {
                    gate(ctx.any_agent_running, reason::NO_AGENT_RUNNING)
                }
            }
            Self::ToggleAgentRaw => gate(ctx.agents_selected > 0, reason::NO_AGENT),
            // One node. A rule cascade and a schedule are single-node configuration —
            // `crate::event::AgentEdit` says why — so the editors that produce them are
            // offered for exactly one selected agent rather than for a selection that
            // happens to contain one.
            Self::EditAgentRules | Self::EditAgentSchedule | Self::SetChatBackground => {
                gate(ctx.agents_selected == 1, reason::ONE_AGENT)
            }

            // A territory belongs to a role that can spawn into it. Offering the sweep for a
            // worker would let the user draw a rectangle that nothing would ever read.
            Self::SetTerritory => {
                gate(ctx.agents_selected == 1 && ctx.manager_selected, reason::ONE_MANAGER)
            }

            // Exactly one agent, and one that has a worktree: this deletes a specific
            // directory, and offering it for a node with none is a button whose only possible
            // answer is "there is nothing there".
            Self::RemoveWorktree => gate(
                ctx.agents_selected == 1 && ctx.worktree_selected,
                "This agent has no worktree of its own",
            ),

            Self::Group => gate(ctx.selected > 1, reason::NEEDS_TWO),
            Self::Ungroup => gate(ctx.any_group, reason::NO_GROUP),

            // Aligning one item against itself is a no-op; distributing needs a
            // middle item to move, so it takes three.
            Self::AlignLeft
            | Self::AlignCenterHorizontal
            | Self::AlignRight
            | Self::AlignTop
            | Self::AlignMiddleVertical
            | Self::AlignBottom => {
                if ctx.selected < 2 {
                    Disabled(reason::NEEDS_TWO)
                } else {
                    gate(!ctx.all_locked, reason::LOCKED)
                }
            }
            Self::DistributeHorizontally | Self::DistributeVertically => {
                if ctx.selected < 3 {
                    Disabled(reason::NEEDS_THREE)
                } else {
                    gate(!ctx.all_locked, reason::LOCKED)
                }
            }
        }
    }

    /// Whether the command is available given what is on screen.
    pub const fn is_enabled(self, ctx: &CommandContext) -> bool {
        self.availability(ctx).is_enabled()
    }

    /// True for commands that change the document, so the app can close an open text
    /// session before dispatching one.
    ///
    /// # Why this exists, and why it is a list rather than "everything"
    ///
    /// The on-canvas caret holds a Loro undo group open for the whole editing session on
    /// purpose — that is what makes a typed word one `⌘Z` rather than one per letter. Loro
    /// has no depth count, so a *second* grouped operation while that one is open answers
    /// `UndoGroupAlreadyStarted`, and nothing else ever closes a group: one failure breaks
    /// move, delete, paste, align and restyle for the rest of the session. The user hit it
    /// as *"when I type or when I delete something it gives an error"* — type one character
    /// in a sticky, then choose Delete from a menu.
    ///
    /// Committing before **every** command would be wrong, not merely wasteful: `Undo` and
    /// `Redo` are meant to act on the session, `Find` and the palette are meant to leave the
    /// caret where it is, and a view toggle has no business ending an edit. So this names
    /// the verbs that touch the board, and nothing else.
    ///
    /// `SelectAll` is deliberately **absent**: with a caret on the board it selects the
    /// characters, not the items, and committing would end the edit it is acting inside.
    /// The four a **canvas text session** implements for itself, and which the chrome must
    /// therefore not also run.
    ///
    /// `ActiveState::type_key` and its two siblings claim `⌘A`, `⌘C`, `⌘X` and `⌘V` for the
    /// *characters* — but claiming a key at the app layer does not take it out of egui's
    /// queue, and for the clipboard three `Shell::restore_clipboard_key` deliberately
    /// **puts one back**. So with only the ⌘/⌃ rule in [`crate::Chrome`]'s table, each of
    /// these fired twice from one press: the session cut the selected characters and the
    /// table cut the item they were in. `Cut` and `Paste` are in [`Self::mutates_board`],
    /// so the second half also *ended the edit first* — which is feedback 40's Backspace
    /// one chord over, and it would have shipped with the fix for it.
    ///
    /// **Undo and Redo are deliberately not here.** No session implements them, so letting
    /// them through is the whole point of drawing the line at ⌘ rather than at the table:
    /// `⌘Z` mid-word is the moment it is most wanted. This is the same list
    /// `crate::menubar`'s accelerator ban keeps for the same reason, minus those two.
    pub const fn claimed_by_a_text_session(self) -> bool {
        matches!(self, Self::SelectAll | Self::Copy | Self::Cut | Self::Paste)
    }

    pub const fn mutates_board(self) -> bool {
        matches!(
            self,
            Self::Cut
                | Self::Paste
                | Self::Duplicate
                | Self::Delete
                | Self::BringToFront
                | Self::BringForward
                | Self::SendBackward
                | Self::SendToBack
                | Self::Group
                | Self::Ungroup
                | Self::Lock
                | Self::Unlock
                | Self::AlignLeft
                | Self::AlignCenterHorizontal
                | Self::AlignRight
                | Self::AlignTop
                | Self::AlignMiddleVertical
                | Self::AlignBottom
                | Self::DistributeHorizontally
                | Self::DistributeVertically
                // Writes `AgentModel::display` into the node's token, which is a document
                // edit like any other. Run and Stop are **not** here: a transcript lives in
                // a sidecar outside the document (`docs/07-agent-canvas.md` §4), so starting
                // an agent changes no board and must not end an edit in progress.
                | Self::ToggleAgentRaw
        )
    }

    /// True for commands that toggle rather than act, so the menu can draw a tick.
    pub const fn is_toggle(self) -> bool {
        matches!(
            self,
            Self::ToggleMinimap
                | Self::TogglePropertiesPanel
                | Self::PresentationMode
                | Self::StarBoard
                | Self::ToggleTranslucency
                | Self::ToggleLinkPreviews
                | Self::ToggleAlignObjects
                | Self::SnapToGrid
                | Self::ToggleAgentRaw
                | Self::ToggleBrowserNodes
                | Self::ToggleWorktrees
        )
    }
}

/// What the chrome knows about the app's state when deciding what is available.
///
/// Everything here is cheap for the app to supply and none of it requires the UI to
/// hold a reference to the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommandContext {
    pub board_open: bool,
    /// Whether the open board has a file behind it. A board that has never been saved
    /// has no library row, so it cannot be starred, filed or deleted there.
    pub board_saved: bool,
    /// Whether the open board is starred, for the tick beside *Star this board*.
    pub board_starred: bool,
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub clipboard_has_content: bool,
    pub selected: usize,
    pub any_locked: bool,
    pub all_locked: bool,
    pub any_group: bool,
    /// How many spaces the library holds, for *Move to*.
    pub spaces: usize,
    /// The OS has switched translucency off — Reduce Transparency or high contrast —
    /// so the in-app override cannot turn it back on.
    pub transparency_blocked: bool,
    /// How many selected items are agent nodes.
    ///
    /// Counted rather than inferred from `selected`, because a selection can hold an agent
    /// and four stickies and the agent verbs still apply to the one — the same abstention
    /// rule `crate::selection::Field` applies to every other property.
    pub agents_selected: usize,
    /// Whether the one selected agent is an orchestrator or the meta agent — a role that owns
    /// a region. Its own field rather than a `RoleKind` because the only question anybody asks
    /// of it is this one.
    pub manager_selected: bool,
    /// Whether the one selected agent has a git worktree on disk right now.
    ///
    /// Read from `SelectionItem::worktree`'s `At(_)` state rather than from the node's
    /// `worktree` switch: the switch says *should have one*, and this says *has one*. Removing
    /// needs the second — a node switched on but never run has nothing to delete.
    pub worktree_selected: bool,
    /// Whether `git` is on this machine at all.
    ///
    /// Probed once per run rather than per frame — it spawns `git --version` — and false only
    /// disables a switch, never hides one: the row stays visible so the feature is still
    /// discoverable, with a reason where its tick would be.
    pub git_available: bool,
    pub any_agent_running: bool,
    /// True only when at least one agent is selected and every one of them is running,
    /// which is what makes *Run* refuse rather than start something twice.
    pub all_agents_running: bool,
}

/// Whether `git` is installed, asked once and remembered.
///
/// Here rather than on the app side because the answer is a property of the machine, not of a
/// board — and because the one consumer is this file's own availability table. `OnceLock`
/// because the gate is evaluated on every frame that draws a menu and the probe spawns a
/// process; git does not appear halfway through a session.
pub fn git_available() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(vellum_agent::worktree::available)
}

/// Renders a shortcut for display.
///
/// **Modifiers are spelled out; the key is not.** [`egui::ModifierNames::SYMBOLS`]
/// would give the macOS ⌘⌥⇧ glyphs, but egui's own documentation notes they are
/// missing from the bundled font and they would render as tofu. Spelled-out names
/// always draw, on both platforms.
///
/// That reasoning covers the modifiers and stops there, which is the bug this
/// replaces. Asking egui for the whole thing takes the long form for the *key* too,
/// so `⌘]` printed as `Cmd+CloseBracket`, `⌘[` as `Cmd+OpenBracket`, and zoom as
/// `Cmd+Plus` and `Cmd+Minus` — the internal spelling of a variant, shown to the user
/// as though it were a key they could find. `[`, `]`, `+` and `−` are plain ASCII and
/// in every font, so there is nothing to fall back from.
pub fn format_shortcut(shortcut: KeyboardShortcut, is_mac: bool) -> String {
    let mut out = egui::ModifierNames::NAMES.format(&shortcut.modifiers, is_mac);
    if !out.is_empty() {
        out += egui::ModifierNames::NAMES.concat;
    }
    out += shortcut.logical_key.symbol_or_name();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The exclusions are the load-bearing half of [`Command::mutates_board`], so they are
    /// what this asserts.
    ///
    /// The inclusions are self-evident from the list; the exclusions are the ones a later
    /// change would get wrong, because "commit the caret before acting" reads as something
    /// you would want to do *always*. Each of these three families breaks if it does:
    /// `Undo`/`Redo` are meant to act on the session that is open, `Find` and the palette are
    /// meant to leave the caret alone, a view toggle has no business ending an edit, and
    /// `SelectAll` with a caret up selects **characters** — committing would end the very
    /// edit it is acting inside.
    #[test]
    fn the_commands_that_must_not_close_an_open_caret_are_named() {
        for command in [
            Command::Undo,
            Command::Redo,
            Command::Find,
            Command::CommandPalette,
            Command::SelectAll,
            Command::Copy,
            Command::SnapToGrid,
            Command::ZoomToFit,
            Command::TogglePropertiesPanel,
        ] {
            assert!(
                !command.mutates_board(),
                "{:?} would close an open text session, which is not what it means",
                command.label()
            );
        }
    }

    /// Every verb that opens its own undo group is covered.
    ///
    /// A grouped operation dispatched while the caret's group is open fails and leaves that
    /// group open, which breaks *every* later grouped operation for the session — the user's
    /// *"when i type or when i delete something it gives an error"*. Missing one here is how
    /// that comes back, and it comes back silently.
    #[test]
    fn every_board_mutating_verb_closes_an_open_caret_first() {
        for command in [
            Command::Cut,
            Command::Paste,
            Command::Duplicate,
            Command::Delete,
            Command::BringToFront,
            Command::SendToBack,
            Command::Group,
            Command::Ungroup,
            Command::Lock,
            Command::Unlock,
            Command::AlignLeft,
            Command::AlignBottom,
            Command::DistributeHorizontally,
            Command::DistributeVertically,
        ] {
            assert!(
                command.mutates_board(),
                "{:?} changes the document and must close an open text session first",
                command.label()
            );
        }
    }

    /// Every command listed anywhere in the tree, with the menu it was found under.
    fn listed() -> Vec<(Command, Menu)> {
        let mut found = Vec::new();
        for menu in Menu::ALL {
            for entry in menu.entries() {
                match entry {
                    Entry::Item(command) => found.push((*command, menu)),
                    // Neither is a command, so neither is listed. A heading in particular
                    // must not be: it has no label(), no shortcut and nothing to run, and
                    // counting one would put a row in the shortcut sheet that cannot be
                    // pressed.
                    Entry::Separator | Entry::Heading(_) => {}
                    Entry::Sub(sub) => {
                        assert_eq!(sub.parent(), menu, "{sub:?} is drawn under {menu:?}");
                        for entry in sub.entries() {
                            if let Entry::Item(command) = entry {
                                found.push((*command, menu));
                            }
                        }
                    }
                }
            }
        }
        found
    }

    /// The menus are the only way to reach most commands, so a command missing from
    /// them is unreachable, and one listed twice is a copy-paste slip.
    #[test]
    fn every_command_appears_in_exactly_one_menu_in_the_menu_it_declares() {
        let mut seen: Vec<Command> = Vec::new();
        for (command, menu) in listed() {
            assert_eq!(command.menu(), menu, "{command:?} is listed under {menu:?}");
            assert!(!seen.contains(&command), "{command:?} is listed twice");
            seen.push(command);
        }
        for command in Command::ALL {
            assert!(seen.contains(command), "{command:?} is in no menu");
        }
        assert_eq!(seen.len(), Command::ALL.len());
    }

    /// `docs/04-ui-reference.md` §4's tree, which is the one the user's hands know.
    #[test]
    fn the_bar_reads_board_edit_view_preferences() {
        assert_eq!(
            Menu::ALL.map(Menu::title),
            ["Board", "Edit", "View", "Preferences"]
        );
    }

    /// The export targets §4 commits to, replacing Miro's cloud-only entries.
    #[test]
    fn export_offers_the_four_formats_and_the_backup() {
        let labels: Vec<&str> = Submenu::Export
            .entries()
            .iter()
            .filter_map(|e| match e {
                Entry::Item(c) => Some(c.label()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "PNG image…",
                "PDF…",
                "SVG…",
                "Spreadsheet (CSV)…",
                "Board backup (.velm)…",
            ]
        );
        // …and none of the entries that need an account or a server survived the
        // transcription. Named exactly, because "presentation mode" is a local feature
        // and "share as presentation" is not.
        let labels: Vec<String> =
            Command::ALL.iter().map(|c| c.label().to_lowercase()).collect();
        for cloud in [
            "google drive",
            "save to google drive",
            "embed",
            "catch up",
            "share as presentation",
        ] {
            assert!(
                !labels.iter().any(|label| label.contains(cloud)),
                "the cloud entry {cloud:?} survived"
            );
        }
    }

    /// Both were in Miro's Edit menu and both were missing from this table.
    #[test]
    fn the_palette_and_find_are_bound_to_the_keys_miro_binds_them_to() {
        assert_eq!(
            Command::CommandPalette.shortcut(),
            Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::K))
        );
        assert_eq!(
            Command::Find.shortcut(),
            Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::F))
        );
        assert_eq!(Command::CommandPalette.menu(), Menu::Edit);
        assert_eq!(Command::Find.menu(), Menu::Edit);
    }

    /// Two commands on one binding means one of them silently never fires. The
    /// alternates are in the same namespace as the primaries, so they are checked
    /// together — `⌘Y` colliding with something in the Board menu would be exactly as
    /// broken as two primaries colliding, and far easier to miss.
    #[test]
    fn no_two_commands_share_a_shortcut_including_the_alternates() {
        let mut seen = HashSet::new();
        for command in Command::ALL {
            for shortcut in
                [command.shortcut(), command.alternate_shortcut()].into_iter().flatten()
            {
                assert!(
                    seen.insert((shortcut.modifiers, shortcut.logical_key)),
                    "{command:?} reuses {}",
                    format_shortcut(shortcut, true)
                );
            }
        }
    }

    /// `docs/04-ui-reference.md` §4 makes this explicit: both bindings, because the
    /// user's muscle memory is Miro's and the platform's convention is the other one.
    #[test]
    fn redo_answers_to_both_the_miro_binding_and_the_macos_one() {
        let macos = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z);
        let miro = KeyboardShortcut::new(Modifiers::COMMAND, Key::Y);
        let bound = [Command::Redo.shortcut(), Command::Redo.alternate_shortcut()];
        assert!(bound.contains(&Some(macos)), "Cmd+Shift+Z is unbound");
        assert!(bound.contains(&Some(miro)), "Cmd+Y is unbound");
        // The menu shows one of them, and it is the convention rather than Miro's.
        assert_eq!(Command::Redo.shortcut(), Some(macos));
    }

    /// The delete key a Mac laptop actually has.
    #[test]
    fn delete_answers_to_backspace_as_well_as_to_forward_delete() {
        assert_eq!(
            Command::Delete.alternate_shortcut(),
            Some(KeyboardShortcut::new(Modifiers::NONE, Key::Backspace))
        );
    }

    /// Only the two documented commands carry a second binding. A third appearing
    /// without a reason in `alternate_shortcut`'s doc comment is drift.
    #[test]
    fn exactly_two_commands_carry_an_alternate() {
        let with_alternates: Vec<_> = Command::ALL
            .iter()
            .copied()
            .filter(|c| c.alternate_shortcut().is_some())
            .collect();
        assert_eq!(with_alternates, vec![Command::Redo, Command::Delete]);
    }

    #[test]
    fn no_menu_starts_or_ends_with_a_separator() {
        let is_separator = |e: &Entry| matches!(e, Entry::Separator);
        for entries in Menu::ALL
            .into_iter()
            .map(Menu::entries)
            .chain(Submenu::ALL.into_iter().map(Submenu::entries))
            .filter(|e| !e.is_empty())
        {
            assert!(!is_separator(&entries[0]), "{entries:?} opens with a rule");
            assert!(!is_separator(&entries[entries.len() - 1]), "{entries:?} ends with a rule");
            assert!(
                !entries.windows(2).any(|w| is_separator(&w[0]) && is_separator(&w[1])),
                "{entries:?} has a doubled separator"
            );
        }
    }

    #[test]
    fn nothing_that_needs_a_board_is_available_without_one() {
        let ctx = CommandContext::default();
        let available: Vec<_> =
            Command::ALL.iter().copied().filter(|c| c.is_enabled(&ctx)).collect();
        assert_eq!(
            available,
            vec![
                Command::NewBoard,
                Command::OpenBoard,
                Command::ImportFromMiro,
                Command::CommandPalette,
                Command::ToggleTranslucency,
                // The switch that decides whether anything is fetched at all cannot be gated
                // on a board being open: it is a preference, not an action on a board.
                Command::ToggleLinkPreviews,
                // Snapping is a preference too, and it is one you set *before* opening
                // the board you want it on.
                Command::ToggleAlignObjects,
                // Browser nodes are an application preference living in the library
                // sidecar, not a board's, so the row is offered with no board open —
                // exactly as translucency and link previews are.
                Command::ToggleBrowserNodes,
                Command::KeyboardShortcuts,
                Command::Documentation,
                Command::About,
            ]
        );
        // …and each of the rest says why, rather than being inertly grey.
        for command in Command::ALL.iter().filter(|c| !c.is_enabled(&ctx)) {
            assert_eq!(
                command.availability(&ctx).reason(),
                Some(reason::NO_BOARD),
                "{command:?}"
            );
        }
    }

    /// The rule the whole `Availability` type exists for: a row the user cannot click
    /// must be able to say why. An empty reason is the same as no reason.
    #[test]
    fn every_disabled_command_and_submenu_carries_a_reason() {
        let contexts = [
            CommandContext::default(),
            CommandContext { board_open: true, ..CommandContext::default() },
            CommandContext {
                board_open: true,
                board_saved: true,
                selected: 1,
                any_locked: true,
                all_locked: true,
                ..CommandContext::default()
            },
            CommandContext {
                board_open: true,
                board_saved: true,
                dirty: true,
                can_undo: true,
                can_redo: true,
                clipboard_has_content: true,
                selected: 3,
                any_group: true,
                spaces: 6,
                transparency_blocked: true,
                ..CommandContext::default()
            },
        ];
        for ctx in contexts {
            for command in Command::ALL {
                if let Some(why) = command.availability(&ctx).reason() {
                    assert!(!why.is_empty(), "{command:?} is disabled with no reason");
                }
            }
            for sub in Submenu::ALL {
                if let Some(why) = sub.availability(&ctx).reason() {
                    assert!(!why.is_empty(), "{sub:?} is disabled with no reason");
                }
            }
        }
    }

    #[test]
    fn selection_size_gates_group_align_and_distribute() {
        let base = CommandContext { board_open: true, ..CommandContext::default() };

        let one = CommandContext { selected: 1, ..base };
        assert_eq!(Command::Group.availability(&one).reason(), Some(reason::NEEDS_TWO));
        assert_eq!(Command::AlignLeft.availability(&one).reason(), Some(reason::NEEDS_TWO));
        assert!(Command::Copy.is_enabled(&one));

        let two = CommandContext { selected: 2, ..base };
        assert!(Command::Group.is_enabled(&two));
        assert!(Command::AlignLeft.is_enabled(&two));
        assert_eq!(
            Command::DistributeHorizontally.availability(&two).reason(),
            Some(reason::NEEDS_THREE)
        );

        let three = CommandContext { selected: 3, ..base };
        assert!(Command::DistributeHorizontally.is_enabled(&three));
    }

    /// A locked item that can still be deleted or restacked is not locked.
    #[test]
    fn locking_withdraws_the_commands_that_would_modify_the_selection() {
        let locked = CommandContext {
            board_open: true,
            selected: 2,
            any_locked: true,
            all_locked: true,
            ..CommandContext::default()
        };
        for command in [
            Command::Delete,
            Command::BringToFront,
            Command::SendToBack,
            Command::AlignLeft,
            Command::Lock,
        ] {
            assert_eq!(
                command.availability(&locked).reason(),
                Some(reason::LOCKED),
                "{command:?} survived the lock"
            );
        }
        assert!(Command::Unlock.is_enabled(&locked));
        assert!(Command::Copy.is_enabled(&locked), "copying a locked item is harmless");
    }

    #[test]
    fn save_is_offered_only_when_there_is_something_to_save() {
        let clean = CommandContext { board_open: true, ..CommandContext::default() };
        assert_eq!(
            Command::Save.availability(&clean).reason(),
            Some(reason::NOTHING_TO_SAVE)
        );
        assert!(Command::Save.is_enabled(&CommandContext { dirty: true, ..clean }));
    }

    /// A board with no file behind it has no library row, so the entries that act on
    /// one have to say so rather than acting on nothing.
    #[test]
    fn an_unsaved_board_cannot_be_starred_filed_duplicated_or_deleted() {
        let unsaved = CommandContext {
            board_open: true,
            spaces: 6,
            ..CommandContext::default()
        };
        for command in [Command::StarBoard, Command::DuplicateBoard, Command::DeleteBoard] {
            assert_eq!(
                command.availability(&unsaved).reason(),
                Some(reason::UNSAVED_BOARD),
                "{command:?}"
            );
        }
        assert_eq!(
            Submenu::MoveToSpace.availability(&unsaved).reason(),
            Some(reason::UNSAVED_BOARD)
        );

        let saved = CommandContext { board_saved: true, ..unsaved };
        assert!(Submenu::MoveToSpace.availability(&saved).is_enabled());
        assert_eq!(
            Submenu::MoveToSpace
                .availability(&CommandContext { spaces: 0, ..saved })
                .reason(),
            Some(reason::NO_SPACES)
        );
    }

    /// `docs/05-design-language.md` §3a asks for an in-app translucency override *as
    /// well as* honouring the OS. When the OS has already refused, the switch says so
    /// instead of pretending to work.
    #[test]
    fn the_translucency_switch_defers_to_the_operating_system() {
        let free = CommandContext::default();
        assert!(Command::ToggleTranslucency.is_enabled(&free));
        let blocked = CommandContext { transparency_blocked: true, ..free };
        assert_eq!(
            Command::ToggleTranslucency.availability(&blocked).reason(),
            Some(reason::SYSTEM_OPAQUE)
        );
    }

    /// The chrome renders the platform's own spelling; a Mac build showing "Ctrl+Z"
    /// is the kind of thing that reads as a port rather than an app.
    #[test]
    fn shortcuts_render_per_platform() {
        let undo = Command::Undo.shortcut().unwrap();
        assert_eq!(format_shortcut(undo, true), "Cmd+Z");
        assert_eq!(format_shortcut(undo, false), "Ctrl+Z");
    }

    /// The keys that are punctuation are shown as punctuation. egui's `Key::name` — what
    /// `KeyboardShortcut::format` reaches for — gives the variant's own spelling, so the
    /// shortcut sheet advertised `Cmd+CloseBracket` and `Cmd+Plus`: names of enum arms,
    /// printed as though they were keys to press.
    #[test]
    fn a_punctuation_key_is_shown_as_punctuation_not_as_its_variant_name() {
        let sheet: Vec<String> = Command::ALL
            .iter()
            .filter_map(|c| c.shortcut())
            .map(|s| format_shortcut(s, true))
            .collect();

        for spelt in ["CloseBracket", "OpenBracket", "Plus", "Minus"] {
            assert!(
                !sheet.iter().any(|s| s.contains(spelt)),
                "`{spelt}` is an enum variant, not a key: {sheet:?}",
            );
        }
        assert_eq!(format_shortcut(Command::BringToFront.shortcut().unwrap(), true), "Shift+Cmd+]");
        assert_eq!(format_shortcut(Command::SendToBack.shortcut().unwrap(), true), "Shift+Cmd+[");
        assert_eq!(format_shortcut(Command::ZoomIn.shortcut().unwrap(), true), "Cmd++");
    }

    #[test]
    fn only_the_stateful_commands_are_ticked() {
        let toggles: Vec<_> = Command::ALL.iter().copied().filter(|c| c.is_toggle()).collect();
        assert_eq!(
            toggles,
            vec![
                Command::StarBoard,
                Command::ToggleMinimap,
                Command::TogglePropertiesPanel,
                Command::PresentationMode,
                Command::ToggleTranslucency,
                Command::ToggleLinkPreviews,
                Command::ToggleAlignObjects,
                Command::SnapToGrid,
                Command::ToggleAgentRaw,
                Command::ToggleBrowserNodes,
                Command::ToggleWorktrees,
            ]
        );
    }

    /// The two preferences that are off by default say **why** on the row.
    ///
    /// `docs/07-agent-canvas.md` §0's third rule is that everything expensive degrades to a
    /// legible explanation rather than a dead button — and a switch whose cost is invisible
    /// from the switch is the version of that failure nobody notices, because the control
    /// works perfectly and the consequence arrives an hour later. The assertion is about
    /// the *cost being named*, not about the wording.
    #[test]
    fn the_expensive_preferences_explain_themselves_on_the_row() {
        let browser = Command::ToggleBrowserNodes.note().expect("browser nodes carry a note");
        assert!(browser.contains("Off by default"), "{browser}");
        assert!(browser.contains("MB"), "the memory cost is the whole reason it is off");

        let worktrees = Command::ToggleWorktrees.note().expect("worktrees carry a note");
        assert!(worktrees.contains("Off by default"), "{worktrees}");
        assert!(worktrees.contains("worktree"), "{worktrees}");

        // Not on every row. A tooltip on a row whose label already says everything trains
        // people to stop reading tooltips, including the two above.
        //
        // **`Command::RunAgent` was in this list and has deliberately left it**, which is a
        // reversal rather than a relaxation. The rule the list encodes is *"a label that
        // already says everything needs no tooltip"*, and it was applied to the agent verbs
        // on the reasoning that *Run* is self-evident. The user disagreed, of the whole layer
        // rather than of one row: *"for every future that is for the terminal / ai mode i
        // want a tool tip"*. They are right about the category — *Run* on a sticky would be
        // meaningless, and on an agent node it starts a process on a subscription, streams
        // third-party text onto the board and can be scheduled to happen again. That is not a
        // label that says everything. The rule stands for the rest of the application, and
        // `every_agent_menu_entry_carries_a_tooltip` is the exception stated positively.
        for quiet in [Command::Copy, Command::ZoomIn, Command::Save, Command::Delete] {
            assert!(quiet.note().is_none(), "{quiet:?} does not need explaining");
        }
    }

    /// None of the agent commands takes a key.
    ///
    /// Deliberate, and worth pinning. `⌘↵` was the obvious binding for *Run*, and it is
    /// exactly the chord a user presses while a caret is open in a sticky — so it would
    /// have started an agent from inside an edit, which is the family of bug trap 9 and
    /// feedback 27 are both about. A menu row that cannot be pressed by accident is worth
    /// more here than a shortcut nobody asked for.
    #[test]
    fn no_agent_command_claims_a_key() {
        for command in [
            Command::RunAgent,
            Command::StopAgent,
            Command::ToggleAgentRaw,
            Command::EditAgentRules,
            Command::EditAgentSchedule,
        ] {
            assert_eq!(command.shortcut(), None, "{command:?}");
            assert_eq!(command.alternate_shortcut(), None, "{command:?}");
        }
    }

    /// Run and Stop are gated on the half of the selection each can act on, and a mixed
    /// selection gets both. A toggle would have to pick one and be wrong about the rest.
    #[test]
    fn run_and_stop_are_each_offered_only_where_they_can_act() {
        let idle = CommandContext {
            board_open: true,
            selected: 1,
            agents_selected: 1,
            ..CommandContext::default()
        };
        assert!(Command::RunAgent.is_enabled(&idle));
        assert!(!Command::StopAgent.is_enabled(&idle));

        let running = CommandContext {
            any_agent_running: true,
            all_agents_running: true,
            ..idle
        };
        assert!(!Command::RunAgent.is_enabled(&running));
        assert!(Command::StopAgent.is_enabled(&running));

        let mixed = CommandContext {
            selected: 2,
            agents_selected: 2,
            any_agent_running: true,
            all_agents_running: false,
            ..idle
        };
        assert!(Command::RunAgent.is_enabled(&mixed), "one of them is idle");
        assert!(Command::StopAgent.is_enabled(&mixed), "one of them is running");

        // And nothing agent-shaped is offered without an agent — with the reason named,
        // rather than a row that can be clicked and does nothing.
        let none = CommandContext { board_open: true, selected: 3, ..CommandContext::default() };
        for command in [Command::RunAgent, Command::StopAgent, Command::ToggleAgentRaw] {
            assert_eq!(
                command.availability(&none).reason(),
                Some(reason::NO_AGENT),
                "{command:?}"
            );
        }
    }

    /// A rule cascade and a schedule belong to **one** node, so their editors are offered
    /// for exactly one selected agent. Two agents have two cascades and two schedules;
    /// there is no shared one to edit, and offering it would write one over both.
    #[test]
    fn the_agent_editors_need_exactly_one_agent() {
        let base = CommandContext { board_open: true, selected: 1, ..CommandContext::default() };
        for command in [Command::EditAgentRules, Command::EditAgentSchedule] {
            assert!(!command.is_enabled(&CommandContext { agents_selected: 0, ..base }));
            assert!(command.is_enabled(&CommandContext { agents_selected: 1, ..base }));
            assert_eq!(
                command
                    .availability(&CommandContext { agents_selected: 2, selected: 2, ..base })
                    .reason(),
                Some(reason::ONE_AGENT),
                "{command:?}"
            );
        }
    }

    /// The agent submenu hangs under Edit rather than becoming a fifth top-level menu, and
    /// every one of its rows is an ordinary command — which is what makes the right-click
    /// menu's reachability rule satisfiable.
    #[test]
    fn the_agent_verbs_live_under_edit_and_are_all_real_commands() {
        assert_eq!(Submenu::Agent.parent(), Menu::Edit);
        assert!(Menu::Edit.entries().contains(&Entry::Sub(Submenu::Agent)));
        assert!(
            Submenu::Agent.entries().iter().any(|e| matches!(e, Entry::Item(_))),
            "a submenu drawn from the table must have rows in the table"
        );
        for entry in Submenu::Agent.entries() {
            if let Entry::Item(command) = entry {
                assert_eq!(command.menu(), Menu::Edit, "{command:?}");
            }
        }
        // Disabled as a whole rather than opening onto four greyed rows.
        let empty = CommandContext { board_open: true, ..CommandContext::default() };
        assert_eq!(Submenu::Agent.availability(&empty).reason(), Some(reason::NO_AGENT));
    }

    /// *"for every future that is for the terminal / ai mode i want a tool tip."*
    ///
    /// The contract, so the next agent command cannot ship mute. It is deliberately spelled
    /// as *"everything in the Agent band"* rather than as a list of names — a list would
    /// have to be edited to add a command, which is exactly the edit somebody skips.
    ///
    /// The Preferences half is picked out by the heading, which is the only thing that says
    /// where that band begins and ends; a run of rows between separators has no boundary a
    /// test can see, which is half of why the heading is worth having at all.
    #[test]
    fn every_agent_menu_entry_carries_a_tooltip() {
        let mut checked = 0;
        for entry in Submenu::Agent.entries() {
            if let Entry::Item(command) = entry {
                assert!(
                    command.note().is_some_and(|note| note.len() > 40),
                    "{command:?} is in Edit ▸ Agent with no tooltip, or with one too short \
                     to explain anything"
                );
                checked += 1;
            }
        }
        assert!(checked >= 6, "only {checked} agent commands were checked — is the list right?");

        // Preferences ▸ Agents: every row from the heading to the next separator.
        let mut in_band = false;
        let mut band = 0;
        for entry in Menu::Preferences.entries() {
            match entry {
                Entry::Heading(title) if *title == AGENT_SECTION => in_band = true,
                Entry::Separator if in_band => break,
                Entry::Item(command) if in_band => {
                    assert!(command.note().is_some(), "{command:?} is in the Agents band mute");
                    band += 1;
                }
                Entry::Sub(sub) if in_band => {
                    assert!(sub.note().is_some(), "{sub:?} is in the Agents band mute");
                    band += 1;
                }
                _ => {}
            }
        }
        assert!(in_band, "Preferences has no Agents heading — the band cannot be found");
        assert!(band >= 4, "only {band} rows were found under the Agents heading");
    }

    /// A heading introduces rows; one with nothing under it is a label about nothing, and one
    /// immediately after a separator that is itself followed by a separator is two rules and
    /// a title where a band should be. Checked structurally rather than by eye because
    /// headings are cheap to add and easy to leave stranded.
    #[test]
    fn a_heading_always_has_rows_under_it() {
        for entries in Menu::ALL
            .into_iter()
            .map(Menu::entries)
            .chain(Submenu::ALL.into_iter().map(Submenu::entries))
        {
            for (i, entry) in entries.iter().enumerate() {
                let Entry::Heading(title) = entry else { continue };
                let next = entries.get(i + 1);
                assert!(
                    matches!(next, Some(Entry::Item(_) | Entry::Sub(_))),
                    "the {title:?} heading is followed by {next:?} rather than by a row"
                );
                assert!(
                    !title.trim().is_empty(),
                    "a heading with no words is a gap that looks like a bug"
                );
            }
        }
    }

    /// The palette shows where a command lives, and the trail is derived so it cannot
    /// name a menu the command has since left.
    #[test]
    fn a_commands_trail_names_the_submenu_it_sits_in() {
        assert_eq!(Command::ExportCsv.path(), (Menu::Board, Some("Export")));
        assert_eq!(Command::Group.path(), (Menu::Edit, Some("Arrange")));
        assert_eq!(Command::ZoomIn.path(), (Menu::View, None));
    }
}
