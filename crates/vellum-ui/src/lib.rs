//! Vellum's application chrome: menu bar, tool palette, properties panel, board
//! library, colour picker and dialogs.
//!
//! # Data in, events out
//!
//! The whole crate is one call per frame. The app hands over a borrowed snapshot of
//! its state and gets back a list of things the user asked for:
//!
//! ```
//! use vellum_ui::{Chrome, ChromeState, Command, Screen, UiEvent};
//!
//! let ctx = egui::Context::default();
//! let mut chrome = Chrome::new();
//!
//! // Each frame:
//! let state = ChromeState { screen: Screen::Library, ..ChromeState::default() };
//! let mut output = None;
//! ctx.run_ui(egui::RawInput::default(), |ui| {
//!     output = Some(chrome.show(ui, &state));
//! });
//!
//! for event in &output.expect("one pass").events {
//!     match event {
//!         UiEvent::Command(Command::NewBoard) => { /* create a board */ }
//!         UiEvent::ToolChanged(tool) => { /* switch tool */ }
//!         _ => {}
//!     }
//! }
//! // Draw the canvas into `output.canvas_rect` — but ignore pointer input on any
//! // frame where `output.pointer_over_ui` is set.
//! ```
//!
//! Nothing in here opens a file, mutates a document, moves a camera or touches the
//! clipboard. That is not a stylistic preference: it is what lets every panel be
//! driven from a test with synthetic input and no window, which is how most of this
//! crate's tests work.
//!
//! # Why egui
//!
//! `docs/01-architecture.md` rules out a webview and puts the renderer on `wgpu`
//! directly. That leaves the chrome needing a toolkit that renders through the same
//! device, does not own the event loop, and handles IME. egui is immediate-mode and
//! renderer-agnostic — its entire output is a vertex buffer plus a font-atlas delta —
//! so `vellum-ui` depends on `egui` alone, with no GPU or windowing crate anywhere in
//! its tree. The workspace manifest records why the wgpu integration is drawn by
//! `vellum-render` rather than pulled in as a dependency.
//!
//! # Layout
//!
//! # The look
//!
//! `docs/05-design-language.md` supplies the supplied colourway and one hard
//! constraint — *"make it beautiful and don't make it look like AI design"*. [`theme`]
//! is where that stops being a preference and becomes mechanical: it holds the only
//! colour literals in the crate, and `tests/tokens.rs` scans the source and fails if
//! a widget names a colour or a radius for itself. The short version of what
//! that buys: flat surfaces separated by 1px hairlines rather than shadows, 4–6px
//! corners, a 4px spacing grid, hierarchy carried by type, translucent chrome only
//! where something genuinely floats over the canvas, and **every number in the
//! interface set in monospace** so digits do not jitter while they are being dragged.
//!
//! # Layout
//!
//! - [`chrome`] — [`Chrome`], the one entry point, and the state it holds.
//! - [`command`] — the command table the menu bar, the keymap and the palette share.
//! - [`command_palette`], [`find`] — the two `Cmd+K` / `Cmd+F` overlays.
//! - [`context_bar`], [`context_menu`] — the toolbar that floats above the selection
//!   and the menu the right button opens. Between them they are what a board is
//!   driven from; the docked [`properties`] panel is the long form, off by default.
//! - [`event`] — [`UiEvent`], everything the chrome can ask for.
//! - [`selection`] — selection to panel state, including the mixed-value rule.
//! - [`theme`] — the palette, the material, and the egui style derived from them.
//! - [`icon`] — the icon set, drawn from geometry rather than shipped as assets.
//! - [`mark`] — Velm's mark, drawn the same way for the same reason.
//! - [`tabs`] — the strip along the top, and the sticky home tab.
//! - [`menu`], [`toolbar`], [`properties`], [`library`], [`status`] — the panels.
//! - [`color`], [`dialog`], [`widgets`] — the pieces they are built from.
//!
//! # What the app has to supply that it did not before
//!
//! The public interface moved once, to close the gaps the first pass left open. The
//! whole delta, so the wiring is a checklist rather than a search:
//!
//! - **[`BoardState`] gained `path` and `starred`.** A board with no file behind it
//!   has no row in the library, so *Star*, *Move to*, *Duplicate board* and *Delete
//!   board* withdraw themselves with a reason instead of acting on nothing.
//! - **[`BoardCard`] gained `starred`**, and the library emits
//!   [`LibraryEvent::SetStarred`] — plus [`LibraryEvent::MoveToSpace`],
//!   `CreateSpace`, `RenameSpace`, `DeleteSpace` and `SetSpacePinned`, which are the
//!   five verbs that make Spaces a feature rather than a list.
//! - **[`ChromeState`] gained `find_matches`**, the `(current, total)` the `Cmd+F`
//!   bar shows. The chrome owns the field; the app owns the index.
//! - **[`Chrome::set_custom_shapes`]** supplies the user's own SVG shapes to the
//!   picker, and [`UiEvent::UploadShape`] asks for another. The chrome never reads a
//!   file.
//! - **[`UiEvent`] gained `Find`, `ThemePreferenceChanged`, `ShapeColorsChanged`,
//!   `CustomShapeChosen` and `UploadShape`.** The two preference events are applied by
//!   the chrome to itself as well as reported, so a partially wired app still works.
//! - **[`Command::availability`] replaces the bare boolean.** A command that cannot
//!   act carries the reason it cannot, and every menu row, palette row and context
//!   entry shows it. `is_enabled` is still there and still means what it did.
//! - **[`Menu`] is now Board · Edit · View · Preferences**, with [`Submenu`]s under
//!   it; [`Menu::entries`] yields [`Entry`] rather than `Option<Command>`.
//! - **`Tool::Comment` is gone.** Collaboration is cut, and
//!   `docs/04-ui-reference.md` §1 asks for no dead buttons.
//! - **[`Chrome::set_tabs`] and the [`tabs`] module.** The strip is drawn on every
//!   screen and takes its own band off the top of the window, so
//!   [`ChromeOutput::canvas_rect`] is shorter than it was by
//!   [`theme::TAB_STRIP_HEIGHT`]. The app supplies which boards are open and reads
//!   [`Chrome::active_tab`] back; the strip owns the order and the selection. Four new
//!   [`UiEvent`]s go with it: `SelectTab`, `CloseTab`, `ReorderTabs`, `NewBoardTab`.

