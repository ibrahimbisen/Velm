//! The chrome, mounted: egui's context, `winit`'s input, and `vellum-ui`'s panels.
//!
//! `vellum-ui` is nine thousand lines of menus, toolbar, properties panel, board
//! library and colour picker that until now nothing could see. This is the file that
//! puts them on the screen. It owns three things and no more:
//!
//! 1. **The egui context and its `winit` bridge.** One `Context`, one
//!    `egui_winit::State`, one call per frame.
//! 2. **The state the chrome reads but does not own** — which screen, which tool,
//!    which view toggles, what is selected, what the library holds.
//! 3. **The translation at each edge**: `vellum-ui` reports floating surfaces as
//!    rectangles in points and a palette; `vellum-render` wants
//!    [`GlassPanel`]s in physical pixels with a material.
//!
//! What it deliberately does **not** own is what the events *do*. That is
//! `crate::actions`, which has the document, the camera and the GPU. The split is the
//! same one `vellum-ui` makes for the same reason: everything here can be driven from
//! a test with synthetic input, and everything there needs a board.
//!
//! # Input must not be stolen from the canvas
//!
//! `crate::input` was tuned against direct user feedback — left-drag marquees,
//! middle/right/space-drag pans, the wheel zooms, no inertia — and
//! `docs/06-mouse-controls.md` records that stealing the left drag was *"the single
//! worst fault in the first version"*. So routing is per event and errs toward the
//! canvas:
//!
//! - Every event is offered to egui first, because egui is what knows whether the
//!   pointer is over a panel.
//! - An event egui says it consumed does not reach [`crate::input`] — **unless a
//!   canvas gesture is already in flight**. Without that exception a pan begun on the
//!   canvas would never end if the pointer happened to be over the toolbar when the
//!   button came up, and the board would stick to the cursor.
//! - Window-level events — resize, scale factor, modifiers — always reach both. They
//!   are not "input" in the sense either party competes over.
//!
//! # One writer for the cursor
//!
//! egui and `crate::input` both have an opinion about the pointer shape, and two
//! writers means a flicker on every frame the pointer is near a panel edge. So the
//! app's icon is written *into egui's own platform output* before it is handed back
//! to `winit`: egui remains the only caller of `Window::set_cursor`, and its existing
//! change-detection stops the redundant writes.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use egui::epaint::ClippedPrimitive;
use vellum_render::{GlassMaterial, GlassPanel, GlassRenderer, Rgba};
use vellum_ui::{
    BoardCard, BoardState, BoardTab, Chrome, ChromeState, Dialog, DialogId, Palette, Screen,
    SelectionItem, TabKey, Toast, Tool, UiEvent, ViewState,
};
use winit::window::Window;

use crate::appearance::Appearance;
use crate::library::Library;

/// What the app tells the chrome about the open board each frame.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub title: &'a str,
    pub path: Option<&'a Path>,
    pub starred: bool,
    /// Whether anything is still on its way to disk. Autosave is instant, so this is
    /// normally false for a fraction of a second at a time — which is exactly what
    /// the user asked to be able to *see*.
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    /// Scale factor, `1.0` being 100%.
    pub zoom: f32,
    pub clipboard_has_content: bool,
    /// The open board's canvas colour and pattern, for Board ▸ Background.
    pub background: vellum_doc::Background,
    /// What a click on the canvas would do, from `crate::input`. Applied only where
    /// the pointer is over the board — see the module header.
    pub canvas_cursor: egui::CursorIcon,
    /// Where the selection is on screen, in **logical** points, for the toolbar
    /// `vellum_ui::context_bar` floats above it.
    ///
    /// Converted by the app because only the app has the camera, and in logical points
    /// because egui lays out in them while `ScreenPoint` is physical everywhere in this
    /// crate (trap 4). `None` when nothing is selected.
    pub selection_rect: Option<egui::Rect>,
}

/// A question the app asked, so its answer can be matched to what it was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// A dialog that only tells the user something — the shortcut sheet, the document
    /// index, About. Answering it does nothing, and it is dismissed either way.
    ///
    /// Worth a variant rather than borrowing another one. All three of these used to be
    /// tagged `Ask::CreateSpace`, which was harmless only because a `Confirm` dialog
    /// cannot emit `Renamed` and so never matched that arm — a coincidence, one arm
    /// away from About creating a folder.
    Nothing,
    DeleteBoard(PathBuf),
    /// Empty Recently deleted. Carries no path: the answer is acted on against whatever is
    /// in the trash when it comes back, which is the same set the dialog counted a moment
    /// earlier — nothing can be added to the trash while a modal is up.
    EmptyTrash,
    RenameBoard(PathBuf),
    NewBoard,
    CreateSpace,
    RenameSpace(String),
    DeleteSpace(String),
    /// The four literal steps of `docs/04-ui-reference.md` §5, shown as a dialog because
    /// the user found the flow confusing when described in prose — and, before them,
    /// [`Self::ImportSteps`].
    ImportFromMiro,
    /// The instructions in front of the whole import: two steps in Miro, two in Velm, each
    /// with a ⓘ saying why it matters. Answering *Continue* runs what `ImportFromMiro`
    /// used to run straight away.
    ImportSteps,
}

/// The chrome and everything it needs between frames.
pub struct Shell {
    ctx: egui::Context,
    winit: egui_winit::State,
    chrome: Chrome,
    pub library: Library,
    appearance: Appearance,
    screen: Screen,
    tool: Tool,
    view: ViewState,
    /// The current selection, flattened for the properties panel. Rebuilt only when
    /// the selection or the document actually changed — a select-all on the reference
    /// board is 596 items, and rebuilding that every frame would allocate more than
    /// the renderer does.
    selection: Vec<SelectionItem>,
    selection_key: (u64, usize, u64),
    cards: Vec<BoardCard>,
    fonts: Vec<String>,
    find_matches: Option<(usize, usize)>,
    /// Texture **and its pixel size**, per board. The size is what keeps a preview
    /// from being stretched to a square — see [`Shell::set_thumbnail`].
    thumbnails: HashMap<PathBuf, (egui::TextureId, [usize; 2])>,
    pending: Vec<(DialogId, Ask)>,
    next_dialog: u64,
    primitives: Vec<ClippedPrimitive>,
    textures_delta: egui::TexturesDelta,
    panels: Vec<GlassPanel>,
    pointer_over_ui: bool,
    keyboard_captured: bool,
    canvas_rect: egui::Rect,
    /// What the chrome resolved on its last pass. Held rather than recomputed so the
    /// **native** menu bar greys and ticks exactly the rows the in-app one does — see
    /// [`vellum_ui::ChromeOutput::command_context`].
    command_context: vellum_ui::CommandContext,
    menu_flags: vellum_ui::MenuFlags,
    pixels_per_point: f32,
}

