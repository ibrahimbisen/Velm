//! The **native** menu bar: macOS's own, along the top of the screen.
//!
//! *"i also want you to add these to velm as well"*, with four screenshots of Miro's
//! desktop menus — File, Edit, View, Window. That is a different object from the in-app
//! `⋮` menu `vellum_ui::menu` draws, which is Miro's *web* chrome; the desktop app has
//! both, and so does Velm now.
//!
//! # Built from the same command table
//!
//! Every custom row is a [`Command`], carries [`Command::label`] and
//! [`Command::shortcut`], and asks [`Command::availability`] whether it may act — so the
//! native bar cannot drift from the in-app one, and a command added to
//! `vellum_ui::command` appears here by editing one list rather than two.
//!
//! # The rows that deliberately carry no key equivalent
//!
//! **A custom row's accelerator is claimed by `NSMenu` at `performKeyEquivalent:`,
//! before the key reaches the window at all.** Put `⌘C` on one and it stops arriving at
//! `winit` — taking the canvas caret's copy, `egui`'s `TextEdit` copy, and the clipboard
//! path that cost four rounds to get right (CLAUDE.md trap 9) with it. Put a bare
//! `Delete` on one and the Delete key stops working *inside a word*.
//!
//! So [`accelerator`] refuses two families, and [`tests`] pins both:
//!
//! - **Cut, Copy, Paste, Select all, Undo, Redo** — whatever has the keyboard owns
//!   these, and in this app that is `egui`, not the responder chain.
//! - **Anything with no ⌘ or ⌃** — a bare key belongs to whatever is being typed into.
//!   `Command::Delete` is the only one today.
//!
//! Those rows are still *rows*: they are there, they are enabled, and clicking one does
//! exactly what the keyboard does, because [`Chosen`] goes to the same
//! `crate::actions::ActiveState::run`. What they do not show is the shortcut text beside
//! the label — which is a real cosmetic cost, taken knowingly. `PredefinedMenuItem`
//! would show it, but those are targeted at `nil` and walk the responder chain: nothing
//! in `winit`'s view implements `copy:`, so the row would grey itself out and clicking
//! it would do nothing at all. A row that is honest and works beats one that looks right
//! and does not.
//!
//! # macOS only, for now
//!
//! `muda` supports Windows too, but the Windows menu bar lives inside the window and
//! takes a band off the top of it — which is a layout change `vellum_ui::chrome` would
//! have to know about, and a second thing to verify on a machine that cannot run it.
//! Gated rather than half-built.

#![cfg(target_os = "macos")]

use muda::accelerator::{Accelerator, Code, Modifiers as Mods};
use muda::{
    AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use vellum_ui::{Command, CommandContext, MenuFlags};

/// A row that is not a [`Command`] — the platform's own verbs, and the two tab moves the
/// app owns but the command table does not list.
///
/// Kept as its own enum rather than folded into `Command` because none of these belong
/// in the in-app menu: `vellum_ui` has no window to minimise and no notion of a native
/// clipboard responder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extra {
    /// File ▸ Select next / previous tab. `⌘⌥→` / `⌘⌥←`, which is what the app already
    /// binds — see CLAUDE.md feedback 4 for why not `⌘⇧[`.
    NextTab,
    PreviousTab,
    /// File ▸ New tab.
    NewTab,
}

/// Every one of these is a row, and none of them gets a key equivalent. See the module
/// note: `NSMenu` would claim the key before `winit` saw it, and in this app these six
/// belong to whatever has the keyboard.
const KEYBOARD_OWNS: [Command; 6] = [
    Command::Cut,
    Command::Copy,
    Command::Paste,
    Command::SelectAll,
    Command::Undo,
    Command::Redo,
];

/// What a menu click asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chosen {
    Command(Command),
    Extra(Extra),
}

/// The bar, and everything it has to be able to grey out later.
pub struct MenuBar {
    /// The root. Dropping it would tear the bar down, so it is held for the process.
    _root: Menu,
    /// Every row that can be disabled, beside the command that decides it.
    rows: Vec<(Command, MenuItem)>,
    /// Every row that shows a tick.
    ticks: Vec<(Command, CheckMenuItem)>,
    /// What the rows were last set to, so an idle frame costs no Objective-C at all.
    /// A menu bar is re-synced sixty times a second otherwise, and each row is a
    /// separate message send across the language boundary.
    last: Option<(CommandContext, MenuFlags)>,
}

