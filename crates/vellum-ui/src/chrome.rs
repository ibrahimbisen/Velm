//! The one entry point: everything the app draws, in one call per frame.
//!
//! The interface is data in, events out. [`ChromeState`] is a borrowed snapshot the
//! app assembles from its own model; [`ChromeOutput`] carries the events, the
//! rectangle the canvas may draw into, and whether the pointer and keyboard belong
//! to the chrome this frame. Nothing here reads a document, a file or a camera.

use crate::command::{Command, CommandContext};
use crate::command_palette::CommandPalette;
use crate::context_bar::ContextBarState;
use crate::context_menu::{ContextMenu, ContextTarget};
use crate::dialog::{Dialog, DialogStack, Toast};
use crate::event::{EventSink, UiEvent};
use crate::find::FindBar;
use crate::library::{BoardCard, LibraryState, Space};
use crate::menu::MenuFlags;
use crate::properties::PropertiesState;
use crate::selection::{ItemFacet, PanelModel, SelectionItem};
use crate::tabs::{BoardTab, TabStrip};
use crate::theme::{GlassSurface, Palette, SystemAppearance, Theme, ThemePreference};
use crate::tool::{CustomShape, PenPreset, Tool};
use crate::toolbar::ToolbarState;
use egui::{Context, Key, Modifiers, Rect, Ui};
use std::path::Path;
use std::time::SystemTime;
use vellum_shapes::Shape;

/// Which top-level screen the window is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The board library, shown when no board is open.
    #[default]
    Library,
    /// A board, with the full chrome around it.
    Board,
}

/// What the chrome needs to know about the open board.
#[derive(Debug, Clone, Copy, Default)]
pub struct BoardState<'a> {
    pub title: &'a str,
    /// The file behind it, if it has one yet. A board that has never been saved has no
    /// row in the library, so *Star*, *Move to*, *Duplicate* and *Delete* have nothing
    /// to act on and say so rather than acting on nothing.
    pub path: Option<&'a Path>,
    /// Whether it is starred, for the tick in the Board menu and the mark in the bar.
    pub starred: bool,
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    /// Whether Paste has anything to paste — including Miro's clipboard format.
    pub clipboard_has_content: bool,
}

/// The camera and view toggles, as the status cluster and View menu show them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewState {
    /// Scale factor, where `1.0` is 100%.
    pub zoom: f32,
    pub minimap_visible: bool,
    pub presenting: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { zoom: 1.0, minimap_visible: false, presenting: false }
    }
}

/// Everything the chrome reads this frame.
#[derive(Debug, Clone, Copy)]
pub struct ChromeState<'a> {
    pub screen: Screen,
    pub board: BoardState<'a>,
    pub tool: Tool,
    pub selection: &'a [SelectionItem],
    pub view: ViewState,
    pub library: &'a [BoardCard],
    /// Families offered by the font dropdown, from the app's font system.
    pub font_families: &'a [String],
    /// What the find bar shows: `(current, total)`, one-based. `None` while the app
    /// has not searched — the readout stays blank rather than claiming zero matches.
    pub find_matches: Option<(usize, usize)>,
    /// The open board's canvas colour and pattern, for Board ▸ Background's ticks.
    pub background: vellum_doc::Background,
    /// The wall clock, for the library's relative timestamps. Passed in rather than
    /// read here so a test can pin it.
    pub now: SystemTime,
    /// Whether link cards may fetch their own titles and preview images, for the tick beside
    /// Preferences ▸ Fetch link previews. The app owns the setting and persists it.
    pub link_previews: bool,
    /// Whether relative snapping is on, for the tick beside Preferences ▸ Align objects.
    /// The app owns the setting and persists it, exactly as it does the one above.
    pub align_objects: bool,
    /// Whether a move lands on the board's grid, for the tick beside View ▸ Grid ▸ Snap to
    /// grid. The strict snapping, off by default; `align_objects` is the loose one.
    pub snap_to_grid: bool,
    /// The grid every board wears — pattern, colour and opacity — as one application
    /// setting. The app owns it and persists it in the library sidecar, exactly as it does
    /// `align_objects` and the accent; see `crate::event::UiEvent::GridChanged`.
    pub grid: crate::event::GridSettings,
    /// The selection's bounding box **in screen space**, which is where
    /// [`crate::context_bar`] floats its toolbar.
    ///
    /// Supplied rather than derived: `PanelModel::bounds` is in world units and only
    /// the app holds the camera that converts them. `None` — nothing selected, or a
    /// selection scrolled off screen — draws no bar, which is correct either way.
    pub selection_rect: Option<Rect>,
    /// Whether a text session on the **canvas** owns the keyboard — the on-canvas caret.
    ///
    /// It is not an egui widget, so `Context::egui_wants_keyboard_input` — the only thing this
    /// crate could otherwise ask — is false throughout it. The app is the only party that
    /// knows, which is why this is supplied rather than derived.
    ///
    /// **What it cost while it did not exist.** Every keystroke reaches egui through
    /// `Shell::on_window_event` *before* the app's own sessions claim it, so the shortcut
    /// table below ran on the letters of every word typed on a board: renaming a frame and
    /// pressing Backspace fired [`Command::Delete`] and **deleted the frame**, a word
    /// containing `r` or `o` armed the shape tool so the click that left the field placed a
    /// rectangle, and `v`/`n`/`t`/`f` changed the tool under the hands typing them. The
    /// user's own three reports, one root cause. See [`Chrome::shortcuts`] for what
    /// suppression means exactly — it is not all of them.
    pub text_session: bool,
}

impl Default for ChromeState<'_> {
    fn default() -> Self {
        Self {
            screen: Screen::Library,
            board: BoardState::default(),
            tool: Tool::Select,
            selection: &[],
            view: ViewState::default(),
            library: &[],
            font_families: &[],
            find_matches: None,
            background: vellum_doc::Background::default(),
            now: SystemTime::UNIX_EPOCH,
            link_previews: true,
            align_objects: true,
            snap_to_grid: false,
            grid: crate::event::GridSettings::default(),
            selection_rect: None,
            text_session: false,
        }
    }
}

impl ChromeState<'_> {
    /// The enablement facts the command table needs, derived rather than supplied so
    /// the app cannot get them subtly out of step with the selection it passed.
    pub fn command_context(&self) -> CommandContext {
        let locked = self.selection.iter().filter(|i| i.locked).count();
        CommandContext {
            board_open: self.screen == Screen::Board,
            board_saved: self.board.path.is_some(),
            board_starred: self.board.starred,
            dirty: self.board.dirty,
            can_undo: self.board.can_undo,
            can_redo: self.board.can_redo,
            clipboard_has_content: self.board.clipboard_has_content,
            selected: self.selection.len(),
            any_locked: locked > 0,
            all_locked: locked > 0 && locked == self.selection.len(),
            any_group: self.selection.iter().any(|i| i.facet == ItemFacet::Group),
            // Filled in by `Chrome::show`, which is the only place that knows them.
            spaces: 0,
            transparency_blocked: false,
        }
    }
}

/// What the app gets back.
#[derive(Debug, Clone, PartialEq)]
pub struct ChromeOutput {
    /// In the order the user caused them.
    pub events: Vec<UiEvent>,
    /// Screen-space rectangle the canvas may draw and hit-test in, after the docked
    /// panels have taken their share. The floating tool palette and status cluster
    /// deliberately do *not* shrink it — the board runs underneath them.
    pub canvas_rect: Rect,
    /// True when the pointer is over chrome. The app must not pan, zoom or hit-test
    /// the board on a frame where this is set, or a drag on a slider also drags the
    /// canvas behind it.
    pub pointer_over_ui: bool,
    /// True when a text field has focus. The app must not act on bare-key shortcuts
    /// while this is set.
    pub keyboard_captured: bool,
    /// The enablement facts as the chrome resolved them, and the toggles as it ticked
    /// them.
    ///
    /// Reported rather than left for the app to recompute, because a **native** menu bar
    /// has to grey and tick exactly the same rows as the in-app one — and two
    /// derivations of "can this act right now" is precisely how the two menus come to
    /// disagree. `vellum_app::menubar` reads these. Two of the fields in `command_context`
    /// are the chrome's own (how many spaces the library holds, whether the OS has
    /// already refused translucency), so the app could not have produced them anyway.
    pub command_context: CommandContext,
    pub menu_flags: MenuFlags,
}