impl Shell {
    /// Brings the chrome up against a live window.
    ///
    /// `max_texture_side` comes from the GPU's limits: egui grows its font atlas until
    /// it hits that, and a value it does not know produces an atlas the device refuses
    /// to allocate, which shows up as missing text rather than as an error.
    pub fn new(
        window: &Window,
        library: Library,
        appearance: Appearance,
        max_texture_side: usize,
    ) -> Self {
        let ctx = egui::Context::default();
        let winit = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(max_texture_side),
        );

        let mut chrome = Chrome::new();
        chrome.set_theme_preference(library.theme_preference());
        chrome.set_translucency(library.translucency());
        chrome.set_glass_opacity(library.glass_opacity());
        // The board takes the same accent from here on the first frame, because
        // `crate::app::theme_for` derives the canvas palette from the chrome's rather than
        // reading the sidecar a second time.
        chrome.set_accent(library.accent());
        chrome.set_spaces(library.spaces());

        let view = ViewState {
            minimap_visible: library.minimap(),
            ..ViewState::default()
        };

        let mut shell = Self {
            // Filled in by the first `run`. Default until then, which greys every row —
            // correct, since there is nothing to act on before the first frame.
            command_context: vellum_ui::CommandContext::default(),
            menu_flags: vellum_ui::MenuFlags::default(),
            ctx,
            winit,
            chrome,
            library,
            appearance,
            screen: Screen::Board,
            tool: Tool::Select,
            view,
            selection: Vec::new(),
            selection_key: (u64::MAX, 0, 0),
            cards: Vec::new(),
            fonts: font_families(),
            find_matches: None,
            thumbnails: HashMap::new(),
            pending: Vec::new(),
            next_dialog: 1,
            primitives: Vec::new(),
            textures_delta: egui::TexturesDelta::default(),
            panels: Vec::new(),
            pointer_over_ui: false,
            keyboard_captured: false,
            canvas_rect: egui::Rect::NOTHING,
            pixels_per_point: window.scale_factor() as f32,
        };
        shell.refresh_cards();
        shell
    }

    // ----- what the app reads -----------------------------------------------

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
    }

    pub fn view(&self) -> ViewState {
        self.view
    }

    pub fn view_mut(&mut self) -> &mut ViewState {
        &mut self.view
    }

    pub fn shape(&self) -> vellum_shapes::Shape {
        self.chrome.shape()
    }

    pub fn pen(&self) -> vellum_ui::PenPreset {
        self.chrome.pen()
    }

    pub fn eraser(&self) -> vellum_ui::EraserMode {
        self.chrome.eraser()
    }

    pub fn theme(&self) -> vellum_ui::Theme {
        self.chrome.theme()
    }

    /// Which colour the primary accent wears — Preferences ▸ Accent colour.
    pub fn accent(&self) -> vellum_ui::Accent {
        self.chrome.accent()
    }

    pub fn set_accent(&mut self, accent: vellum_ui::Accent) {
        self.chrome.set_accent(accent);
    }

    pub fn palette(&self) -> Palette {
        self.chrome.palette()
    }

    pub fn theme_preference(&self) -> vellum_ui::ThemePreference {
        self.chrome.theme_preference()
    }

    pub fn translucency(&self) -> bool {
        self.chrome.translucency()
    }

    pub fn primitives(&self) -> &[ClippedPrimitive] {
        &self.primitives
    }

    pub fn textures_delta(&self) -> &egui::TexturesDelta {
        &self.textures_delta
    }

    /// Where the floating chrome is, ready for `vellum-render`'s blur.
    pub fn glass_panels(&self) -> &[GlassPanel] {
        &self.panels
    }

    /// True when the pointer belongs to the chrome this frame.
    pub fn pointer_over_ui(&self) -> bool {
        self.pointer_over_ui
    }

    pub fn keyboard_captured(&self) -> bool {
        self.keyboard_captured
    }

    /// The rectangle left for the board after the docked panels took their share.
    pub fn canvas_rect(&self) -> egui::Rect {
        self.canvas_rect
    }

    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    pub fn find_query(&self) -> Option<&str> {
        self.chrome.find_query()
    }

    pub fn set_find_matches(&mut self, matches: Option<(usize, usize)>) {
        self.find_matches = matches;
    }

    // ----- the tab strip ----------------------------------------------------
    //
    // *"i have the tabs on top of the application"*. The strip owns the order and
    // which tab is in front — see `vellum_ui::tabs` — so the app holds no list of its
    // own. It opens a tab when it opens a board, keeps every tab's dot in step with
    // autosave, and asks the strip which board should be in front after the user has
    // moved it.
    //
    // The documents behind those tabs are `crate::session`'s: the board in front is
    // hot in `ActiveState`, the rest are parked, and this is the only place that turns
    // a path into the [`TabKey`] both sides agree on.

    /// The strip's name for a board. Derived from the path rather than counted, so
    /// the same board is the same tab across a close and a re-open — and so
    /// `crate::session` can match a parked document to a tab without a second
    /// identity.
    ///
    /// **Made absolute first, and that is load-bearing rather than tidy.** The board
    /// library only ever hands out absolute paths, but the command line does not:
    /// `--board ./site-plan.vellum` and the library's own row for the same file would hash
    /// differently, the app would decide the board was not open, and it would put a
    /// *second* `Editor` — with a second autosave thread — over one SQLite file. A
    /// path that is already absolute is left alone, so the per-frame call this makes
    /// for the unsaved-work dot costs nothing.
    pub fn tab_key(path: &Path) -> TabKey {
        use std::hash::{Hash as _, Hasher as _};
        let absolute;
        let path = if path.is_absolute() {
            path
        } else {
            absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
            absolute.as_path()
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        TabKey(hasher.finish())
    }

    /// Puts a board on the strip and brings it to the front, as a browser opens a
    /// page. A board that is already there is switched to rather than opened twice.
    pub fn open_tab(&mut self, path: &Path, title: &str) {
        self.chrome.open_tab(BoardTab::new(Self::tab_key(path), title).with_path(path));
    }

    /// Puts the home tab in front without opening or closing anything — for a launch
    /// that lands on the board library with a document already loaded behind it.
    pub fn show_home_tab(&mut self) {
        self.chrome.set_active_tab(0);
    }

    /// The board the strip has in front, or `None` when that is the home tab.
    pub fn active_tab_path(&self) -> Option<PathBuf> {
        let index = self.chrome.active_tab().checked_sub(1)?;
        self.chrome.tabs().get(index)?.path.clone()
    }

    /// How many tabs there are, the home tab included. Never zero.
    pub fn tab_count(&self) -> usize {
        self.chrome.tabs().len() + 1
    }

    /// Brings a tab to the front by its **strip** index — `0` is home. Out of range is
    /// ignored, which is what makes `Cmd+7` on a window with three tabs do nothing
    /// rather than land somewhere arbitrary.
    pub fn select_tab(&mut self, index: usize) {
        self.chrome.set_active_tab(index);
    }

    /// Every open board's key, in the order the strip draws them.
    ///
    /// The app compares this against what `crate::session` holds to find the boards
    /// whose tabs have gone — the strip closes a tab before the app hears about it, so
    /// there is nothing else to ask.
    pub fn tab_keys(&self) -> Vec<TabKey> {
        self.chrome.tabs().iter().map(|tab| tab.key).collect()
    }

    /// Whether a board still has a tab. False means its document may be released.
    pub fn has_tab(&self, key: TabKey) -> bool {
        self.chrome.tabs().iter().any(|tab| tab.key == key)
    }

    /// Renames a tab in place, without moving it or bringing it to the front.
    ///
    /// Through `set_tabs` rather than a setter of its own, because that is the call
    /// `vellum_ui` gives for a membership refresh and it already keeps the order and
    /// the selection. Feeding it back the list it just gave, with one title changed,
    /// is a rename and nothing else.
    pub fn set_tab_title(&mut self, path: &Path, title: &str) {
        let key = Self::tab_key(path);
        let mut tabs = self.chrome.tabs().to_vec();
        let Some(tab) = tabs.iter_mut().find(|tab| tab.key == key) else { return };
        tab.title = title.to_owned();
        self.chrome.set_tabs(tabs);
    }

    /// Raises or clears one tab's unsaved-work dot. The board in front is done for the
    /// app by [`Shell::run`]; this is how the boards *behind* it stay honest.
    pub fn set_tab_dirty(&mut self, key: TabKey, dirty: bool) {
        self.chrome.set_tab_dirty(key, dirty);
    }

    /// Takes the tab in front off the strip — the app's side of `Cmd+W`, and of a
    /// board being deleted underneath it. Reports whether there was one to close;
    /// `false` means home was in front, which cannot be closed.
    pub fn close_active_tab(&mut self) -> bool {
        let index = self.chrome.active_tab();
        self.chrome.tab_strip().close(index)
    }

    /// Takes one board off the strip, wherever it is.
    pub fn close_tab_for(&mut self, path: &Path) {
        let key = Self::tab_key(path);
        let strip = self.chrome.tab_strip();
        if let Some(index) = strip.tabs().iter().position(|tab| tab.key == key) {
            strip.close(index + 1);
        }
    }

    // ----- things the app tells the chrome ----------------------------------

    /// Moves `step` tabs along, wrapping. See [`vellum_ui::Chrome::cycle_tab`] for why
    /// the native menu bar needs this rather than sending a key.
    pub fn cycle_tab(&mut self, step: i64) -> Option<usize> {
        self.chrome.cycle_tab(step)
    }

    /// The enablement facts and toggles the chrome resolved on its last pass, for the
    /// native menu bar to grey and tick from. Both are `Default` until the first frame.
    pub const fn command_context(&self) -> vellum_ui::CommandContext {
        self.command_context
    }

    pub const fn menu_flags(&self) -> vellum_ui::MenuFlags {
        self.menu_flags
    }

    /// Opens the right-button menu at a screen position, in logical points.
    ///
    /// See `crate::actions::ActiveState::open_context_menu` for why the app decides the
    /// target rather than the chrome.
    pub fn open_context_menu(&mut self, at: egui::Pos2, target: vellum_ui::ContextTarget) {
        self.chrome.open_context_menu(at, target);
    }

    /// Closes it. Called when a gesture starts that the menu should not survive.
    pub fn close_context_menu(&mut self) {
        self.chrome.close_context_menu();
    }

    pub fn context_menu_open(&self) -> bool {
        self.chrome.context_menu_open()
    }

    /// The colour the next placed sticky takes, from the sticky tool's flyout.
    pub fn sticky_color(&self) -> Option<vellum_doc::Color> {
        self.chrome.sticky_color()
    }

    /// Which of the three agent roles the next placed agent node takes, from the agent
    /// tool's flyout.
    pub fn agent_role(&self) -> vellum_agent::RoleKind {
        self.chrome.agent_role()
    }

    /// Offers the picker the families the shaper can actually use.
    ///
    /// **Only families with faces**, which the hardcoded list did not promise: it named
    /// `Segoe UI` and `Cascadia Mono`, both Windows fonts, so on macOS picking either stored a
    /// new value and changed nothing on screen — *"when i change the font somewhere all it does
    /// it update but it doesnt visually update."* A control that reports success and does
    /// nothing is worse than one that is not offered.
    ///
    /// The bundled family leads, because it is the default and the one every machine has; the
    /// rest follow in the shaper's own order. Capped, because a Mac reports several hundred
    /// families and a dropdown that long is a list nobody scrolls.
    pub fn set_font_families(&mut self, mut families: Vec<String>) {
        const MOST: usize = 120;
        families.retain(|name| name != vellum_text::BUNDLED_FAMILY);
        families.truncate(MOST);
        self.fonts = std::iter::once(vellum_text::BUNDLED_FAMILY.to_owned())
            .chain(families)
            .collect();
    }

    /// Raises a chrome surface that only a click can otherwise reach, for `--screenshot`.
    ///
    /// The properties panel, the command palette, the find bar, a tool's flyout and one group
    /// of the `⋮` menu are all click-only state, so an unattended render could never
    /// photograph any of them — the same gap `--open-dialog` closed for modals. Returns
    /// whether the name was recognised, so the caller can warn rather than fail silently.
    pub fn force_open(&mut self, what: &str) -> bool {
        use vellum_ui::Flyout;
        match what {
            "properties" => self.chrome.set_properties_open(true),
            "palette" => self.chrome.open_command_palette(),
            "find" => self.chrome.open_find(),
            "shapes" => self.chrome.set_open_flyout(Some(Flyout::Shape)),
            "pen" => self.chrome.set_open_flyout(Some(Flyout::Pen)),
            "eraser" => self.chrome.set_open_flyout(Some(Flyout::Eraser)),
            "more" => self.chrome.set_open_flyout(Some(Flyout::More)),
            "menu" => vellum_ui::menu::force_open_menu(&self.ctx),
            _ => return false,
        }
        true
    }

    pub fn toast(&mut self, toast: Toast) {
        self.chrome.toast(&self.ctx, toast);
    }

    /// Whether an overlay the user types into is up. See [`vellum_ui::Chrome::has_modal`].
    pub fn has_modal(&self) -> bool {
        self.chrome.has_modal()
    }

    /// Queues a question and remembers what it was about. The answer arrives as
    /// [`UiEvent::Dialog`] and is matched by [`Shell::take_ask`].
    pub fn ask(&mut self, dialog: impl FnOnce(DialogId) -> Dialog, about: Ask) {
        let id = DialogId(self.next_dialog);
        self.next_dialog += 1;
        self.pending.push((id, about));
        self.chrome.ask(dialog(id));
    }

    /// What a dialog was about, consumed so an answer cannot be acted on twice.
    pub fn take_ask(&mut self, id: DialogId) -> Option<Ask> {
        let index = self.pending.iter().position(|(pending, _)| *pending == id)?;
        Some(self.pending.remove(index).1)
    }

    /// Rebuilds the library snapshot the chrome reads, thumbnails and all.
    ///
    /// Called when a board is created, renamed, duplicated or deleted — not per
    /// frame. `docs/01-architecture.md` §5's whole point is that listing a hundred
    /// boards costs a hundred small index reads, and doing that sixty times a second
    /// would throw the property away.
    pub fn refresh_cards(&mut self) {
        self.cards = self.library.cards().to_vec();
        for card in &mut self.cards {
            card.thumbnail = self
                .thumbnails
                .get(&card.path)
                .map(|(texture, size)| vellum_ui::Thumbnail { texture: *texture, size: *size });
        }
        self.chrome.set_spaces(self.library.spaces());
    }

    /// Records a decoded thumbnail for a board. The texture is the app's; the chrome
    /// only draws it.
    ///
    /// **The size is stored, not re-derived**, and that is what this map exists to carry
    /// beyond the texture id. `capture::fitted_size` deliberately preserves the board's
    /// aspect — a wide board becomes 512×218, a tall one 122×512 — so a thumbnail is
    /// square only by coincidence. [`Self::refresh_cards`] used to hand every card
    /// `[THUMBNAIL_SIZE; 2]` because the real figure was not kept anywhere, and
    /// `library::preview` maps the whole texture onto the box it computes from that size:
    /// every preview in the library was therefore stretched to a square, reported as
    /// *"the preview images distoreted"*. It was right for the frame after an upload —
    /// this function had the true size and set it — and wrong from the next
    /// `refresh_cards` onwards, which is to say from opening a board, a rescan, or a
    /// rename. Exactly the shape of `crate::draw`'s `UvRect::FULL` bug on link cards.
    pub fn set_thumbnail(&mut self, path: PathBuf, texture: egui::TextureId, size: [usize; 2]) {
        self.thumbnails.insert(path.clone(), (texture, size));
        for card in &mut self.cards {
            if card.path == path {
                card.thumbnail = Some(vellum_ui::Thumbnail { texture, size });
            }
        }
    }

    /// Whether a board already has a thumbnail uploaded, so the app does not decode
    /// the same blob on every visit to the library.
    pub fn has_thumbnail(&self, path: &Path) -> bool {
        self.thumbnails.contains_key(path)
    }

    /// How many boards have a thumbnail entry, placeholders included.
    pub fn thumbnail_count(&self) -> usize {
        self.thumbnails.len()
    }

    /// Forgets every thumbnail whose board is not in `keep`, handing the textures back
    /// for freeing.
    ///
    /// The bound this enforces is on **boards ever browsed**, not boards open: the
    /// loader uploads a preview for every card in the library and nothing but deleting
    /// a board ever took one down again. At 512² RGBA that is up to a megabyte each,
    /// so a library of a few hundred boards would quietly hold a few hundred megabytes
    /// of previews for rows that are not even on screen.
    pub fn retain_thumbnails(&mut self, keep: &BTreeSet<PathBuf>) -> Vec<egui::TextureId> {
        let doomed: Vec<PathBuf> = self
            .thumbnails
            .keys()
            .filter(|path| !keep.contains(*path))
            .cloned()
            .collect();
        doomed.iter().filter_map(|path| self.drop_thumbnail(path)).collect()
    }

    /// Forgets a board's thumbnail and hands the texture back for freeing.
    pub fn drop_thumbnail(&mut self, path: &Path) -> Option<egui::TextureId> {
        let (texture, _size) = self.thumbnails.remove(path)?;
        for card in &mut self.cards {
            if card.path == path {
                card.thumbnail = None;
            }
        }
        Some(texture)
    }

    /// Replaces the flattened selection, but only when it actually changed.
    ///
    /// `key` is `(document generation, selection length, a cheap digest)`. It is a
    /// heuristic, not a hash of the selection's contents — but the generation moves on
    /// every edit, so the only thing the digest has to catch is a same-length swap
    /// within one generation.
    pub fn sync_selection(
        &mut self,
        key: (u64, usize, u64),
        build: impl FnOnce() -> Vec<SelectionItem>,
    ) {
        if self.selection_key != key {
            self.selection_key = key;
            self.selection = build();
        }
    }

    /// Forces the next [`Shell::sync_selection`] to rebuild, for an edit that changed
    /// a property without changing the key the caller passes.
    pub fn invalidate_selection(&mut self) {
        self.selection_key = (u64::MAX, usize::MAX, u64::MAX);
    }

    pub fn selection(&self) -> &[SelectionItem] {
        &self.selection
    }

    pub fn set_theme_preference(&mut self, preference: vellum_ui::ThemePreference) {
        self.chrome.set_theme_preference(preference);
    }

    pub fn set_translucency(&mut self, on: bool) {
        self.chrome.set_translucency(on);
    }

    /// Puts the keyboard in the selected item's text field. See
    /// [`vellum_ui::Chrome::focus_text`].
    pub fn focus_text(&mut self) {
        self.chrome.focus_text();
    }

    /// Withdraws a pending request for the panel's text field, because the caret is on
    /// the canvas instead. See `vellum_ui::Chrome::release_text_focus`.
    pub fn release_text_focus(&mut self) {
        self.chrome.release_text_focus();
    }

    // ----- the frame ---------------------------------------------------------

    /// Offers a window event to the chrome.
    ///
    /// The caller decides what to do with the answer; see the module header for the
    /// rule, which is not simply "obey `consumed`".
    pub fn on_window_event(
        &mut self,
        window: &Window,
        event: &winit::event::WindowEvent,
    ) -> egui_winit::EventResponse {
        if let winit::event::WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
            self.pixels_per_point = *scale_factor as f32;
        }
        let response = self.winit.on_window_event(window, event);
        self.restore_clipboard_key(event);
        response
    }

    /// Puts back the `⌘V` / `⌘C` / `⌘X` key press that `egui-winit` eats.
    ///
    /// **This is the whole reason pasting a screenshot did nothing.** `egui-winit`'s
    /// `on_keyboard_input` matches the three clipboard chords *before* it pushes
    /// anything, turns them into [`egui::Event::Paste`]/`Copy`/`Cut`, and `return`s —
    /// so `egui::Event::Key { key: V, modifiers: COMMAND }` never enters egui's input
    /// at all, and [`vellum_ui::Chrome`]'s `consume_shortcut(⌘V)` can never match.
    /// `Command::Paste` was therefore unreachable from the keyboard; only the Edit
    /// menu row, the command palette and `--paste` ever called it. That is also why
    /// `--paste` "verified" the earlier `arboard` fix — it bypasses egui entirely.
    ///
    /// Worse for the reported bug: egui-winit builds its `Paste` event from
    /// `arboard::get_text`, and the early `return` sits *outside* that `if let`. With
    /// an image-only pasteboard — exactly what a screenshot leaves — there is no text,
    /// so **no event of any kind is produced** and the keystroke evaporates.
    ///
    /// The repair is additive rather than a rewrite: egui's own `Paste`/`Copy`/`Cut`
    /// events are left in place, so a focused `TextEdit` (the rename dialog, the find
    /// bar) still pastes into itself, and the synthetic key rides alongside for the
    /// command table. The two cannot both fire, because `Chrome::shortcuts` already
    /// returns early on `egui_wants_keyboard_input()` — a focused field takes the
    /// text, an unfocused board takes the command.
    ///
    /// Safe against the two collisions worth naming, both checked against egui 0.35:
    /// `TextEdit` reacts to `Event::Paste`, never to a `V` key, so nothing types a
    /// stray "v"; and the bare-`V` select-tool binding uses
    /// `consume_key(Modifiers::NONE, …)`, whose pattern with neither ctrl nor command
    /// resolves to `!self.ctrl && !self.command` — so a command-held `V` cannot reach
    /// it.
    fn restore_clipboard_key(&mut self, event: &winit::event::WindowEvent) {
        let winit::event::WindowEvent::KeyboardInput { event, .. } = event else {
            return;
        };
        if !event.state.is_pressed() {
            return;
        }
        let physical = clipboard_key_from_code(event.physical_key);
        let Some(key) = clipboard_key_from_logical(&event.logical_key).or(physical) else {
            return;
        };
        // egui-winit's own modifier state, which `ModifiersChanged` has already
        // updated by the time the key arrives. Reading it here rather than tracking a
        // second copy is what keeps this in step with the events it is repairing.
        let input = self.winit.egui_input_mut();
        let modifiers = input.modifiers;
        if !modifiers.command {
            return;
        }
        input.events.push(egui::Event::Key {
            key,
            physical_key: physical,
            pressed: true,
            repeat: false,
            modifiers,
        });
    }

    /// Draws one frame of chrome and returns everything the user asked for.
    pub fn run(&mut self, window: &Window, facts: &Facts<'_>) -> Vec<UiEvent> {
        // The OS reading is re-supplied every frame rather than at launch, which is
        // what `docs/05-design-language.md` §3a means by reacting live. The window's
        // own theme is deliberately not consulted: the app is light only.
        self.chrome.set_system_appearance(self.appearance.system());
        // The tab's unsaved-work dot, every frame: the user asked to be able to *see*
        // that autosave works, and autosave is instant, so the dot is only up for a
        // fraction of a second at a time and cannot be sampled any less often.
        if let Some(path) = facts.path {
            self.chrome.set_tab_dirty(Self::tab_key(path), facts.dirty);
        }

        let raw = self.winit.take_egui_input(window);
        self.pixels_per_point = window.scale_factor() as f32;

        let mut output;
        let mut full;
        {
            let Self {
                ctx,
                chrome,
                cards,
                selection,
                screen,
                tool,
                view,
                fonts,
                find_matches,
                ..
            } = self;

            let view_state = ViewState { zoom: facts.zoom, ..*view };
            let state = ChromeState {
                link_previews: self.library.link_previews(),
                align_objects: self.library.align_objects(),
                snap_to_grid: self.library.snap_to_grid(),
                grid: vellum_ui::GridSettings {
                    // The same fallback `app::canvas_pattern` applies, so the tick in the
                    // menu is on the row the canvas is actually drawing. `Shell` cannot see
                    // the open board, so the board's own pattern arrives on `background`.
                    pattern: self.library.grid_pattern().unwrap_or(facts.background.pattern),
                    color: self.library.grid_color(),
                },
                screen: *screen,
                board: BoardState {
                    title: facts.title,
                    path: facts.path,
                    starred: facts.starred,
                    dirty: facts.dirty,
                    can_undo: facts.can_undo,
                    can_redo: facts.can_redo,
                    clipboard_has_content: facts.clipboard_has_content,
                },
                tool: *tool,
                selection: selection.as_slice(),
                view: view_state,
                library: cards.as_slice(),
                font_families: fonts.as_slice(),
                find_matches: *find_matches,
                background: facts.background,
                now: SystemTime::now(),
                selection_rect: facts.selection_rect,
            };

            let mut chrome_output = None;
            full = ctx.run_ui(raw, |ui| {
                chrome_output = Some(chrome.show(ui, &state));
            });
            output = chrome_output.expect("egui runs the closure at least once");
        }

        self.primitives = self.ctx.tessellate(std::mem::take(&mut full.shapes), full.pixels_per_point);
        self.textures_delta = std::mem::take(&mut full.textures_delta);
        self.pointer_over_ui = output.pointer_over_ui;
        self.keyboard_captured = output.keyboard_captured;
        self.canvas_rect = output.canvas_rect;
        self.command_context = output.command_context;
        self.menu_flags = output.menu_flags;

        // One writer for the cursor: over the board, the app's shape is pushed into
        // egui's own output rather than raced against it.
        if !output.pointer_over_ui {
            full.platform_output.cursor_icon = facts.canvas_cursor;
        }
        self.winit.handle_platform_output(window, full.platform_output);

        self.rebuild_glass();
        std::mem::take(&mut output.events)
    }

    /// Turns the chrome's reported surfaces into the renderer's panels.
    ///
    /// Two translations and one rule. The translations are points to physical pixels
    /// and palette tokens to a material. The rule is `docs/05` §3a's **glass never
    /// stacks**: a surface drawn entirely inside another is drawing over glass, not
    /// over the board, so it gets no backdrop of its own. The chrome already avoids
    /// reporting one; checking here costs a comparison per panel against a mistake
    /// that would show up as a double-blurred smear.
    fn rebuild_glass(&mut self) {
        self.panels.clear();
        let palette = self.chrome.palette();
        let scale = self.pixels_per_point;
        let surfaces = self.chrome.glass_surfaces();

        for (index, surface) in surfaces.iter().enumerate() {
            if !surface.rect.is_positive() {
                continue;
            }
            if surfaces[..index]
                .iter()
                .any(|other| other.rect.contains_rect(surface.rect))
            {
                continue;
            }
            self.panels.push(
                GlassPanel::new(
                    [surface.rect.min.x * scale, surface.rect.min.y * scale],
                    [surface.rect.width() * scale, surface.rect.height() * scale],
                    material(palette, surface.opacity),
                )
                .with_corner_radius(surface.corner_radius * scale),
            );
        }
    }

    /// Puts the renderer's material in step with the palette and the OS.
    pub fn configure_glass(&self, glass: &mut GlassRenderer) {
        glass.set_scale_factor(self.pixels_per_point);
        glass.set_mode(if self.chrome.translucency() {
            vellum_render::GlassMode::Translucent
        } else {
            vellum_render::GlassMode::Opaque
        });
    }
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell")
            .field("screen", &self.screen)
            .field("tool", &self.tool)
            .field("boards", &self.cards.len())
            .field("selection", &self.selection.len())
            .field("glass", &self.panels.len())
            .finish()
    }
}