impl MenuBar {
    /// Builds the bar and installs it. **Main thread only** — `NSMenu` requires it, and
    /// so does `muda`.
    ///
    /// Returns `None` when the platform refuses, which is not worth failing a launch
    /// over: an app with no menu bar still has every one of these verbs on the keyboard
    /// and in the in-app menu.
    pub fn install() -> Option<Self> {
        let root = Menu::new();
        let mut bar = Self { _root: root, rows: Vec::new(), ticks: Vec::new(), last: None };
        let built = match bar.build() {
            Ok(built) => built,
            Err(error) => {
                log::warn!("native menu bar: {error}");
                return None;
            }
        };
        bar._root.init_for_nsapp();
        // **After** `init_for_nsapp`, which `muda` documents and which is not a
        // formality: both of these hand a submenu to `NSApplication`, and it has
        // nothing to hand them to until the bar is the app's own.
        built.window.set_as_windows_menu_for_nsapp();
        built.help.set_as_help_menu_for_nsapp();
        Some(bar)
    }

    fn build(&mut self) -> muda::Result<Built> {
        use Command as C;

        // The application menu. macOS takes its title from the bundle, and *About*,
        // *Services*, *Hide* and *Quit* are the platform's own — an app that hand-rolls
        // them gets them subtly wrong and loses the Services integration entirely.
        let about = AboutMetadata {
            name: Some("Velm".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            comments: Some("A native infinite canvas.".into()),
            ..AboutMetadata::default()
        };
        let app = Submenu::new("Velm", true);
        app.append_items(&[
            &PredefinedMenuItem::about(Some("About Velm"), Some(about)),
            &PredefinedMenuItem::separator(),
            // ⚠ **The first custom row in this submenu.** Everything else here is a
            // `PredefinedMenuItem`, so this is a genuinely new kind of entry rather than one
            // more of the same, and it goes straight after About because that is where macOS
            // puts Settings in every application. `accelerator` gives it ⌘, — `code_for`
            // already maps `Key::Comma`, and a shortcut carrying ⌘ passes both of that
            // function's refusals.
            //
            // Verify this by screenshot rather than by reasoning: whether `muda` draws a
            // custom row correctly in the application submenu is not something the type
            // system has an opinion about.
            &self.row(C::OpenSettings),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(Some("Hide Velm")),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(Some("Quit Velm")),
        ])?;

        let file = Submenu::new("File", true);
        let export = Submenu::new("Export", true);
        export.append_items(&[
            &self.row(C::ExportPng),
            &self.row(C::ExportPdf),
            &self.row(C::ExportSvg),
            &self.row(C::ExportCsv),
            &PredefinedMenuItem::separator(),
            &self.row(C::ExportBackup),
        ])?;
        file.append_items(&[
            &self.row(C::NewBoard),
            &extra(Extra::NewTab, "New tab", accel(Mods::SUPER, Code::KeyT)),
            &self.row(C::OpenBoard),
            &self.row(C::ImportFromMiro),
            &PredefinedMenuItem::separator(),
            &self.row(C::Save),
            &self.row(C::SaveAs),
            &export,
            &PredefinedMenuItem::separator(),
            &self.row(C::CloseBoard),
            // Miro's own File menu has these two, and the app already binds them —
            // CLAUDE.md feedback 4 records why they are `⌘⌥←/→` and not `⌘⇧[`/`⌘⇧]`,
            // which Miro spends on *Send to back* / *Bring to front*.
            &extra(
                Extra::NextTab,
                "Select next tab",
                accel(Mods::SUPER | Mods::ALT, Code::ArrowRight),
            ),
            &extra(
                Extra::PreviousTab,
                "Select previous tab",
                accel(Mods::SUPER | Mods::ALT, Code::ArrowLeft),
            ),
        ])?;

        // Everything in the first two bands is predefined. See the module note: a key
        // equivalent on a custom row is claimed by `NSMenu` before the window sees it,
        // and these six keys belong to whatever is being typed into.
        let edit = Submenu::new("Edit", true);
        let arrange = Submenu::new("Arrange", true);
        arrange.append_items(&[
            &self.row(C::BringToFront),
            &self.row(C::BringForward),
            &self.row(C::SendBackward),
            &self.row(C::SendToBack),
            &PredefinedMenuItem::separator(),
            &self.row(C::Group),
            &self.row(C::Ungroup),
            &PredefinedMenuItem::separator(),
            &self.row(C::Lock),
            &self.row(C::Unlock),
            &PredefinedMenuItem::separator(),
            &self.row(C::AlignLeft),
            &self.row(C::AlignCenterHorizontal),
            &self.row(C::AlignRight),
            &self.row(C::AlignTop),
            &self.row(C::AlignMiddleVertical),
            &self.row(C::AlignBottom),
            &PredefinedMenuItem::separator(),
            &self.row(C::DistributeHorizontally),
            &self.row(C::DistributeVertically),
        ])?;
        edit.append_items(&[
            &self.row(C::Undo),
            &self.row(C::Redo),
            &PredefinedMenuItem::separator(),
            &self.row(C::Cut),
            &self.row(C::Copy),
            &self.row(C::Paste),
            &self.row(C::Duplicate),
            &self.row(C::Delete),
            &PredefinedMenuItem::separator(),
            &self.row(C::SelectAll),
            &PredefinedMenuItem::separator(),
            &arrange,
            &PredefinedMenuItem::separator(),
            &self.row(C::Find),
            &self.row(C::CommandPalette),
        ])?;

        let view = Submenu::new("View", true);
        view.append_items(&[
            &self.row(C::ZoomIn),
            &self.row(C::ZoomOut),
            &self.row(C::ZoomToFit),
            &self.row(C::ZoomToSelection),
            &self.row(C::ZoomActualSize),
            &PredefinedMenuItem::separator(),
            // Snap to grid rather than a grid show/hide: which pattern the board wears is
            // the board's own choice and lives in the in-app View ▸ Grid submenu, which
            // `muda` has no equivalent for — a native submenu of colour swatches and a
            // slider is not a menu row. What *is* a row is the toggle.
            &self.tick(C::SnapToGrid),
            &self.tick(C::ToggleMinimap),
            &self.tick(C::TogglePropertiesPanel),
            &PredefinedMenuItem::separator(),
            &self.row(C::GoToStartView),
            &self.row(C::SetStartView),
            &PredefinedMenuItem::separator(),
            &self.tick(C::PresentationMode),
            &PredefinedMenuItem::separator(),
            // The platform's, so it goes green-button full screen rather than Velm's
            // own presentation mode. The two are genuinely different and Miro's View
            // menu lists both.
            &PredefinedMenuItem::fullscreen(None),
        ])?;

        // Entirely the platform's. AppKit fills in the window list, *Fill*, *Center*,
        // *Move & Resize* and *Full Screen Tile* itself once this is the windows menu —
        // which is why Miro's own Window menu in the user's screenshot has rows Miro
        // never wrote.
        let window = Submenu::new("Window", true);
        window.append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(Some("Zoom")),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::bring_all_to_front(None),
        ])?;