/// The chrome, and everything it remembers between frames.
///
/// One long-lived value owned by the app. All the per-frame inputs arrive through
/// [`Chrome::show`]; the fields here are only the state that *belongs* to the UI —
/// which flyout is open, what the pickers last chose, what is queued to be asked.
#[derive(Debug, Default)]
pub struct Chrome {
    theme: Theme,
    /// What the user asked for, which may be "whatever the system is doing".
    /// [`Chrome::theme`] is the answer; this is the question.
    preference: ThemePreference,
    /// What the OS last told us. Re-supplied by the app every frame it changes, so
    /// Reduce Transparency takes effect live rather than at the next launch.
    system: SystemAppearance,
    /// `None` until the first frame, so the palette is installed once rather than
    /// every frame: applying it clones and replaces the whole egui style.
    applied: Option<(Theme, SystemAppearance, crate::theme::Accent)>,
    /// Where the glass surfaces were last frame, for the renderer to blur behind.
    glass: Vec<GlassSurface>,
    /// The in-app translucency override `docs/05-design-language.md` §3a asks for, in
    /// addition to honouring the OS. Held as *off* rather than *on* so the default —
    /// glass, as specified — needs no constructor.
    translucency_off: bool,
    /// How much tint a glass panel lays over the blurred canvas, when the user has
    /// asked for something other than the palette's own figure.
    ///
    /// `None` means "not chosen", which is the palette default — held that way so the
    /// preference does not silently pin an opacity the palette later moves. A `Some`
    /// only ever *replaces* [`Palette::glass_opacity`]; it cannot override the OS, which
    /// is resolved after it in [`Chrome::palette`].
    glass_opacity: Option<u8>,
    /// Which colour the primary accent wears — Preferences ▸ Accent colour.
    ///
    /// A plain value rather than an `Option`, unlike `glass_opacity` above: there *is* no
    /// "not chosen" here, because [`crate::theme::Accent`]'s own default is the palette's
    /// accent. Holding it as `Some`/`None` would let the two disagree about what the
    /// default is, in a type whose whole job is to be one of three known colours.
    accent: crate::theme::Accent,
    toolbar: ToolbarState,
    /// The toolbar that floats above the selection — what a board is actually driven
    /// from now. See [`crate::context_bar`].
    context_bar: ContextBarState,
    /// The right button's menu, and the one behind the bar's `⋮`.
    context_menu: ContextMenu,
    /// Whether the board's name in the top-left pill is being edited in place.
    rename: crate::menu::Rename,
    properties: PropertiesState,
    /// Whether the docked properties panel is showing.
    ///
    /// **Off by default**, which is the change the floating bar bought: a canvas app
    /// should not spend a fixed column of board space on controls that are relevant
    /// some of the time. Nothing was removed — `Command::TogglePropertiesPanel`,
    /// `⌥⌘P`, View ▸ Properties panel and a row in the context menu all reach it, and
    /// it still holds every control it did.
    properties_open: bool,
    library: LibraryState,
    /// The boards that are open, and which of them is in front. The strip owns the
    /// order and the selection; the app owns the membership — see [`crate::tabs`].
    tabs: TabStrip,
    /// The user's own SVG shapes, supplied by the app. The chrome never reads a file.
    custom_shapes: Vec<CustomShape>,
    command_palette: CommandPalette,
    find: FindBar,
    dialogs: DialogStack,
}

impl Chrome {
    pub fn new() -> Self {
        Self::default()
    }

    /// The palette actually in force — the preference resolved against the system.
    pub const fn theme(&self) -> Theme {
        self.theme
    }

    /// Records a palette preference. Takes effect on the next [`Chrome::show`].
    ///
    /// **The app is light only** — *"i want only light mode"* — so the palette that
    /// comes back is always the light one whatever is passed here. The preference is
    /// still remembered, and it is still what the app persists, so re-offering the
    /// choice later is a change to `ThemePreference::resolve` and one menu.
    pub const fn set_theme(&mut self, theme: Theme) {
        self.preference = match theme {
            Theme::Light => ThemePreference::Light,
            Theme::Dark => ThemePreference::Dark,
        };
        self.theme = self.preference.resolve(self.system);
    }

    /// What the user chose: light, dark, or follow the system.
    pub const fn theme_preference(&self) -> ThemePreference {
        self.preference
    }

    /// Sets the preference. `System` resolves against whatever
    /// [`Chrome::set_system_appearance`] last reported.
    pub const fn set_theme_preference(&mut self, preference: ThemePreference) {
        self.preference = preference;
        self.theme = preference.resolve(self.system);
    }

    /// Hands over what the operating system is doing right now.
    ///
    /// Called every frame the reading changes — `docs/05-design-language.md` §3a
    /// requires Reduce Transparency to be *detected at runtime and reacted to live*.
    /// A dark-mode flip also moves the palette if the preference is
    /// [`ThemePreference::System`].
    pub const fn set_system_appearance(&mut self, system: SystemAppearance) {
        self.system = system;
        self.theme = self.preference.resolve(system);
    }

    pub const fn system_appearance(&self) -> SystemAppearance {
        self.system
    }

    /// The palette in force, with the OS's accessibility settings and the in-app
    /// override already applied.
    pub fn palette(&self) -> Palette {
        let palette = Palette::resolve(self.theme, self.system);
        // Only while the material is actually in force. `resolve` has already answered
        // 255 if the OS asked for reduced transparency, and the user's slider must not
        // walk an accessibility setting back.
        let palette = match self.glass_opacity {
            Some(opacity) if palette.translucent => palette.with_glass_opacity(opacity),
            _ => palette,
        };
        let palette = if self.translucency_off { palette.opaque() } else { palette };
        // Last, and unconditionally: unlike the material, an accent is not something the
        // operating system has an opinion about, so nothing above can veto it.
        palette.with_accent(self.accent)
    }

    /// Which colour the primary accent wears.
    pub const fn accent(&self) -> crate::theme::Accent {
        self.accent
    }

    /// Sets it. The app calls this at startup with what it read from the sidecar, and the
    /// chrome sets it on itself when the user picks a row in Preferences.
    pub const fn set_accent(&mut self, accent: crate::theme::Accent) {
        self.accent = accent;
    }

    /// Whether floating chrome is translucent right now — the user's switch *and* the
    /// operating system's, resolved.
    pub fn translucency(&self) -> bool {
        self.palette().translucent
    }

    /// The in-app translucency switch, which `docs/05-design-language.md` §3a asks for
    /// alongside the OS setting: a user may want the material off on one machine and
    /// on elsewhere. It cannot overrule the OS in the permissive direction — Reduce
    /// Transparency still wins.
    pub const fn set_translucency(&mut self, on: bool) {
        self.translucency_off = !on;
    }

    /// How see-through the floating chrome is, as the slider shows it.
    ///
    /// Reports the palette's own figure when the user has not chosen one, so the
    /// control opens where the material actually is rather than at some placeholder.
    pub const fn glass_opacity(&self) -> u8 {
        match self.glass_opacity {
            Some(opacity) => opacity,
            None => Palette::resolve(self.theme, self.system).glass_opacity,
        }
    }

    /// Sets the tint strength. `None` returns to the palette's own figure.
    pub const fn set_glass_opacity(&mut self, opacity: Option<u8>) {
        self.glass_opacity = opacity;
    }

    /// Opens the right-button menu at a screen position.
    ///
    /// The app calls this, not the chrome, because deciding *what* was clicked needs
    /// the scene: a right-click on an item selects it and arrives as
    /// [`ContextTarget::Selection`]; one on bare board arrives as
    /// [`ContextTarget::Canvas`]. See `vellum_app::actions::open_context_menu`.
    pub const fn open_context_menu(&mut self, at: egui::Pos2, target: ContextTarget) {
        self.context_menu.open(at, target);
    }

    /// Closes it, for a gesture that starts elsewhere — a pan, a new selection.
    pub const fn close_context_menu(&mut self) {
        self.context_menu.close();
    }

    pub const fn context_menu_open(&self) -> bool {
        self.context_menu.is_open()
    }

    /// Whether the docked properties panel is showing. Off until asked for.
    pub const fn properties_open(&self) -> bool {
        self.properties_open
    }

    pub const fn set_properties_open(&mut self, open: bool) {
        self.properties_open = open;
    }

    /// Opens a tool's flyout — the shape picker, the pen's presets, the eraser's modes.
    ///
    /// One of the diagnostics group below. See [`Self::open_command_palette`] for why they
    /// exist at all.
    /// The colour the sticky tool will place, or `None` for the palette's default.
    pub const fn sticky_color(&self) -> Option<vellum_doc::Color> {
        self.toolbar.sticky
    }

    pub const fn set_open_flyout(&mut self, flyout: Option<crate::tool::Flyout>) {
        self.toolbar.open_flyout = flyout;
    }

    /// Raises the command palette, as `⌘K` does.
    ///
    /// # Why these four setters exist
    ///
    /// `--screenshot` renders one frame of a window nobody is touching, so a surface that only
    /// a click can raise could never be photographed unattended — and the properties panel,
    /// the palette, the find bar and the tool flyouts are all in that group. That is exactly
    /// the gap `--open-dialog` was added to close for modals, and the same argument applies:
    /// the chrome that is hardest to get right was the chrome nothing could check.
    ///
    /// They set the state directly rather than pushing a synthetic `⌘K` into egui's input.
    /// Both work; a key has to be injected before `take_egui_input` runs, which makes it
    /// order-dependent on a frame the app has not started yet, while a setter is legible from
    /// the first-frame block where every other `--flag` is applied.
    pub fn open_command_palette(&mut self) {
        self.command_palette.open();
    }

    /// Raises the find bar, as `⌘F` does.
    pub fn open_find(&mut self) {
        self.find.open();
    }

    /// Supplies the user's own SVG shapes to the shape picker.
    ///
    /// The chrome has no SVG parser and no rasteriser, so a custom shape is a name, an
    /// id the app chose, and optionally a preview texture the app has already uploaded.
    pub fn set_custom_shapes(&mut self, shapes: Vec<CustomShape>) {
        self.custom_shapes = shapes;
    }

    pub fn custom_shapes(&self) -> &[CustomShape] {
        &self.custom_shapes
    }

    /// Puts the keyboard in the selected item's text field on the next frame.
    ///
    /// The app calls this when the user has asked to *write* rather than to restyle —
    /// a double click on the canvas, or a sticky that has just been placed. There is
    /// no caret on the canvas yet (`docs/features/README.md` §3 puts rich-text editing
    /// at P4), so the properties panel's field is where the words go, and this is what
    /// makes reaching it a gesture rather than a hunt.
    pub fn focus_text(&mut self) {
        self.properties.focus_text();
    }