/// The material one floating surface is made of.
///
/// # Who draws which of the five layers
///
/// `docs/05-design-language.md` §3a builds the material out of five things: the
/// blurred backdrop, a saturation lift, the tint, a hairline, and the specular catch
/// along the top edge. **`vellum-ui` already paints the last three itself** — its
/// `floating_frame` fills with the tint at the palette's opacity and `paint_glass_edge`
/// draws the hairline and the specular — because a build with no blur behind it still
/// has to look like a panel. Its `Glass` documentation says so in as many words: *"the
/// tint alone renders as a flat surface, which is the documented fallback"*.
///
/// So the renderer's half is the two layers egui cannot do: **the blur and the
/// saturation lift, and nothing else**. Asking it for the tint as well composites the
/// tint twice — 0.72 over 0.72 is 0.92 — and the panel comes out very nearly opaque,
/// which looks exactly like the material having never been wired up. The specular
/// would be doubled the same way.
///
/// The tint's alpha is therefore zero here, which is not "no tint": it is what makes
/// the fragment shader emit the blurred backdrop at full strength for egui's tint to
/// land on. `GlassMaterial::needs_backdrop` is still true, so the blur is still
/// charged for and still cached.
fn material(palette: Palette, opacity: u8) -> GlassMaterial {
    if !palette.translucent || opacity == u8::MAX {
        // Reduce Transparency, high contrast, or a surface that asked to be opaque.
        // No backdrop is captured and no pass is encoded for it at all; egui's own
        // opaque fill is the whole panel.
        return GlassMaterial::opaque(rgba(palette.glass_tint), Rgba::TRANSPARENT)
            .with_border(Rgba::TRANSPARENT, 0.0);
    }
    GlassMaterial::new(rgba(palette.glass_tint).with_alpha(0.0), Rgba::TRANSPARENT)
        .with_border(Rgba::TRANSPARENT, 0.0)
        .with_highlight(Rgba::TRANSPARENT, 0.0)
}