        let help = Submenu::new("Help", true);
        help.append_items(&[
            &self.row(C::KeyboardShortcuts),
            &self.row(C::Documentation),
            &PredefinedMenuItem::separator(),
            &self.row(C::About),
        ])?;

        self._root.append_items(&[&app, &file, &edit, &view, &window, &help])?;
        Ok(Built { window, help })
    }

    /// One ordinary row, remembered so it can be greyed out later.
    fn row(&mut self, command: Command) -> MenuItem {
        let item = MenuItem::with_id(id_of(command), command.label(), true, accelerator(command));
        self.rows.push((command, item.clone()));
        item
    }

    /// One row that shows a tick.
    fn tick(&mut self, command: Command) -> CheckMenuItem {
        let item = CheckMenuItem::with_id(
            id_of(command),
            command.label(),
            true,
            false,
            accelerator(command),
        );
        self.ticks.push((command, item.clone()));
        item
    }

    /// Greys out what cannot act and ticks what is on.
    ///
    /// Called every frame and does nothing on almost all of them: each row is a separate
    /// message send across the language boundary, and a menu bar re-synced at 60Hz is a
    /// few thousand of those a second to change nothing.
    pub fn sync(&mut self, ctx: &CommandContext, flags: MenuFlags) {
        if self.last == Some((*ctx, flags)) {
            return;
        }
        self.last = Some((*ctx, flags));
        for (command, item) in &self.rows {
            item.set_enabled(command.is_enabled(ctx));
        }
        for (command, item) in &self.ticks {
            item.set_enabled(command.is_enabled(ctx));
            item.set_checked(is_on(*command, flags));
        }
    }

    /// Everything clicked since the last call. Never blocks.
    ///
    /// Drained from `muda`'s process-wide channel rather than from a field, because the
    /// menu fires on AppKit's own thread and `muda` is the thing that owns the bridge.
    pub fn drain() -> Vec<Chosen> {
        let mut out = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match resolve(&event.id) {
                Some(chosen) => out.push(chosen),
                None => log::debug!("native menu bar: unmapped id {:?}", event.id),
            }
        }
        out
    }
}