    /// Withdraws a pending request for the panel's text field. See
    /// `Properties::release_text_focus`.
    pub fn release_text_focus(&mut self) {
        self.properties.release_text_focus();
    }

    /// Whether the `Cmd+K` palette is showing.
    pub const fn command_palette_is_open(&self) -> bool {
        self.command_palette.is_open()
    }

    /// Whether the `Cmd+F` find bar is showing, and what it is searching for.
    pub fn find_query(&self) -> Option<&str> {
        self.find.is_open().then(|| self.find.query())
    }

    /// Where the floating chrome was drawn last frame, so `vellum-render` can blur a
    /// downsampled copy of the canvas behind exactly those rectangles and nothing
    /// else.
    ///
    /// Empty when the material is off — under Reduce Transparency there is nothing to
    /// blur, and spending the frame budget on it anyway is the failure mode §3a's
    /// performance section exists to prevent. Read straight after [`Chrome::show`];
    /// the rectangles describe the frame that was just built.
    pub fn glass_surfaces(&self) -> &[GlassSurface] {
        &self.glass
    }

    /// Supplies the user's folders to the board library.
    ///
    /// Membership is by path, so the app can build these from whatever it organises
    /// boards with without the library learning a second identity for a board.
    pub fn set_spaces(&mut self, spaces: Vec<Space>) {
        self.library.spaces = spaces;
    }

    pub fn spaces(&self) -> &[Space] {
        &self.library.spaces
    }

    /// Supplies the boards that are open.
    ///
    /// A *membership* update: tabs already on the strip keep their slot and take their
    /// new title and dirty flag, tabs the app has added arrive at the end — which is
    /// what makes a newly opened board appear on top, the way a browser opens a page —
    /// and tabs it has dropped go. The order the user dragged the strip into and the
    /// tab that is in front both survive, because those belong to the interface rather
    /// than to the app. Ignored outright while a tab is being dragged.
    ///
    /// Cheap enough to call every frame, which is how the unsaved-work dot keeps up
    /// with autosave.
    pub fn set_tabs(&mut self, tabs: Vec<BoardTab>) {
        self.tabs.set_tabs(tabs);
    }

    /// The open boards, in the order the strip draws them. Strip index `n` is
    /// `tabs()[n - 1]`; index `0` is the home tab and has no entry here.
    pub fn tabs(&self) -> &[BoardTab] {
        self.tabs.tabs()
    }

    /// Strip index of the tab in front. `0` is home — the board library — so this is
    /// also the answer to which [`Screen`] the app should be showing.
    pub fn active_tab(&self) -> usize {
        self.tabs.active()
    }

    /// Whether the home tab is in front, which is to say whether the window is showing
    /// the board library.
    pub fn showing_library(&self) -> bool {
        self.tabs.is_home()
    }

    /// Brings a tab to the front without asking the strip to report it — for the app
    /// putting the board it has just opened on top.
    pub fn set_active_tab(&mut self, index: usize) {
        self.tabs.set_active(index);
    }

    /// Opens a board as a tab and brings it to the front, appending it when it is not
    /// already there. Returns its strip index.
    ///
    /// The one call an app that opens one board at a time needs: it neither has to
    /// keep a list of its own nor rebuild one to hand back through
    /// [`Chrome::set_tabs`], and opening the same board twice switches to the tab it
    /// already has rather than making a second one.
    pub fn open_tab(&mut self, tab: BoardTab) -> usize {
        self.tabs.open(tab)
    }

    /// Updates one tab's unsaved-work dot, leaving its title and slot alone. Cheap
    /// enough for every frame, which is what following an autosave takes.
    pub fn set_tab_dirty(&mut self, key: crate::tabs::TabKey, dirty: bool) {
        self.tabs.set_dirty(key, dirty);
    }

    /// The strip itself, for the app that wants to close a tab or move the selection
    /// through the same state machine the interface uses rather than a second copy of
    /// its rules.
    pub const fn tab_strip(&mut self) -> &mut TabStrip {
        &mut self.tabs
    }

    /// The shape a click with the shape tool will place.
    pub const fn shape(&self) -> Shape {
        self.toolbar.shape
    }

    /// The pen tool's current settings.
    pub const fn pen(&self) -> PenPreset {
        self.toolbar.pen
    }

    /// What the eraser takes, as chosen in its flyout. ⇧ inverts it for one gesture —
    /// see [`EraserMode::with_shift`](crate::EraserMode::with_shift).
    pub const fn eraser(&self) -> crate::tool::EraserMode {
        self.toolbar.eraser
    }

    /// Whether an overlay the user is **typing into** is up: a dialog, the command
    /// palette, or the find bar.
    ///
    /// The same three that make [`Self::shortcuts`] stand down, and for a related
    /// reason — while one of these is showing, the board behind it is scenery. The app
    /// uses it to keep background work off the frames where a keystroke has to land;
    /// see `vellum_app::assets::Assets::suspend_decoding`.
    pub fn has_modal(&self) -> bool {
        self.dialogs.is_showing() || self.command_palette.is_open() || self.find.is_open()
    }

    /// Queues a modal question. The answer arrives as [`UiEvent::Dialog`].
    pub fn ask(&mut self, dialog: Dialog) {
        self.dialogs.push(dialog);
    }

    /// Withdraws a queued question the app has since answered itself.
    pub fn withdraw(&mut self, id: crate::event::DialogId) {
        self.dialogs.dismiss(id);
    }

    /// Puts the board library on one of its scopes.
    ///
    /// For `--show settings`, which needs to reach a page that is otherwise one sidebar
    /// click away — and a click is the one thing an unattended `--screenshot` run cannot
    /// make. The same argument `--open-dialog` and the seven flyouts already carry.
    pub fn set_library_scope(&mut self, scope: crate::LibraryScope) {
        self.library.scope = scope;
    }

    /// Which page of the settings is open. See [`Self::set_library_scope`].
    pub fn set_settings_tab(&mut self, tab: crate::SettingsTab) {
        self.library.settings_tab = tab;
    }

    /// What the app knows about the account, for Settings ▸ Account. Called every frame.
    ///
    /// **It writes the app's half of [`AccountFields`](crate::library::AccountFields) and
    /// never the three text buffers.** Those belong to the person typing, and a per-frame
    /// write would replace what they were half way through entering, sixty times a second.
    /// The one place the app seeds a buffer is [`Self::seed_account`], which runs once.
    ///
    /// `signed_in_as` and `signed_in_to` are ignored unless `state` is
    /// [`AccountState::SignedIn`](crate::library::AccountState::SignedIn); the page draws
    /// neither in any other state.
    ///
    /// `upload` is a first run in flight, or `None`. Six arguments including `self`, which is
    /// under clippy's threshold of seven — worth stating, because the seventh would have to
    /// be a struct rather than another parameter.
    pub fn set_account_status(
        &mut self,
        state: crate::library::AccountState,
        signed_in_as: &str,
        signed_in_to: &str,
        message: Option<&str>,
        upload: Option<crate::library::UploadProgress>,
    ) {
        let account = &mut self.library.account;
        account.state = state;
        // `Copy` and four `u32`s, so this one is assigned rather than compared first.
        account.upload = upload;
        // Compared before assigning, so an unchanged sentence is not a fresh `String` per
        // frame. The page reads these; nothing else does.
        if account.signed_in_as != signed_in_as {
            account.signed_in_as = signed_in_as.to_owned();
        }
        if account.signed_in_to != signed_in_to {
            account.signed_in_to = signed_in_to.to_owned();
        }
        if account.message.as_deref() != message {
            account.message = message.map(str::to_owned);
        }
    }

    /// Fills the address and the name in from what was saved last time.
    ///
    /// **Once, at startup**, and never again: this writes buffers the person is otherwise the
    /// only owner of. See [`Self::set_account_status`] for the half that is safe per frame.
    pub fn seed_account(&mut self, server: &str, username: &str) {
        self.library.account.server = server.to_owned();
        self.library.account.username = username.to_owned();
    }

    /// Shows a transient message.
    pub fn toast(&mut self, ctx: &Context, toast: Toast) {
        let now = ctx.input(|i| i.time);
        self.dialogs.toast(toast, now);
    }