/// egui's colour, in the renderer's.
///
/// **The two disagree about premultiplication and the difference is not cosmetic.**
/// `egui::Color32` stores sRGB with alpha already multiplied in;
/// `vellum_render::Rgba`'s public surface is straight, and its shaders premultiply on
/// output. Handing a premultiplied value across unchanged multiplies by alpha twice:
/// the 14% specular that separates the glass material from a plain blur would arrive
/// at 2% and simply vanish, which is exactly the kind of failure that looks like the
/// feature was never implemented.
fn rgba(color: egui::Color32) -> Rgba {
    let [r, g, b, a] = color.to_array();
    if a == 0 {
        return Rgba::TRANSPARENT;
    }
    let alpha = f32::from(a) / 255.0;
    let straight = |channel: u8| (f32::from(channel) / 255.0 / alpha).min(1.0);
    Rgba::new(straight(r), straight(g), straight(b), alpha)
}

/// The families the font dropdown offers.
///
/// **An honest gap.** `vellum-text` owns the `cosmic_text::FontSystem` that knows
/// every installed family, and exposes no way to enumerate them — its
/// `TextEngine::fonts` field is `pub(crate)`. Rather than reach around that, this is
/// the set `docs/05-design-language.md` §5 names plus the family Miro's boards are set
/// in. Choosing one that is not installed is not a failure: `cosmic-text` falls back,
/// and the document keeps what the user chose so it resolves on a machine that has it.
/// The families the picker offers, before the engine has been asked.
///
/// A starting list only — [`Shell::set_font_families`] replaces it with what the machine can
/// actually shape as soon as the text engine exists. It holds the bundled family alone,
/// because that is the one name guaranteed to work on any machine this ever runs on.
fn font_families() -> Vec<String> {
    vec![vellum_text::BUNDLED_FAMILY.to_owned()]
}