pub mod agent_dialogs;
pub mod agent_panel;
pub mod chrome;
pub mod color;
pub mod command;
pub mod command_palette;
pub mod context_bar;
pub mod context_menu;
pub mod dialog;
pub mod event;
pub mod find;
pub mod icon;
pub mod library;
pub mod mark;
pub mod menu;
pub mod properties;
pub mod selection;
pub mod status;
pub mod tabs;
pub mod theme;
pub mod tool;
pub mod toolbar;
pub mod widgets;

pub use agent_dialogs::{RulesForm, compose as compose_rules, schedule_problem};
pub use agent_panel::{AgentPanelState, AgentRow};
pub use chrome::{BoardState, Chrome, ChromeOutput, ChromeState, Screen, ViewState};
pub use color::{ColorPicker, Hsv, SWATCHES, from_egui, parse_hex, to_egui};
pub use command::{Availability, Command, CommandContext, Entry, Menu, Submenu, format_shortcut};
pub use command_palette::CommandPalette;
pub use context_bar::{ContextBarState, Control};
pub use context_menu::{ContextMenu, ContextTarget, Row as ContextRow};
pub use dialog::{Dialog, DialogStack, ReferenceSection, Step, Toast, ToastKind};
pub use event::{
    AgentEdit, DialogEvent, DialogId, EventSink, FindEvent, GridSettings, LibraryEvent, SecretKey,
    StyleEdit, TransformEdit, UiEvent,
};
pub use find::FindBar;
pub use icon::Icon;
pub use library::{
    BoardCard, IMPORT_STEPS, LayoutMode as LibraryLayout, Scope as LibraryScope, Space, Thumbnail,
};
pub use menu::{MenuFlags, MenuHeader, ProviderStatus};
pub use properties::ColorTarget;
pub use selection::{
    AgentLink, AgentSummary, Border, Bounds, BrowserSummary, ConnectorSummary, Field,
    FileTreeSummary, FontWeight, ItemFacet, LinkSummary, NoteSummary, PanelModel, SelectionItem,
    TextSummary, VerticalAlign, WorktreeState,
};
pub use status::{ZOOM_STEPS, zoom_label};
pub use tabs::{BoardTab, TabKey, TabStrip};
pub use theme::{
    Accent, Backing, Glass, GlassSurface, Palette, SystemAppearance, Theme, ThemePreference,
};
pub use tool::{
    CustomShape, CustomShapeId, EraserMode, Flyout, PenKind, PenPreset, SHAPE_SHORTCUTS,
    ShapeColors, ShapeGroup, Tool, shape_label, shape_shortcut,
};

// Re-exported so the app can spell the types the chrome hands it without having to
// name a dependency it may not otherwise carry.
pub use egui;
// The Agent Canvas configuration types. `AgentSummary`, `Dialog::Schedule` and
// `AgentEdit` all carry these, and a public field whose type the caller cannot name is a
// field they cannot build — the same reason `CardMode` is re-exported below.
pub use vellum_agent;
pub use vellum_agent::{
    AgentRules, Completion, DisplayMode, Layer, NoteScope, Permissions, Provider, ProviderChoice,
    Recurrence, ResolvedRules, RoleKind, RuleFile, Schedule, Territory, Transport, Trigger,
};
pub use vellum_connect::{AnchorSide, Arrowhead, LineStyle, RoutingMode};
// `CardMode` is here because `LinkSummary` carries one: a public struct whose field type
// the caller cannot name is one they cannot build.
pub use vellum_doc::{Align, CardMode, Color, ItemId, Placement};
pub use vellum_shapes::{CATALOGUE as SHAPE_CATALOGUE, Shape};