    /// Draws the whole chrome for one frame.
    ///
    /// `ui` is the root ui of the pass — the one `Context::run_ui` hands out. The
    /// docked panels take their space out of it, so what is left is the canvas.
    pub fn show(&mut self, ui: &mut Ui, state: &ChromeState<'_>) -> ChromeOutput {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // The **palette**, not `(theme, system)`: the accent is a preference applied after
        // `Palette::resolve`, so a guard that did not include it left every field egui
        // paints for itself — text selection, the caret, hyperlinks, the active widget's
        // stroke — in the default accent for the life of the process.
        let palette = self.palette();
        let signature = (self.theme, self.system, self.accent);
        if self.applied != Some(signature) {
            crate::theme::apply_palette(ctx, self.theme, palette);
            self.applied = Some(signature);
        }

        let model = PanelModel::derive(state.selection);
        // The two facts the state cannot carry, because they belong to the chrome
        // rather than to the app: how many spaces the library holds, and whether the
        // operating system has already refused translucency.
        let cmd_ctx = CommandContext {
            spaces: self.library.spaces.len(),
            transparency_blocked: self.system.reduce_transparency
                || self.system.increase_contrast,
            ..state.command_context()
        };
        let mut events = EventSink::default();

        // Hoisted out of the header so `ChromeOutput` can report them: the native menu
        // bar ticks from exactly these, and a second derivation of "is the grid on" is
        // how two menus come to disagree — and the presenting early return below needs
        // them too.
        let flags = MenuFlags {
            link_previews: state.link_previews,
            align_objects: state.align_objects,
            snap_to_grid: state.snap_to_grid,
            minimap_visible: state.view.minimap_visible,
            presenting: state.view.presenting,
            starred: state.board.starred,
            translucent: palette.translucent,
            properties_panel: self.properties_open,
        };

        self.shortcuts(ctx, state, &cmd_ctx, &mut events);

        let mut panels: Vec<Rect> = Vec::new();
        // Where the floating tool palette ended up. The context bar is kept clear of
        // it — see the note where `toolbar::show` returns this.
        let mut tool_palette = Rect::NOTHING;
        self.glass.clear();

        // **Presenting: the chrome stands down entirely.**
        //
        // Not "most of it" — a presentation with a toolbar down one side is a screen
        // share of an editor, which is the thing a presentation mode exists to stop
        // being. The whole window becomes canvas, nothing registers as glass, and
        // nothing is over the pointer.
        //
        // `shortcuts` has already run, so Escape still leaves and the arrow keys still
        // move between frames. That ordering is the whole reason this can be an early
        // return rather than a flag threaded through every panel.
        if state.screen == Screen::Board && state.view.presenting {
            self.find.close();
            return ChromeOutput {
                events: events.take(),
                canvas_rect: ui.max_rect(),
                pointer_over_ui: false,
                keyboard_captured: ctx.egui_wants_keyboard_input(),
                command_context: cmd_ctx,
                menu_flags: flags,
            };
        }

        // *"on the top i want there to [be] pages constantly"* — before the match,
        // because the strip belongs to the window rather than to either screen, and
        // above the menu bar, because that is where a browser puts it and the home tab
        // is how the user gets back to the library.
        panels.push(crate::tabs::show(ui, palette, &mut self.tabs, &mut events));

        // Drawn before the header is built, not in a match arm beside the board's.
        // `MenuHeader` borrows `self.library.spaces` and the library panel needs
        // `&mut self.library`, so the two cannot overlap — and cloning the spaces into
        // the header every frame to avoid that would be an allocation per frame to
        // work around an ordering that costs nothing to get right.
        if state.screen == Screen::Library {
            // Built here rather than borrowed from `MenuHeader`, which cannot exist yet —
            // it borrows `self.library.spaces` and the panel below needs `&mut self.library`.
            // Every field is `Copy` or borrows `state`, so there is nothing to clone.
            let settings = crate::library::SettingsView {
                accent: self.accent,
                glass_opacity: self.glass_opacity(),
                transparency_blocked: cmd_ctx.transparency_blocked,
                link_previews: state.link_previews,
                align_objects: state.align_objects,
                snap_to_grid: state.snap_to_grid,
            };
            crate::library::show(
                ui,
                palette,
                &mut self.library,
                state.library,
                state.now,
                &settings,
                &mut events,
            );
        }

        // `flags` was built above, before the presenting early return needed it. There used
        // to be a second, identical construction here that shadowed it — harmless while the
        // two agreed, and exactly the second derivation the comment above warns about the
        // moment a field is added to only one of them.

        // Read by the menu bar and by the context menu, which is drawn after the canvas
        // rectangle is known — hence out here rather than inside the board's arm.
        let header = crate::menu::MenuHeader {
            title: state.board.title,
            path: state.board.path,
            dirty: state.board.dirty,
            dark_theme: self.theme.is_dark(),
            preference: self.preference,
            background: state.background,
            grid: state.grid,
            spaces: &self.library.spaces,
            glass_opacity: self.glass_opacity(),
            accent: self.accent,
            flags,
        };

        if state.screen == Screen::Board {
            let pill = crate::menu::show(
                ui,
                palette,
                &header,
                &cmd_ctx,
                &mut self.rename,
                &mut events,
                &mut self.glass,
            );
            let bar = pill.rect;
            // The pill's `⋮` opens the same list the right button opens on bare canvas.
            // Reported rather than opened, exactly as `context_bar` does it, so `menu`
            // never has to know the context menu exists.
            if let Some(at) = pill.more_at {
                self.context_menu.open(at, ContextTarget::Canvas);
            }
            // The long form, and only when asked for. `properties::show` already
            // returns an empty rect at the right edge for an empty selection, so
            // the same call answers both "nothing selected" and "not wanted" and
            // the canvas arithmetic below is unchanged either way.
            let side = if self.properties_open {
                crate::properties::show(
                    ui,
                    palette,
                    &mut self.properties,
                    &model,
                    &cmd_ctx,
                    state.font_families,
                    &mut events,
                )
            } else {
                let available = ui.max_rect();
                Rect::from_min_max(available.right_top(), available.right_bottom())
            };
            tool_palette = crate::toolbar::show(
                ui,
                palette,
                &mut self.toolbar,
                state.tool,
                &self.custom_shapes,
                &cmd_ctx,
                &mut events,
                &mut self.glass,
            );
            crate::status::show(
                ui,
                palette,
                state.view.zoom,
                state.view.minimap_visible,
                &cmd_ctx,
                &mut events,
                &mut self.glass,
            );

            panels.push(bar);
            panels.push(side);
        }

        // Whatever the docked panels did not take — the tab strip included, since it
        // is one. Read from the ui rather than reconstructed from the panel
        // rectangles, so a panel that egui sizes differently — a scrollbar, a rounded
        // scale factor — cannot leave the canvas overlapping it by a pixel.
        let canvas = match state.screen {
            Screen::Library => Rect::NOTHING,
            Screen::Board => ui.available_rect_before_wrap(),
        };

        // **The floating chrome a selection gets**, and the whole of what the user asked
        // for: a small toolbar directly above whatever is selected, and the menu the
        // right button opens.
        //
        // Drawn here rather than inside the match because both need `canvas`. The bar is
        // constrained to it, so a selection against the window's edge keeps its last
        // control reachable; and neither takes width from it, because both float — which
        // is the difference between this and the column it replaces.
        if state.screen == Screen::Board {
            if let Some(anchor) = state.selection_rect {
                // The canvas, minus the gutter the tool palette floats in. The board
                // runs under that palette on purpose; a *toolbar* underneath it would
                // have controls that cannot be clicked.
                let room = if tool_palette.is_positive() {
                    Rect::from_min_max(
                        egui::pos2(tool_palette.right() + crate::theme::space::of(2), canvas.top()),
                        canvas.max,
                    )
                } else {
                    canvas
                };
                let out = crate::context_bar::show(
                    ctx,
                    palette,
                    &mut self.context_bar,
                    &model,
                    &cmd_ctx,
                    anchor,
                    room,
                    state.font_families,
                    &mut events,
                );
                // The bar's own radius, not the shared one — see `register_glass_rounded`.
                // Its frame opened to 10 and the blurred region behind it has to follow, or
                // the corners of the one surface still made of glass show unblurred canvas.
                crate::toolbar::register_glass_rounded(
                    &mut self.glass,
                    palette,
                    out.rect,
                    crate::theme::SELECTION_BAR_RADIUS,
                );
                // `⋮` is the same menu the right button opens. The bar reports where
                // rather than opening it, so `context_bar` never has to know the menu
                // exists.
                if let Some(at) = out.more_at {
                    self.context_menu.open(at, ContextTarget::Selection);
                }
            }
            // Deliberately **not** registered as glass: the menu is opaque now, and asking
            // the renderer to blur the board behind an opaque panel is a blur pass nobody
            // can see. `ContextMenu::show` carries the reasoning.
            let _ = self.context_menu.show(ctx, palette, &model, &cmd_ctx, &header, &mut events);
        }

        // Both overlays float over the canvas, above everything except a modal. They
        // are drawn after the panels so a menu click that opens one is already in the
        // event list by the time it is read, and before the dialogs so a modal still
        // covers them.
        if let Some(rect) = self.command_palette.show(ctx, palette, &cmd_ctx, &mut events) {
            self.glass.extend(crate::command_palette::glass(palette, rect));
        }
        if let Some(rect) = self.find.show(ctx, palette, state.find_matches, &mut events) {
            self.glass.extend(crate::find::glass(palette, rect));
        }

        self.dialogs.show(ctx, palette, &mut events);
        self.glass.extend_from_slice(self.dialogs.glass_surfaces());

        // The preferences the chrome owns are applied to itself as well as reported, so
        // the very next frame draws in the new palette without the app having to route
        // the event back — and so a build that has not wired these up yet still works.
        // Read after everything has drawn, so a command from a menu, a shortcut or the
        // palette itself all arrive by the same path.
        for event in events.as_slice() {
            match event {
                UiEvent::ThemeChanged(theme) => self.set_theme(*theme),
                UiEvent::ThemePreferenceChanged(preference) => {
                    self.set_theme_preference(*preference);
                }
                UiEvent::AccentChanged(accent) => {
                    self.accent = *accent;
                }
                UiEvent::GlassOpacityChanged(opacity) => {
                    self.glass_opacity = Some(*opacity);
                }
                UiEvent::Command(Command::ToggleTranslucency) => {
                    self.translucency_off = !self.translucency_off;
                }
                UiEvent::Command(Command::TogglePropertiesPanel) => {
                    self.properties_open = !self.properties_open;
                }
                UiEvent::Command(Command::CommandPalette) => self.command_palette.open(),
                UiEvent::Command(Command::Find) => self.find.open(),
                // A new tab shows the library, exactly as a browser's shows its start
                // page. Applied here rather than in the strip so the `+` button and
                // `Cmd+T` land in the same place.
                UiEvent::NewBoardTab => self.tabs.set_active(TabStrip::HOME),
                _ => {}
            }
        }
        if state.screen != Screen::Board {
            // A find bar over the board library would be searching a board that is not
            // there. Closing it silently is right: the user closed the board.
            self.find.close();
        }

        ChromeOutput {
            events: events.take(),
            canvas_rect: canvas,
            pointer_over_ui: pointer_over_ui(ctx, &panels),
            keyboard_captured: ctx.egui_wants_keyboard_input(),
            command_context: cmd_ctx,
            // Read after the self-applied events above, so a `TogglePropertiesPanel` on
            // this very frame reaches the native bar's tick on this frame too rather
            // than one behind.
            menu_flags: MenuFlags { properties_panel: self.properties_open, ..flags },
        }
    }