/// The two submenus `NSApplication` has to be told about by hand once the bar is
/// installed. Everything else it works out from the order they were appended in.
struct Built {
    window: Submenu,
    help: Submenu,
}

/// A row that is not a command.
fn extra(which: Extra, label: &str, accelerator: Option<Accelerator>) -> MenuItem {
    MenuItem::with_id(extra_id(which), label, true, accelerator)
}

/// The id a command's row carries.
///
/// Its `Debug` name, not its index in [`Command::ALL`]: an index is silently wrong the
/// moment a variant is inserted, and the failure — one menu row running a different
/// command — is exactly the kind nothing would catch.
fn id_of(command: Command) -> String {
    format!("cmd:{command:?}")
}

fn extra_id(which: Extra) -> String {
    format!("extra:{which:?}")
}

/// The reverse, by scanning the tables rather than by parsing — so the two directions
/// cannot disagree about a name.
fn resolve(id: &MenuId) -> Option<Chosen> {
    let text = id.as_ref();
    if let Some(name) = text.strip_prefix("cmd:") {
        return Command::ALL
            .iter()
            .find(|c| format!("{c:?}") == name)
            .map(|c| Chosen::Command(*c));
    }
    let name = text.strip_prefix("extra:")?;
    [Extra::NextTab, Extra::PreviousTab, Extra::NewTab]
        .into_iter()
        .find(|e| format!("{e:?}") == name)
        .map(Chosen::Extra)
}

fn accel(mods: Mods, code: Code) -> Option<Accelerator> {
    Some(Accelerator::new(Some(mods), code))
}

/// A command's key equivalent, translated from the one the in-app menu shows.
///
/// `None` in three cases, and the first two are the whole safety argument of this module:
///
/// 1. **A verb the keyboard owns** — see [`KEYBOARD_OWNS`].
/// 2. **A shortcut with no ⌘ and no ⌃.** A bare key belongs to whatever is being typed
///    into; a native `Delete` key equivalent would eat backspace inside a word, in every
///    text field in the app, from the moment the bar was installed.
/// 3. A key with no `keyboard_types::Code`, which is a missing row in [`code_for`] rather
///    than a reason to fail — and a failing test rather than a silent gap.
fn accelerator(command: Command) -> Option<Accelerator> {
    if KEYBOARD_OWNS.contains(&command) {
        return None;
    }
    let shortcut = command.shortcut()?;
    if !(shortcut.modifiers.command || shortcut.modifiers.ctrl) {
        return None;
    }
    let mut mods = Mods::empty();
    if shortcut.modifiers.command {
        mods |= Mods::SUPER;
    }
    if shortcut.modifiers.shift {
        mods |= Mods::SHIFT;
    }
    if shortcut.modifiers.alt {
        mods |= Mods::ALT;
    }
    if shortcut.modifiers.ctrl {
        mods |= Mods::CONTROL;
    }
    Some(Accelerator::new(Some(mods), code_for(shortcut.logical_key)?))
}

/// `egui::Key` to `keyboard_types::Code`, for the keys the command table actually uses.
///
/// Exhaustive over that set and no wider: a `_ => None` arm over the whole of
/// `egui::Key` would silently drop a shortcut added later, and `tests::every_shortcut_in_
/// the_command_table_translates` is what makes the omission a failing test instead.
fn code_for(key: egui::Key) -> Option<Code> {
    use egui::Key as K;
    Some(match key {
        K::A => Code::KeyA,
        K::C => Code::KeyC,
        K::D => Code::KeyD,
        K::F => Code::KeyF,
        K::G => Code::KeyG,
        K::K => Code::KeyK,
        K::L => Code::KeyL,
        K::M => Code::KeyM,
        K::N => Code::KeyN,
        K::O => Code::KeyO,
        K::P => Code::KeyP,
        K::S => Code::KeyS,
        K::T => Code::KeyT,
        K::U => Code::KeyU,
        K::V => Code::KeyV,
        K::W => Code::KeyW,
        K::X => Code::KeyX,
        K::Y => Code::KeyY,
        K::Z => Code::KeyZ,
        K::Num0 => Code::Digit0,
        K::Num1 => Code::Digit1,
        K::Num2 => Code::Digit2,
        K::Plus | K::Equals => Code::Equal,
        K::Minus => Code::Minus,
        K::OpenBracket => Code::BracketLeft,
        K::CloseBracket => Code::BracketRight,
        K::Delete => Code::Delete,
        K::Backspace => Code::Backspace,
        K::ArrowLeft => Code::ArrowLeft,
        K::ArrowRight => Code::ArrowRight,
        K::Comma => Code::Comma,
        _ => return None,
    })
}