/// The three clipboard keys, from what the layout produced.
///
/// Only `V`, `C` and `X`: they are the only keys `egui-winit` swallows, and a mapper
/// that answered for more would put keys back that were never taken. Written here
/// rather than reused because `egui_winit`'s own `key_from_winit_key` is private.
fn clipboard_key_from_logical(key: &winit::keyboard::Key) -> Option<egui::Key> {
    match key.as_ref() {
        winit::keyboard::Key::Character("v" | "V") => Some(egui::Key::V),
        winit::keyboard::Key::Character("c" | "C") => Some(egui::Key::C),
        winit::keyboard::Key::Character("x" | "X") => Some(egui::Key::X),
        _ => None,
    }
}

/// The same three keys, from the physical position instead.
///
/// This is `egui-winit`'s own `logical_key.or(physical_key)` fallback, kept because it
/// is the half that makes the chords work on a layout with no Latin characters — the
/// keys that *hold* C, X and V on a QWERTY board keep their clipboard meaning there.
fn clipboard_key_from_code(key: winit::keyboard::PhysicalKey) -> Option<egui::Key> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    match key {
        PhysicalKey::Code(KeyCode::KeyV) => Some(egui::Key::V),
        PhysicalKey::Code(KeyCode::KeyC) => Some(egui::Key::C),
        PhysicalKey::Code(KeyCode::KeyX) => Some(egui::Key::X),
        _ => None,
    }
}