    /// Turns key presses into the same events the menus and buttons emit.
    ///
    /// Skipped entirely while a text field has focus. Otherwise `Delete` would fire
    /// while renaming a board and `V` would switch tools mid-word — and there is no
    /// binding here worth the risk of getting that wrong in one direction only.
    ///
    /// Both of a command's bindings are consumed, which is how Redo answers to Miro's
    /// `⌘Y` and to the platform's `⌘⇧Z` — see [`Command::alternate_shortcut`].
    fn shortcuts(
        &mut self,
        ctx: &Context,
        state: &ChromeState<'_>,
        cmd_ctx: &CommandContext,
        events: &mut EventSink,
    ) {
        // The two overlays own the keyboard while they are up, including the bindings
        // that opened them: `Cmd+K` has to reach the palette to close it, and if this
        // table consumed it first the palette would clear itself instead.
        if ctx.egui_wants_keyboard_input()
            || self.dialogs.is_showing()
            || self.command_palette.is_open()
            || self.find.is_open()
        {
            return;
        }

        // A caret on the canvas takes the keys a *typing hand* produces, and the four chords
        // it implements itself. Nothing else.
        //
        // The line is drawn at ⌘/⌃ rather than at the whole table, and the difference is
        // deliberate. Everything without one of those two is something typing emits by
        // definition — a bare letter, a shifted capital, Backspace, and on macOS an ⌥ chord,
        // which is a character (`⌥D` is `∂`). Everything *with* one is a chord nobody
        // produces by accident, so `⌘Z`, `⌘S` and `⌘K` keep working mid-word exactly as they
        // do inside a focused field, which is what Miro does. Suppressing the *whole* table
        // instead would be one line shorter and would take Undo away from someone in the
        // middle of typing, which is the moment they most want it.
        //
        // **The ⌘ rule alone is not enough, and assuming it was nearly shipped the same
        // bug it fixes.** A session claiming `⌘X` in the app does not take the key out of
        // egui's queue — `Shell::restore_clipboard_key` deliberately puts one *back* — so
        // the session cut the characters and this table cut the item they were in, ending
        // the edit on the way. [`Command::claimed_by_a_text_session`] is that list.
        let chords_only = state.text_session;

        // egui matches modifiers *logically*: an extra Shift or Alt is ignored, so a
        // press of `Cmd+Shift+Z` also matches the pattern `Cmd+Z`. Walking the table
        // in declaration order therefore fires Undo for Redo and Save for Save-as —
        // silently, because both commands exist and one of them runs. The fix is the
        // one egui's own documentation prescribes: consume the most specific binding
        // first, so a key that carries Shift is offered to the Shift bindings before
        // anything else sees it.
        for level in (0..=MAX_SPECIFICITY).rev() {
            for command in Command::ALL {
                // Every binding at this level is consumed, not just the first that
                // matched, so a key cannot fall through to a widget behind the chrome.
                //
                // NOTE Clippy offers `.any()` here and it is **wrong**: `any` short-circuits
                // on the first `true`, so a command whose primary shortcut matched would
                // leave its alternate un-consumed and that key would reach whatever is
                // behind the chrome. The fold is deliberate, and `f(x) || hit` — not
                // `hit || f(x)` — is what keeps `f` running on every element.
                #[expect(clippy::unnecessary_fold, reason = "consuming every binding is the point")]
                let pressed = [command.shortcut(), command.alternate_shortcut()]
                    .into_iter()
                    .flatten()
                    .filter(|s| specificity(s.modifiers) == level)
                    .filter(|s| {
                        !chords_only
                            || ((s.modifiers.command || s.modifiers.ctrl)
                                && !command.claimed_by_a_text_session())
                    })
                    .fold(false, |hit, shortcut| {
                        ctx.input_mut(|i| i.consume_shortcut(&shortcut)) || hit
                    });
                if pressed && command.is_enabled(cmd_ctx) {
                    events.command(*command);
                }
            }
        }

        self.tab_shortcuts(ctx, events);

        if state.screen != Screen::Board {
            return;
        }
        // Both loops below are bare keys by construction, so a text session takes all of
        // them: `v`, `n`, `t`, `f` are tools and `r`, `o` are shapes — the letters of
        // ordinary words, which is how typing came to arm the shape tool and how the click
        // that left the field then placed a rectangle.
        if chords_only {
            return;
        }
        for tool in Tool::ALL {
            let keys = [tool.shortcut(), tool.alternate_shortcut()];
            // Both bindings are consumed, for the reason the shortcut loop above gives:
            // `.any()` would short-circuit and leave the alternate key live.
            #[expect(clippy::unnecessary_fold, reason = "consuming every binding is the point")]
            let pressed = keys.into_iter().flatten().fold(false, |hit, key| {
                ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key)) || hit
            });
            if pressed && tool != state.tool {
                self.toolbar.open_flyout = None;
                events.push(UiEvent::ToolChanged(tool));
            }
        }

        // Miro's shape keys. Each both picks the shape and arms the shape tool, which
        // is the whole value of the binding: one keystroke from thinking about a
        // rectangle to dragging one out. The tool event is emitted alongside rather
        // than left for the app to infer, matching what the flyout does.
        for (key, shape) in crate::tool::SHAPE_SHORTCUTS {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key)) {
                self.toolbar.shape = shape;
                self.toolbar.open_flyout = None;
                events.push(UiEvent::ShapeChosen(shape));
                if state.tool != Tool::Shape {
                    events.push(UiEvent::ToolChanged(Tool::Shape));
                }
            }
        }
    }
}

impl Chrome {
    /// The tab strip's own keys, which work on both screens because the strip does.
    ///
    /// Three bindings, and each of the obvious alternatives was tried and rejected for
    /// a reason worth keeping:
    ///
    /// - **`⌘T`** opens a new tab, which shows the library — a browser's *new tab*,
    ///   and what `CLAUDE.md` asks for in the same words.
    /// - **`⌘⌥←` / `⌘⌥→`** cycle. They are what Safari and Chrome bind on macOS and
    ///   they collide with nothing here.
    /// - **Not `⌃⇥` / `⌃⇧⇥`.** *Measured, not assumed:* egui moves keyboard focus on
    ///   `Tab` in `Memory::begin_pass`, before any application code runs, and it does
    ///   not look at `ctrl`. Consuming the key here is therefore too late — `⌃⇧⇥`
    ///   cycled the tab **and** put the caret in the library's search field, after
    ///   which `egui_wants_keyboard_input` was true and every other shortcut in the
    ///   app stopped working until the user clicked away. A binding that disables the
    ///   keymap as a side effect is worse than no binding.
    /// - **Not `⌘⇧[` / `⌘⇧]`**, which `CLAUDE.md` also asks for: they are already
    ///   Miro's *Send to back* and *Bring to front*, bindings the user's hands know
    ///   from the board — which is where they will be pressed. Breaking the frequent
    ///   gesture to serve the rare one is the wrong trade.
    /// - **Not `⌘⇥`**: macOS eats it before an application sees it.
    ///
    /// `⌘W` is not here either: it is already [`Command::CloseBoard`], and the app
    /// closes the tab in front when it runs — one binding, one meaning.
    fn tab_shortcuts(&mut self, ctx: &Context, events: &mut EventSink) {
        const ALT_CMD: Modifiers = Modifiers::COMMAND.plus(Modifiers::ALT);

        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::T)) {
            events.push(UiEvent::NewBoardTab);
        }

        let mut step: i64 = 0;
        for (key, direction) in [(Key::ArrowLeft, -1), (Key::ArrowRight, 1)] {
            if ctx.input_mut(|i| i.consume_key(ALT_CMD, key)) {
                step += direction;
            }
        }
        if let Some(next) = self.cycle_tab(step) {
            events.push(UiEvent::SelectTab(next));
        }
    }

    /// Moves `step` tabs along, wrapping, and reports where it landed.
    ///
    /// `None` when nothing moved. Public because the **native** menu bar's *Select next
    /// tab* claims `⌘⌥→` at `performKeyEquivalent:` — before `egui` ever sees the key —
    /// so the app has to be able to do from a menu click exactly what the shortcut above
    /// does, and one definition of "wrapping, through home" is the only way the two can
    /// agree.
    pub fn cycle_tab(&mut self, step: i64) -> Option<usize> {
        if step == 0 {
            return None;
        }
        // Wrapping, and through home rather than around it: the library is a tab, so
        // cycling reaches it like any other.
        let len = self.tabs.len() as i64;
        if len == 0 {
            return None;
        }
        let next = (self.tabs.active() as i64 + step).rem_euclid(len) as usize;
        if next == self.tabs.active() {
            return None;
        }
        self.tabs.set_active(next);
        Some(next)
    }
}