/// Whether a toggle is currently on, from the same flags the in-app menu ticks from.
const fn is_on(command: Command, flags: MenuFlags) -> bool {
    match command {
        Command::SnapToGrid => flags.snap_to_grid,
        Command::ToggleMinimap => flags.minimap_visible,
        Command::TogglePropertiesPanel => flags.properties_panel,
        Command::PresentationMode => flags.presenting,
        Command::StarBoard => flags.starred,
        Command::ToggleTranslucency => flags.translucent,
        Command::ToggleLinkPreviews => flags.link_previews,
        Command::ToggleAlignObjects => flags.align_objects,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row's id has to survive the round trip, or a click runs a different command
    /// than the one that was drawn. The name-based scheme exists precisely so that
    /// inserting a variant into `Command::ALL` cannot shift what a row does.
    #[test]
    fn every_command_id_resolves_back_to_itself() {
        for command in Command::ALL {
            let id = MenuId::new(id_of(*command));
            assert_eq!(
                resolve(&id),
                Some(Chosen::Command(*command)),
                "{command:?} did not survive the round trip"
            );
        }
    }

    #[test]
    fn every_extra_id_resolves_back_to_itself() {
        for extra in [Extra::NextTab, Extra::PreviousTab, Extra::NewTab] {
            let id = MenuId::new(extra_id(extra));
            assert_eq!(resolve(&id), Some(Chosen::Extra(extra)));
        }
    }

    /// An unknown id is reported, not guessed at. A menu bar that runs *something*
    /// when it cannot read its own row is worse than one that logs and does nothing.
    #[test]
    fn an_unknown_id_resolves_to_nothing() {
        for junk in ["", "cmd:", "cmd:NotACommand", "extra:Nope", "Copy", "cmd:copy"] {
            assert_eq!(resolve(&MenuId::new(junk)), None, "{junk:?} resolved to something");
        }
    }

    /// Every shortcut the in-app menu shows must be expressible as a native key
    /// equivalent, or the native row silently loses it while the in-app row keeps it —
    /// two menus claiming to offer the same command on different keys.
    #[test]
    fn every_shortcut_in_the_command_table_translates() {
        for command in Command::ALL {
            let Some(shortcut) = command.shortcut() else { continue };
            assert!(
                code_for(shortcut.logical_key).is_some(),
                "{command:?} is bound to {:?}, which has no native code",
                shortcut.logical_key
            );
        }
    }

    /// **The load-bearing test of this module.** A key equivalent on a native row is
    /// claimed before `winit` sees the key, so any row that takes one has taken that key
    /// away from every text field and from the canvas caret. Two families must never
    /// have one, and the cost of getting it wrong is a feature the user has already
    /// reported broken three times.
    #[test]
    fn no_native_row_steals_a_key_the_keyboard_owns() {
        for command in KEYBOARD_OWNS {
            assert!(
                accelerator(command).is_none(),
                "{command:?} would claim its key before the window saw it"
            );
        }
        for command in Command::ALL {
            let Some(shortcut) = command.shortcut() else { continue };
            if shortcut.modifiers.command || shortcut.modifiers.ctrl {
                continue;
            }
            assert!(
                accelerator(*command).is_none(),
                "{command:?} is bound to a bare {:?}, which typing needs",
                shortcut.logical_key
            );
        }
    }

    /// …and the other direction, so the rule cannot be "refuse everything". A menu bar
    /// whose rows all lost their shortcuts would pass the test above perfectly.
    #[test]
    fn the_ordinary_rows_keep_their_shortcuts() {
        for command in [
            Command::NewBoard,
            Command::Save,
            Command::Find,
            Command::ZoomToFit,
            Command::TogglePropertiesPanel,
            Command::BringToFront,
        ] {
            assert!(accelerator(command).is_some(), "{command:?} lost its shortcut");
        }
    }

    /// The tick table has to agree with the command table about which rows are toggles,
    /// exactly as `vellum_ui::MenuFlags::is_on` does — the native bar drawing a tick on
    /// a row that is not a toggle, or missing one that is, is a menu that lies.
    #[test]
    fn the_tick_table_covers_every_toggle_and_nothing_else() {
        let all_on = MenuFlags {
            snap_to_grid: true,
            minimap_visible: true,
            properties_panel: true,
            presenting: true,
            starred: true,
            translucent: true,
            link_previews: true,
            align_objects: true,
        };
        for command in Command::ALL {
            assert_eq!(
                is_on(*command, all_on),
                command.is_toggle(),
                "{command:?} disagrees with its toggle flag"
            );
        }
    }
}