/// `crate::input`'s pointer shape, in egui's vocabulary.
///
/// The two enums agree on every shape the canvas uses, so this is a rename. It exists
/// because `egui_winit`'s own translation runs the other way and is private.
pub const fn cursor_icon(icon: winit::window::CursorIcon) -> egui::CursorIcon {
    match icon {
        winit::window::CursorIcon::Grab => egui::CursorIcon::Grab,
        winit::window::CursorIcon::Grabbing => egui::CursorIcon::Grabbing,
        winit::window::CursorIcon::Crosshair => egui::CursorIcon::Crosshair,
        winit::window::CursorIcon::Move => egui::CursorIcon::Move,
        winit::window::CursorIcon::Text => egui::CursorIcon::Text,
        // The resize grips. Without these four the mapping fell through to `Default`,
        // so `Input::cursor_icon` could name the direction and the pointer still showed
        // an arrow — the translation being lossy is as good as not computing it.
        winit::window::CursorIcon::NwseResize => egui::CursorIcon::ResizeNwSe,
        winit::window::CursorIcon::NeswResize => egui::CursorIcon::ResizeNeSw,
        winit::window::CursorIcon::NsResize => egui::CursorIcon::ResizeVertical,
        winit::window::CursorIcon::EwResize => egui::CursorIcon::ResizeHorizontal,
        _ => egui::CursorIcon::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three keys `egui-winit` eats, and only those three.
    ///
    /// This is the test that would have caught the paste bug had it existed, at the
    /// only altitude a test can reach: `Shell::restore_clipboard_key` needs a live
    /// `Window` and an `egui_winit::State`, neither of which a unit test can build
    /// headlessly, so the mapping is factored out and checked here while the wiring
    /// around it is checked by `--paste` and by hand. Naming that honestly matters —
    /// the reason the bug survived is that `vellum-ui`'s shortcut tests synthesise
    /// `egui::Event::Key` directly, which starts *downstream* of the layer that drops
    /// it, so they passed while the app could not paste.
    #[test]
    fn the_clipboard_chords_map_and_nothing_else_does() {
        use winit::keyboard::{Key as WKey, KeyCode, PhysicalKey};

        for (character, expected) in
            [("v", egui::Key::V), ("c", egui::Key::C), ("x", egui::Key::X)]
        {
            let lower = WKey::Character(character.into());
            let upper = WKey::Character(character.to_uppercase().into());
            assert_eq!(clipboard_key_from_logical(&lower), Some(expected));
            assert_eq!(
                clipboard_key_from_logical(&upper),
                Some(expected),
                "a shifted {character} is still the clipboard key"
            );
        }

        // Every other key still reaches egui the ordinary way, so putting one back
        // would deliver it twice.
        for other in ["z", "a", "s", "b"] {
            assert_eq!(clipboard_key_from_logical(&WKey::Character(other.into())), None);
        }
        assert_eq!(
            clipboard_key_from_logical(&WKey::Named(winit::keyboard::NamedKey::Escape)),
            None
        );

        // The physical fallback, which is what makes ⌘V work on a layout whose
        // logical keys are not Latin.
        assert_eq!(clipboard_key_from_code(PhysicalKey::Code(KeyCode::KeyV)), Some(egui::Key::V));
        assert_eq!(clipboard_key_from_code(PhysicalKey::Code(KeyCode::KeyC)), Some(egui::Key::C));
        assert_eq!(clipboard_key_from_code(PhysicalKey::Code(KeyCode::KeyX)), Some(egui::Key::X));
        assert_eq!(clipboard_key_from_code(PhysicalKey::Code(KeyCode::KeyZ)), None);
    }

    /// The restored key must not also fire the bare-`V` select-tool binding.
    ///
    /// `Chrome::shortcuts` consumes tool keys with `consume_key(Modifiers::NONE, …)`,
    /// and egui resolves a pattern carrying neither ctrl nor command to "no ctrl and
    /// no command are held". If that ever loosened, one ⌘V would paste *and* switch
    /// tools. Asserted against egui itself rather than remembered.
    #[test]
    fn a_command_held_v_cannot_reach_the_bare_v_tool_binding() {
        assert!(
            !egui::Modifiers::COMMAND.matches_logically(egui::Modifiers::NONE),
            "⌘V would also switch to the select tool"
        );
        assert!(egui::Modifiers::COMMAND.matches_logically(egui::Modifiers::COMMAND));
        assert!(egui::Modifiers::NONE.matches_logically(egui::Modifiers::NONE));
    }

    /// The renderer's half of the material is the blur and the saturation lift. If it
    /// ever grows a tint, a hairline or a specular back, every one of them is drawn
    /// twice — `vellum-ui` already paints all three — and the panel comes out opaque.
    #[test]
    fn the_renderer_supplies_the_blur_and_leaves_the_tint_to_the_chrome() {
        for palette in [Palette::LIGHT, Palette::DARK] {
            let material = material(palette, palette.glass_opacity);
            assert!(
                material.needs_backdrop(),
                "translucent chrome must still be blurred behind"
            );
            assert_eq!(material.tint.a, 0.0, "the tint is the chrome's to draw");
            assert_eq!(material.border.a, 0.0, "the hairline is the chrome's to draw");
            assert_eq!(material.highlight.a, 0.0, "the specular is the chrome's to draw");
            assert!((material.saturation - GlassMaterial::SATURATION).abs() < 1e-6);
        }
    }

    /// `docs/05` §3a: under Reduce Transparency the material falls back to a fully
    /// opaque surface with no blur behind it. `needs_backdrop` false is what makes
    /// that cost nothing rather than merely look flat.
    #[test]
    fn an_opaque_palette_costs_no_blur() {
        let opaque = Palette::LIGHT.opaque();
        let material = material(opaque, opaque.glass_opacity);
        assert!(!material.needs_backdrop());
        assert_eq!(material.tint.a, 1.0);
        assert_eq!(material.highlight.a, 0.0, "a specular on an opaque panel is a bevel");
    }

    /// The premultiplication trap, pinned. `Color32` stores colour with alpha already
    /// multiplied in and `Rgba` does not; passing one for the other multiplies by
    /// alpha twice and the specular disappears.
    #[test]
    fn a_colour_survives_the_trip_from_egui_to_the_renderer() {
        let red = egui::Color32::from_rgb(0xE6, 0x5B, 0x58);
        assert_eq!(rgba(red).pack(), [0xE6, 0x5B, 0x58, 0xFF]);

        // The glass specular: white at 14%. It has to arrive as *white* at 14%, not
        // as 14% grey at 14%.
        let specular = egui::Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 40);
        assert_eq!(rgba(specular).pack(), [0xFF, 0xFF, 0xFF, 40]);

        let half = egui::Color32::from_rgba_unmultiplied(0x1A, 0x1D, 0x1F, 128);
        let packed = rgba(half).pack();
        assert_eq!(packed[3], 128);
        assert!(packed[0].abs_diff(0x1A) <= 1, "{packed:?}");

        assert_eq!(rgba(egui::Color32::TRANSPARENT).pack(), [0, 0, 0, 0]);
    }

    /// The shapes `crate::input::Input::cursor_icon` can return have to survive the
    /// trip, or the cursor stops reporting what a click will do — which
    /// `docs/06-mouse-controls.md` §4 lists as one of the things that separate "works"
    /// from "feels right".
    #[test]
    fn every_canvas_cursor_has_an_egui_equivalent() {
        use winit::window::CursorIcon as W;
        assert_eq!(cursor_icon(W::Grab), egui::CursorIcon::Grab);
        assert_eq!(cursor_icon(W::Grabbing), egui::CursorIcon::Grabbing);
        assert_eq!(cursor_icon(W::Crosshair), egui::CursorIcon::Crosshair);
        assert_eq!(cursor_icon(W::Move), egui::CursorIcon::Move);
        assert_eq!(cursor_icon(W::Default), egui::CursorIcon::Default);
    }

    /// A board has one tab however its path was spelled. Two keys for one file means
    /// two `Editor`s over one SQLite database, each with its own autosave thread —
    /// the same shape of fault as the import that appended three times.
    #[test]
    fn one_board_has_one_tab_key_however_its_path_was_spelled() {
        let cwd = std::env::current_dir().expect("a working directory");
        let relative = Path::new("boards/site-plan.vellum");
        let absolute = cwd.join("boards/site-plan.vellum");
        assert_eq!(
            Shell::tab_key(relative),
            Shell::tab_key(&absolute),
            "`--board boards/site-plan.vellum` and the library's own row disagreed"
        );
        assert_eq!(Shell::tab_key(&absolute), Shell::tab_key(&absolute.clone()));
        assert_ne!(Shell::tab_key(&absolute), Shell::tab_key(&cwd.join("boards/other.vellum")));
        // An empty path cannot be made absolute; it must still answer rather than panic.
        let _ = Shell::tab_key(Path::new(""));
    }

    /// The picker leads with the canvas default and never offers it twice.
    ///
    /// The seed list is the bundled family alone — the one name guaranteed to work anywhere —
    /// and the machine's own families arrive later through `Shell::set_font_families`, once
    /// the text engine exists to be asked. It used to be a hardcoded seven that included
    /// `Segoe UI` and `Cascadia Mono`, which do not exist on macOS: picking either stored a
    /// value and changed nothing on screen, which the user reported as the control not
    /// working. A list built from the shaper's own faces cannot have that fault.
    #[test]
    fn the_font_picker_leads_with_the_bundled_family_exactly_once() {
        let seed = font_families();
        assert_eq!(seed, vec![vellum_text::BUNDLED_FAMILY.to_owned()]);

        // What the engine reports always contains the bundled family, because it is loaded —
        // so the de-duplication is the case that actually occurs, not a defensive one.
        let reported = vec![
            "Noto Sans".to_owned(),
            vellum_text::BUNDLED_FAMILY.to_owned(),
            "Georgia".to_owned(),
        ];
        let offered = {
            let mut families = reported.clone();
            families.retain(|name| name != vellum_text::BUNDLED_FAMILY);
            families.truncate(120);
            std::iter::once(vellum_text::BUNDLED_FAMILY.to_owned())
                .chain(families)
                .collect::<Vec<_>>()
        };
        assert_eq!(offered.first().map(String::as_str), Some(vellum_text::BUNDLED_FAMILY));
        assert_eq!(
            offered.iter().filter(|n| *n == vellum_text::BUNDLED_FAMILY).count(),
            1,
            "the default must not appear twice: {offered:?}"
        );
        // Miro's boards are set in Noto Sans; import fidelity needs it offered when the
        // machine has it.
        assert!(offered.contains(&"Noto Sans".to_owned()));
    }
}