/// How many modifiers a binding insists on.
///
/// The ordering key for [`Chrome::shortcuts`]. `Command` is the platform key — ⌘ or
/// Ctrl — and counts like any other, because a bare `Delete` must be offered last of
/// all rather than swallowing `Cmd+Delete` on the way past.
const fn specificity(modifiers: Modifiers) -> u8 {
    modifiers.shift as u8 + modifiers.alt as u8 + (modifiers.command || modifiers.ctrl) as u8
}

/// The most modifiers any binding in the table carries: ⌘ plus one of ⇧/⌥.
const MAX_SPECIFICITY: u8 = 2;

/// Whether the pointer is over any part of the chrome.
///
/// `Context::is_pointer_over_egui` cannot be used: it treats the whole background
/// layer as canvas, and the docked panels live in that layer. Their rectangles are
/// checked explicitly, and everything above the background — menus, popovers,
/// flyouts, dialogs — is caught by the layer test.
fn pointer_over_ui(ctx: &Context, panels: &[Rect]) -> bool {
    let Some(pos) = ctx.pointer_interact_pos() else { return false };
    if panels.iter().any(|rect| rect.contains(pos)) {
        return true;
    }
    ctx.layer_id_at(pos).is_some_and(|layer| layer.order != egui::Order::Background)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::format_shortcut;
    use crate::event::{DialogEvent, DialogId};
    use crate::selection::ItemFacet;
    use egui::Key;
    use vellum_doc::{ItemId, Placement};

    fn item(n: i32, facet: ItemFacet) -> SelectionItem {
        SelectionItem::new(
            format!("{n}@1").parse::<ItemId>().unwrap(),
            facet,
            Placement::new(0.0, 0.0, 100.0, 100.0),
        )
    }

    fn board_state<'a>(selection: &'a [SelectionItem]) -> ChromeState<'a> {
        ChromeState {
            screen: Screen::Board,
            board: BoardState {
                title: "Site plan",
                path: Some(Path::new("/boards/site-plan.vellum")),
                starred: false,
                dirty: true,
                can_undo: true,
                can_redo: false,
                clipboard_has_content: true,
            },
            selection,
            ..ChromeState::default()
        }
    }

    /// Runs one chrome frame and returns what it emitted.
    fn frame(
        ctx: &Context,
        chrome: &mut Chrome,
        state: &ChromeState<'_>,
        input: egui::RawInput,
    ) -> ChromeOutput {
        let mut output = None;
        let _ = ctx.run_ui(input, |ui| {
            output = Some(chrome.show(ui, state));
        });
        output.expect("the chrome ran")
    }

    fn key_input(modifiers: Modifiers, key: Key) -> egui::RawInput {
        let mut input = egui::RawInput { modifiers, ..egui::RawInput::default() };
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        });
        input
    }

    #[test]
    fn the_library_screen_leaves_no_canvas_and_draws_without_events() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let output = frame(&ctx, &mut chrome, &ChromeState::default(), egui::RawInput::default());
        assert!(output.events.is_empty());
        assert_eq!(output.canvas_rect, Rect::NOTHING);
    }

    /// A docked panel must actually take space out of the canvas, or the renderer
    /// draws the board underneath it and the user sees it through the gaps between
    /// controls.
    ///
    /// Both halves, because the properties panel is **off by default** now — the
    /// floating `context_bar` is what a selection gets — and "the canvas runs to the
    /// window edge" is as much a part of the contract as "the panel is reserved when
    /// it is open".
    ///
    /// The menu bar is **no longer reserved**, and that is the point of this assertion
    /// now: it is a floating pill over the canvas rather than a docked full-width bar,
    /// so the board runs underneath it and the 36 points it used to take across the
    /// whole window come back. Only the tab strip is still docked.
    #[test]
    fn the_board_screen_reserves_the_menu_bar_and_the_properties_panel_when_it_is_open() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let selection = [item(1, ItemFacet::Sticky)];
        let state = board_state(&selection);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let closed = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let screen = ctx.content_rect();
        assert_eq!(
            closed.canvas_rect.top(),
            crate::theme::TAB_STRIP_HEIGHT,
            "the canvas starts under the strip; the menu pill floats over it"
        );
        assert!(
            closed.canvas_rect.right() > screen.right() - crate::theme::PROPERTIES_WIDTH,
            "the panel is closed, so the canvas should reach the edge: {:?} in {screen:?}",
            closed.canvas_rect
        );

        chrome.set_properties_open(true);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let open = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        assert!(
            open.canvas_rect.right() <= screen.right() - crate::theme::PROPERTIES_WIDTH,
            "canvas {:?} overlaps the properties panel in {screen:?}",
            open.canvas_rect
        );
        assert!(open.canvas_rect.width() > 0.0 && open.canvas_rect.height() > 0.0);
    }

    #[test]
    fn a_command_shortcut_emits_its_command_when_the_command_is_available() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let selection = [item(1, ItemFacet::Sticky)];
        let state = board_state(&selection);

        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, Key::Z));
        assert_eq!(output.events, vec![UiEvent::Command(Command::Undo)]);
    }

    /// The clipboard chords, which are the ones that were broken.
    ///
    /// This table matched them all along; what never arrived was the key. `egui-winit`
    /// turns `⌘V`/`⌘C`/`⌘X` into `Event::Paste`/`Copy`/`Cut` and returns *without*
    /// pushing `Event::Key`, so `consume_shortcut(⌘V)` below could never fire and
    /// `Command::Paste` was unreachable from the keyboard — the user's "I took a
    /// screenshot and it still didn't paste". `Shell::on_window_event` puts the key
    /// back; this asserts the half that then has to hold.
    ///
    /// Note what it does *not* prove: it feeds a synthetic `Event::Key` straight in,
    /// which is downstream of the layer that drops it. That is precisely why the
    /// existing tests here passed through the whole bug, and the reason is recorded
    /// rather than repeated — `crate::shell`'s mapper tests cover the other side.
    #[test]
    fn the_clipboard_chords_reach_their_commands() {
        for (key, command) in [
            (Key::V, Command::Paste),
            (Key::C, Command::Copy),
            (Key::X, Command::Cut),
        ] {
            let ctx = Context::default();
            let mut chrome = Chrome::new();
            let selection = [item(1, ItemFacet::Sticky)];
            let state = board_state(&selection);

            let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
            let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, key));
            assert_eq!(
                output.events,
                vec![UiEvent::Command(command)],
                "{command:?} did not answer its shortcut"
            );
        }
    }

    /// A command-held `V` must not *also* switch to the select tool.
    ///
    /// The tool keys are consumed with `Modifiers::NONE`, and if that ever matched
    /// loosely one press of `⌘V` would paste and change the tool underneath the
    /// user's hand — a bug that would only show up while pasting, which is the one
    /// time nobody is looking at the toolbar.
    #[test]
    fn pasting_does_not_also_pick_up_the_select_tool() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let selection = [item(1, ItemFacet::Sticky)];
        let state = board_state(&selection);

        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, Key::V));
        assert!(
            !output.events.iter().any(|event| matches!(event, UiEvent::ToolChanged(_))),
            "⌘V changed the tool as well as pasting: {:?}",
            output.events
        );
    }

    /// The gate that matters: a disabled command must not fire from the keyboard
    /// either, or Cmd+Z on a fresh board pushes an undo the app has to reject.
    #[test]
    fn a_shortcut_for_an_unavailable_command_is_swallowed_rather_than_emitted() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let mut state = board_state(&[]);
        state.board.can_undo = false;

        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, Key::Z));
        assert!(output.events.is_empty());
    }

    #[test]
    fn a_bare_tool_key_switches_tools_only_when_it_is_not_already_active() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::N));
        assert_eq!(output.events, vec![UiEvent::ToolChanged(Tool::Sticky)]);

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::V));
        assert!(output.events.is_empty(), "Select is already active");
    }

    #[test]
    fn tool_keys_do_nothing_on_the_library_screen() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = ChromeState::default();
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::N));
        assert!(output.events.is_empty());
    }

    /// A dialog is modal: the board's shortcuts must not run behind it, or Escape
    /// cancels the dialog *and* Delete removes the selection underneath.
    #[test]
    fn shortcuts_are_suspended_while_a_dialog_is_showing() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        chrome.ask(Dialog::confirm(DialogId(1), "Close board", "Unsaved changes.", "Discard"));
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let events = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::N)).events;
        assert!(
            !events.iter().any(|e| matches!(e, UiEvent::ToolChanged(_))),
            "{events:?}"
        );
    }

    #[test]
    fn a_queued_dialog_can_be_withdrawn_before_it_is_answered() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        chrome.ask(Dialog::rename(DialogId(4), "Rename board", "Site plan"));
        chrome.withdraw(DialogId(4));
        let output = frame(&ctx, &mut chrome, &board_state(&[]), egui::RawInput::default());
        assert!(
            !output.events.iter().any(|e| matches!(e, UiEvent::Dialog(_))),
            "a withdrawn dialog must not answer itself"
        );
    }

    #[test]
    fn the_command_context_is_derived_from_the_selection_rather_than_supplied() {
        let selection = [item(1, ItemFacet::Group), item(2, ItemFacet::Sticky)];
        let mut state = board_state(&selection);
        let ctx = state.command_context();
        assert_eq!(ctx.selected, 2);
        assert!(ctx.any_group);
        assert!(!ctx.any_locked);

        state.selection = &[];
        assert!(!state.command_context().any_group);
    }

    #[test]
    fn all_locked_needs_every_item_locked_not_merely_one() {
        let mut a = item(1, ItemFacet::Sticky);
        let b = item(2, ItemFacet::Sticky);
        a.locked = true;
        let selection = [a, b];
        let ctx = board_state(&selection).command_context();
        assert!(ctx.any_locked);
        assert!(!ctx.all_locked);
    }

    /// *"i want only light mode"*. Nothing the chrome offers, and nothing the OS
    /// says, can put it in the dark palette — including the OS flipping to dark
    /// mid-session, which is what this used to follow.
    #[test]
    fn the_chrome_is_light_whatever_is_asked_of_it() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        for preference in ThemePreference::ALL {
            chrome.set_theme_preference(preference);
            assert_eq!(chrome.theme(), Theme::Light, "{preference:?}");
            chrome.set_system_appearance(SystemAppearance { dark: true, ..Default::default() });
            assert_eq!(chrome.theme(), Theme::Light, "{preference:?} on a dark machine");
        }
        chrome.set_theme(Theme::Dark);
        assert_eq!(chrome.theme(), Theme::Light, "set_theme reached the dark cut");
        assert_eq!(chrome.palette(), Palette::LIGHT);

        let _ = frame(&ctx, &mut chrome, &ChromeState::default(), egui::RawInput::default());
        assert!(!ctx.global_style().visuals.dark_mode);
    }

    /// The accessibility contract, end to end: the OS reports reduced transparency
    /// and the chrome stops asking the renderer to blur anything.
    #[test]
    fn reduce_transparency_empties_the_glass_regions_and_opaques_the_palette() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);

        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        assert!(!chrome.glass_surfaces().is_empty(), "the toolbar and cluster float");
        assert!(chrome.palette().translucent);

        chrome.set_system_appearance(SystemAppearance {
            reduce_transparency: true,
            ..Default::default()
        });
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        assert!(chrome.glass_surfaces().is_empty(), "{:?}", chrome.glass_surfaces());
        assert!(!chrome.palette().translucent);
        assert_eq!(chrome.palette().glass_opacity, u8::MAX);
    }

    /// A stale glass rectangle would have the renderer blurring a region the chrome
    /// no longer occupies, so the list is rebuilt each frame rather than appended to.
    #[test]
    fn the_glass_regions_do_not_accumulate_across_frames() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let first = chrome.glass_surfaces().len();
        for _ in 0..4 {
            let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        }
        assert_eq!(chrome.glass_surfaces().len(), first);

        // The library screen has no floating chrome at all.
        let _ = frame(&ctx, &mut chrome, &ChromeState::default(), egui::RawInput::default());
        assert!(chrome.glass_surfaces().is_empty());
    }

    /// The right-click menu is **opaque**, and this is where that is enforced.
    ///
    /// It shipped as glass. The user photographed a menu opened over the reference board
    /// with a colourful blur coming through its top-right corner and asked for it to be
    /// fixed — `docs/05` §3a's *legibility wins over the material* case, on the one surface
    /// that gains least from the material.
    ///
    /// Asked as "does it register a glass region" rather than "what colour is its fill",
    /// because the register is what the *renderer* acts on: a menu drawn opaque that still
    /// registered would cost a blur pass behind an opaque panel every frame it was open, and
    /// no screenshot could ever show it.
    #[test]
    fn opening_the_context_menu_adds_no_glass_for_the_renderer_to_blur() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let picked = [item(1, ItemFacet::Sticky)];
        let state = board_state(&picked);

        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let closed = chrome.glass_surfaces().len();
        assert!(closed > 0, "the toolbar and the cluster float, so this is not vacuous");

        chrome.open_context_menu(egui::pos2(200.0, 200.0), ContextTarget::Selection);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        assert!(chrome.context_menu_open(), "the menu has to actually be up");
        assert_eq!(
            chrome.glass_surfaces().len(),
            closed,
            "the context menu registered glass, so it is translucent over the board again"
        );
    }

    /// Miro binds `⌘Y`; macOS binds `⌘⇧Z`. The user's fingers know the first and the
    /// platform expects the second, so both have to fire.
    #[test]
    fn redo_fires_from_both_of_its_bindings() {
        for (modifiers, key) in [
            (Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z),
            (Modifiers::COMMAND, Key::Y),
        ] {
            let ctx = Context::default();
            let mut chrome = Chrome::new();
            let mut state = board_state(&[]);
            state.board.can_redo = true;

            let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
            let output = frame(&ctx, &mut chrome, &state, key_input(modifiers, key));
            assert_eq!(
                output.events,
                vec![UiEvent::Command(Command::Redo)],
                "{modifiers:?}+{key:?} did not redo"
            );
        }
    }

    /// egui matches modifiers logically, so `Cmd+Shift+S` also matches the pattern
    /// `Cmd+S`. Whichever of the two the table reaches first wins — which meant Save
    /// fired for Save-as, and Undo for Redo, with no error anywhere. Every pair where
    /// one binding is the other plus a modifier is checked, because the next one
    /// added will have the same problem.
    #[test]
    fn a_binding_with_more_modifiers_wins_over_the_one_it_contains() {
        let overlapping: Vec<(Command, Command)> = Command::ALL
            .iter()
            .flat_map(|specific| {
                Command::ALL.iter().filter_map(move |general| {
                    let (a, b) = (specific.shortcut()?, general.shortcut()?);
                    let contains = a.logical_key == b.logical_key
                        && specificity(a.modifiers) > specificity(b.modifiers);
                    (contains && specific != general).then_some((*specific, *general))
                })
            })
            .collect();
        assert!(!overlapping.is_empty(), "nothing overlaps, so this test proves nothing");

        for (specific, general) in overlapping {
            let shortcut = specific.shortcut().expect("filtered on it");
            let ctx = Context::default();
            let mut chrome = Chrome::new();
            let selection = [item(1, ItemFacet::Sticky), item(2, ItemFacet::Sticky)];
            let mut state = board_state(&selection);
            state.board.can_redo = true;

            let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
            let output = frame(
                &ctx,
                &mut chrome,
                &state,
                key_input(shortcut.modifiers, shortcut.logical_key),
            );
            assert!(
                !output.events.contains(&UiEvent::Command(general)),
                "{:?} fired {general:?} instead of {specific:?}",
                format_shortcut(shortcut, true)
            );
            assert!(
                output.events.contains(&UiEvent::Command(specific)),
                "{:?} did not fire {specific:?}: {:?}",
                format_shortcut(shortcut, true),
                output.events
            );
        }
    }

    #[test]
    fn delete_fires_from_backspace_as_well_as_from_forward_delete() {
        for key in [Key::Delete, Key::Backspace] {
            let ctx = Context::default();
            let mut chrome = Chrome::new();
            let selection = [item(1, ItemFacet::Sticky)];
            let state = board_state(&selection);

            let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
            let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, key));
            assert_eq!(output.events, vec![UiEvent::Command(Command::Delete)], "{key:?}");
        }
    }

    /// `R` and `O` from Miro's shape flyout: pick the shape *and* arm the tool.
    #[test]
    fn a_shape_key_picks_the_shape_and_arms_the_shape_tool() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::O));
        assert_eq!(
            output.events,
            vec![UiEvent::ShapeChosen(Shape::Ellipse), UiEvent::ToolChanged(Tool::Shape)]
        );
        assert_eq!(chrome.shape(), Shape::Ellipse);

        // With the tool already armed, only the shape changes — a second
        // `ToolChanged` for a tool that is already active is noise the app would have
        // to filter.
        let mut armed = board_state(&[]);
        armed.tool = Tool::Shape;
        let output = frame(&ctx, &mut chrome, &armed, key_input(Modifiers::NONE, Key::R));
        assert_eq!(output.events, vec![UiEvent::ShapeChosen(Shape::Rectangle)]);
        assert_eq!(chrome.shape(), Shape::Rectangle);
    }

    /// `L` is Miro's Line. There is no line *shape* — a line between two points is a
    /// connector in this document model — so the key reaches the connector tool.
    #[test]
    fn the_line_key_reaches_the_connector_tool() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::L));
        assert_eq!(output.events, vec![UiEvent::ToolChanged(Tool::Connector)]);
    }

    #[test]
    fn spaces_reach_the_library_and_come_back_out() {
        use crate::library::Space;
        let mut chrome = Chrome::new();
        assert!(chrome.spaces().is_empty());
        chrome.set_spaces(vec![
            Space::new("Cars", ["/boards/site-plan.vellum".into()]).pinned(),
            Space::new("Books", []),
        ]);
        assert_eq!(chrome.spaces().len(), 2);
        assert!(chrome.spaces()[0].pinned);
    }

    #[test]
    fn a_toast_is_accepted_and_expires_without_a_dialog_present() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        chrome.toast(&ctx, Toast::error("Import failed: unreadable .rtb"));
        let output = frame(&ctx, &mut chrome, &board_state(&[]), egui::RawInput::default());
        assert!(output.events.is_empty());
    }

    #[test]
    fn dialog_answers_reach_the_app_with_the_id_it_asked_with() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        chrome.ask(Dialog::confirm(DialogId(11), "Delete", "Sure?", "Delete"));
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::Escape));
        assert_eq!(
            output.events,
            vec![UiEvent::Dialog(DialogEvent::Cancelled(DialogId(11)))]
        );
    }

    /// `Cmd+K` is the whole point of the palette existing. It opens at the end of the
    /// frame the key arrived on and draws on the next, which is the one-frame latency
    /// every overlay in this crate accepts in exchange for the menus, the keymap and
    /// the palette itself all opening it through one path.
    #[test]
    fn cmd_k_opens_the_command_palette_and_escape_closes_it() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, Key::K));
        assert_eq!(output.events, vec![UiEvent::Command(Command::CommandPalette)]);
        assert!(chrome.command_palette_is_open());

        // It is drawn on the next frame, and Escape on the one after closes it.
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let _ = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::Escape));
        assert!(!chrome.command_palette_is_open());
    }

    /// The board's own bindings must not run underneath an open palette: `N` while
    /// typing "new board" would switch to the sticky tool behind it.
    #[test]
    fn the_board_keymap_is_suspended_while_the_palette_is_open() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        let _ = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, Key::K));
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::NONE, Key::N));
        assert!(
            !output.events.iter().any(|e| matches!(e, UiEvent::ToolChanged(_))),
            "{:?}",
            output.events
        );
    }

    #[test]
    fn cmd_f_opens_the_find_bar_and_leaving_the_board_closes_it() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, Key::F));
        assert_eq!(output.events, vec![UiEvent::Command(Command::Find)]);
        assert_eq!(chrome.find_query(), Some(""));

        // A find bar over the library would be searching a board that is not there.
        let _ = frame(&ctx, &mut chrome, &ChromeState::default(), egui::RawInput::default());
        assert_eq!(chrome.find_query(), None);
    }

    /// There is no appearance control left to emit one, and the event that used to
    /// arrive from it cannot move the palette even when delivered by hand.
    ///
    /// [`UiEvent::ThemePreferenceChanged`] is kept in the public interface on purpose:
    /// the app already persists it, and re-offering the choice later should be a
    /// change to one menu rather than to three crates.
    /// The slider moves the material and survives a round trip through the event, but
    /// it is a *preference*, not an override of an accessibility setting: with Reduce
    /// Transparency on, the chrome stays fully opaque whatever the user chose.
    #[test]
    fn the_transparency_slider_moves_the_material_but_never_beats_the_os() {
        let mut chrome = Chrome::new();
        let default = chrome.glass_opacity();

        let mut events = EventSink::default();
        events.push(UiEvent::GlassOpacityChanged(150));
        for event in events.as_slice() {
            if let UiEvent::GlassOpacityChanged(opacity) = event {
                chrome.set_glass_opacity(Some(*opacity));
            }
        }
        assert_ne!(chrome.glass_opacity(), default, "the slider moved nothing");
        assert_eq!(chrome.palette().glass_opacity, 150);
        assert!(chrome.palette().translucent);

        chrome.set_system_appearance(SystemAppearance {
            reduce_transparency: true,
            ..SystemAppearance::default()
        });
        assert!(!chrome.palette().translucent, "an accessibility setting was overruled");
        assert_eq!(chrome.palette().glass_opacity, u8::MAX);
    }

    /// Nothing chosen follows the palette, so moving the default later moves it for
    /// everyone who never touched the slider.
    #[test]
    fn an_unset_transparency_reports_the_palettes_own_figure() {
        let chrome = Chrome::new();
        assert_eq!(chrome.glass_opacity(), Palette::LIGHT.glass_opacity);
    }

    #[test]
    fn a_theme_preference_event_no_longer_moves_the_palette() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let mut events = EventSink::default();
        events.push(UiEvent::ThemePreferenceChanged(ThemePreference::Dark));
        for event in events.as_slice() {
            if let UiEvent::ThemePreferenceChanged(preference) = event {
                chrome.set_theme_preference(*preference);
            }
        }
        assert_eq!(chrome.theme(), Theme::Light);
        let _ = frame(&ctx, &mut chrome, &ChromeState::default(), egui::RawInput::default());
        assert!(!ctx.global_style().visuals.dark_mode);
    }

    /// `docs/05-design-language.md` §3a asks for an in-app override as well as the OS
    /// one. It can switch the material off, and it cannot switch it back on over an OS
    /// that has already refused.
    #[test]
    fn the_in_app_translucency_switch_turns_the_material_off_but_cannot_overrule_the_os() {
        let mut chrome = Chrome::new();
        assert!(chrome.translucency());

        chrome.set_translucency(false);
        assert!(!chrome.translucency());
        assert!(!chrome.palette().translucent);
        assert_eq!(chrome.palette().glass_opacity, u8::MAX);

        chrome.set_translucency(true);
        assert!(chrome.translucency());
        chrome.set_system_appearance(SystemAppearance {
            reduce_transparency: true,
            ..Default::default()
        });
        assert!(!chrome.translucency(), "the OS still wins");
    }

    #[test]
    fn custom_shapes_reach_the_picker_and_come_back_out() {
        use crate::tool::{CustomShape, CustomShapeId};
        let mut chrome = Chrome::new();
        assert!(chrome.custom_shapes().is_empty());
        chrome.set_custom_shapes(vec![CustomShape {
            id: CustomShapeId(3),
            name: "Wiring symbol".to_owned(),
            thumbnail: None,
        }]);
        assert_eq!(chrome.custom_shapes().len(), 1);
        assert_eq!(chrome.custom_shapes()[0].id, CustomShapeId(3));
    }

    /// A board that has never been saved has no library row, so the entries that act
    /// on one are withdrawn with a reason rather than acting on nothing.
    #[test]
    fn an_unsaved_board_withdraws_the_entries_that_need_a_file() {
        let mut state = board_state(&[]);
        state.board.path = None;
        let ctx = state.command_context();
        assert!(!ctx.board_saved);
        for command in [Command::StarBoard, Command::DuplicateBoard, Command::DeleteBoard] {
            assert_eq!(
                command.availability(&ctx).reason(),
                Some(crate::command::reason::UNSAVED_BOARD),
                "{command:?}"
            );
        }

        state.board.path = Some(Path::new("/boards/site-plan.vellum"));
        state.board.starred = true;
        let ctx = state.command_context();
        assert!(ctx.board_saved && ctx.board_starred);
        assert!(Command::StarBoard.is_enabled(&ctx));
    }

    /// `Cmd+T` is a browser's *new tab*, and a new tab here shows the board library —
    /// which is also what `CLAUDE.md` asks of it: *"⌘T opens the library tab"*.
    #[test]
    fn cmd_t_asks_for_a_new_tab_and_puts_home_in_front() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        chrome.set_tabs(vec![crate::tabs::BoardTab::new(crate::tabs::TabKey(1), "Site plan")]);
        chrome.set_active_tab(1);
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());

        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::COMMAND, Key::T));
        assert_eq!(output.events, vec![UiEvent::NewBoardTab]);
        assert!(chrome.showing_library(), "a new tab shows the library");
    }

    /// `⌘⌥←` / `⌘⌥→` cycle and wrap through home. `Tab` is deliberately not bound —
    /// see [`Chrome::tab_shortcuts`] — and the z-order keys the strip did not take
    /// still reach the board.
    #[test]
    fn the_tab_cycling_keys_wrap_through_home_and_leave_the_z_order_bindings_alone() {
        use crate::tabs::{BoardTab, TabKey};
        const ALT_CMD: Modifiers = Modifiers::COMMAND.plus(Modifiers::ALT);

        let ctx = Context::default();
        let mut chrome = Chrome::new();
        chrome.set_tabs(vec![
            BoardTab::new(TabKey(1), "One"),
            BoardTab::new(TabKey(2), "Two"),
        ]);
        let state = board_state(&[]);
        let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
        assert!(chrome.showing_library(), "the strip starts on home");

        // Forward: home → 1 → 2 → home.
        for expected in [1, 2, 0] {
            let output = frame(&ctx, &mut chrome, &state, key_input(ALT_CMD, Key::ArrowRight));
            assert_eq!(output.events, vec![UiEvent::SelectTab(expected)]);
            assert_eq!(chrome.active_tab(), expected);
        }
        // …and backwards, wrapping the other way.
        let output = frame(&ctx, &mut chrome, &state, key_input(ALT_CMD, Key::ArrowLeft));
        assert_eq!(output.events, vec![UiEvent::SelectTab(2)]);

        // `Tab` belongs to egui's focus traversal and is left alone: a binding that
        // also parks the caret in a text field disables the whole keymap behind it.
        let output = frame(&ctx, &mut chrome, &state, key_input(Modifiers::CTRL, Key::Tab));
        assert!(
            !output.events.iter().any(|e| matches!(e, UiEvent::SelectTab(_))),
            "{:?}",
            output.events
        );

        // …and the bindings the strip did *not* take still do what the board expects.
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let selection = [item(1, ItemFacet::Sticky)];
        let board = board_state(&selection);
        let _ = frame(&ctx, &mut chrome, &board, egui::RawInput::default());
        let output = frame(
            &ctx,
            &mut chrome,
            &board,
            key_input(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::CloseBracket),
        );
        assert_eq!(output.events, vec![UiEvent::Command(Command::BringToFront)]);
    }

    /// The strip is drawn on both screens — *"on the top i want there to [be] pages
    /// constantly"* — and it takes its band off the top of the window on both.
    #[test]
    fn the_tab_strip_is_drawn_on_every_screen_and_shortens_the_canvas() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        for state in [ChromeState::default(), board_state(&[])] {
            let _ = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
            let output = frame(&ctx, &mut chrome, &state, egui::RawInput::default());
            assert!(output.events.is_empty());
            if state.screen == Screen::Board {
                assert!(
                    output.canvas_rect.top() >= crate::theme::TAB_STRIP_HEIGHT,
                    "the canvas overlaps the strip: {:?}",
                    output.canvas_rect
                );
            }
        }
    }

    #[test]
    fn with_no_pointer_nothing_is_over_the_ui() {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        let output = frame(&ctx, &mut chrome, &board_state(&[]), egui::RawInput::default());
        assert!(!output.pointer_over_ui);
        assert!(!output.keyboard_captured);
    }
}
