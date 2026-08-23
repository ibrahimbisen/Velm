//! The board library — the screen the app opens on.
//!
//! `vellum-store` already answers this screen's questions without loading a single
//! document: title, item count, thumbnail hash and mtime come out of one small row
//! per board. The chrome mirrors that discipline by taking [`BoardCard`]s as plain
//! data — no database handle, no `SystemTime` arithmetic beyond formatting — so a
//! hundred boards cost a hundred structs and nothing else.
//!
//! # Shape
//!
//! `docs/04-ui-reference.md` §5 transcribes the reference start screen, and three
//! things from it are structural rather than decorative:
//!
//! - **[`Space`] is first class.** Folders — Personal, Research, Design, Archive,
//!   Planning, Posters — are how people find a board, not a nice-to-have.
//!   A space is a named set of paths rather than a field on a board, so a board can
//!   be listed without the library knowing anything about folders and the app can
//!   supply the grouping from whatever it likes. Spaces can be **created, renamed,
//!   pinned, deleted, and filled** from this screen; a sidebar that could only *show*
//!   them would be a picture of the feature rather than the feature.
//! - **Starring.** §5 puts a star on every board row and a Starred scope in the
//!   sidebar. It is the one piece of organisation that costs a single click.
//! - **Recent leads.** Miro opens on a search box and a template gallery; §5 records
//!   that the user's actual intent is almost always "open the board I was just in",
//!   so [`Scope::Recent`] is the default and everything else is a click away.
//!
//! Import from Miro sits here too, with the three-step instruction spelled out,
//! because §5 notes the user found the copy-paste flow confusing described in prose.
//!
//! # Nothing here acts
//!
//! Every one of those verbs leaves as a [`LibraryEvent`]. The library does not create
//! a folder, move a file or write a star — it reports, and the app decides, exactly as
//! the rest of the crate does. That is what lets the whole screen be driven from a
//! test with synthetic clicks and no store behind it.

use crate::event::{EventSink, LibraryEvent, Secret, UiEvent};
use crate::icon::Icon;
use crate::theme::{
    CARD_PADDING, Palette, SIDEBAR_WIDTH, card_frame, numeric, panel_title, radius,
    screen_title, space,
};
use crate::widgets::{
    ICON_STROKE, hairline, icon_button, search_field, section_header, text_field,
};
use egui::{
    Align, CornerRadius, CursorIcon, Id, Layout, Rect, Response, Sense, Stroke,
    StrokeKind, TextureId, Ui, UiBuilder, Vec2, containers::Popup, vec2,
};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A rendered board preview the app has already uploaded to the GPU.
///
/// A texture id rather than pixels: thumbnails come out of the shared blob store as
/// encoded images, and decoding them on the UI thread every frame is exactly the
/// kind of cost this project exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thumbnail {
    pub texture: TextureId,
    pub size: [usize; 2],
}

/// One board, as the library lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardCard {
    pub path: PathBuf,
    pub title: String,
    pub item_count: u64,
    pub modified: SystemTime,
    pub starred: bool,
    pub thumbnail: Option<Thumbnail>,
    /// When this board was moved to Recently deleted, if it was.
    ///
    /// `Option` rather than a `bool` because the trash lists boards by *when* they were
    /// deleted and says so on the card — "Deleted 3 days ago" — which is the only thing
    /// distinguishing a trashed board from a board, and the thing that tells you whether
    /// the one you are looking for is still recoverable.
    pub deleted: Option<SystemTime>,
}

/// A user's folder of boards.
///
/// Membership is by path rather than by a field on [`BoardCard`], which keeps a board
/// listable by an app that has no folders yet and lets one board sit in a space
/// without the library learning a second identity for it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Space {
    pub name: String,
    /// The boards in it, by the same path [`BoardCard::path`] carries.
    pub boards: Vec<PathBuf>,
    /// Pinned spaces sort to the top of the sidebar, as they do in the user's Miro.
    pub pinned: bool,
}

impl Space {
    pub fn new(name: impl Into<String>, boards: impl IntoIterator<Item = PathBuf>) -> Self {
        Self { name: name.into(), boards: boards.into_iter().collect(), pinned: false }
    }

    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.boards.iter().any(|p| p == path)
    }
}

/// Which set of boards the main pane is showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scope {
    /// Everything, most recently modified first. The default, per §5's *↑improve*.
    #[default]
    Recent,
    /// Everything, by name.
    All,
    /// The starred ones, by name.
    Starred,
    /// Recently deleted: the boards `LibraryEvent::Delete` has moved aside.
    ///
    /// A standing scope beside Recent and Starred rather than a [`Space`], because it is
    /// not one of the user's folders: nothing can be filed *into* it by hand, everything in
    /// it is filed out of every other scope, and it is the one place `Purge` is offered.
    Trash,
    /// The settings page — every preference in the application, on one screen.
    ///
    /// *"add a settings page in the home page so that we can put all of the settings that
    /// are needed to live in the dedicated settings page there."*
    ///
    /// # Why a scope rather than a dialog
    ///
    /// Preferences reach the interface through the `⋮` menu, which is a **menu**: it shows
    /// one thing at a time, closes when you click anything, and cannot say *why* a switch is
    /// off next to the switch. That is right for a verb and wrong for a page of settings you
    /// are reading rather than firing. A scope also gets the sidebar for free, which is the
    /// thing that makes settings findable at all — the menu required knowing they were
    /// behind a `⋮` beside a board's name.
    ///
    /// **The menu rows stay.** This is a second route, not a replacement: `⌘,` habits and
    /// the menu bar both still work, and every control here emits the *same event* its menu
    /// row does, so the two cannot come to disagree about what a setting means.
    ///
    /// It holds no boards, so [`LibraryState::in_scope`] answers `false` for every card and
    /// the grid is never drawn.
    Settings,
    /// One [`Space`], **by name**.
    ///
    /// By name and not by its index in [`LibraryState::spaces`], for exactly the
    /// reason [`LibraryEvent`] already gives for identifying spaces that way: an index
    /// is only valid until the app next replaces the list, and the app replaces it —
    /// *re-sorted*, because pinned spaces come first — in response to these very
    /// events. Held by index, pinning the space you were looking at silently moved the
    /// filter onto a different one, and creating a space moved it again.
    Space(String),
}

/// Which page of the settings you are on.
///
/// # The strip appears here for the first time
///
/// It was **not drawn while there was a single page** — a tab bar with one tab is a control
/// that cannot do anything, which is worse than no control because it invites the click that
/// proves it. [`settings`] still skips it on `ALL.len() == 1`; with [`Self::Account`] there
/// are two, so the strip is drawn.
///
/// The type survived the strip for exactly this moment: it is `pub`, `vellum_app::shell`
/// resolves `--show settings:<tab>` through [`Self::label`], and a page reached by a click
/// and nothing else is unphotographable without that. So `--show settings:Account` needed no
/// change to the app at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    /// Appearance and board behaviour: the application's own settings.
    #[default]
    General,
    /// The server your boards sync to, and who you are on it.
    Account,
}

impl SettingsTab {
    pub const ALL: [Self; 2] = [Self::General, Self::Account];

    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Account => "Account",
        }
    }

    /// One line at the head of the page saying what it is for.
    pub const fn subtitle(self) -> &'static str {
        match self {
            Self::General => "How Velm looks, and how a board behaves as you work on it.",
            Self::Account => "Which server your boards sync to, and who you are on it.",
        }
    }
}

/// Whether this machine is signed in to a server, as far as the interface knows.
///
/// The app owns this value: it is written by
/// [`Chrome::set_account_status`](crate::Chrome::set_account_status) once a frame and read
/// here. The page never sets it, because the page cannot know — a sign-in is a request on a
/// worker thread that answers some frames later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountState {
    /// No session. The three fields are editable and *Sign in* is offered.
    #[default]
    SignedOut,
    /// A request is out. The fields are disabled, because changing one now would describe a
    /// request that has already left.
    SigningIn,
    /// A session is held. The address and the name are shown as text, and *Sign out* is
    /// offered in place of the button.
    SignedIn,
}

/// Settings ▸ Account's own memory between frames: what is being typed, and what the app
/// last said about it.
///
/// # Why it is not on [`SettingsView`]
///
/// `SettingsView` is `Copy` and rebuilt from scratch every frame, so a `&mut String` cannot
/// live there. This is state the interface owns across frames, which is what [`LibraryState`]
/// is for.
///
/// # The two halves have different owners, and mixing them is a bug
///
/// - `server`, `username` and `password` belong to **the person typing**. Nothing outside
///   this page writes them once the page is open, or a per-frame update would fight the
///   keyboard sixty times a second.
/// - `state`, `signed_in_as`, `signed_in_to` and `message` belong to **the app**, and are
///   replaced every frame.
///
/// `Debug` is written by hand because [`LibraryState`] derives it and this holds a password.
#[derive(Default)]
pub struct AccountFields {
    /// The address as typed, raw. The app normalises it; see
    /// [`UiEvent::SignInRequested`](crate::UiEvent::SignInRequested).
    pub server: String,
    pub username: String,
    pub password: String,
    pub state: AccountState,
    /// Who the app says is signed in. Empty unless `state` is
    /// [`AccountState::SignedIn`].
    pub signed_in_as: String,
    /// The address the app actually used, after normalising. Shown so the person can see
    /// that `boards.example.com` became `https://boards.example.com/`.
    pub signed_in_to: String,
    /// One sentence from the app, or `None` when nothing has failed.
    pub message: Option<String>,
}

impl std::fmt::Debug for AccountFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountFields")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("state", &self.state)
            .field("signed_in_as", &self.signed_in_as)
            .field("signed_in_to", &self.signed_in_to)
            .field("message", &self.message)
            .finish()
    }
}

/// Grid of cards or a dense list. Both are in Miro; the list is the one that scales
/// past a screenful, so it is worth having rather than being a toggle for its own
/// sake.
///
/// Named `LayoutMode` rather than `Layout` because this module also lays rows out with
/// [`egui::Layout`], and two types called `Layout` in one file is a trap for whoever
/// edits it next. Re-exported as `LibraryLayout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    #[default]
    Grid,
    List,
}

/// Whether **All boards** is broken into folders or shown as one flat grid.
///
/// *"in the all boards page i want you to give me an option to group them by folders or
/// free for all"*. It is a property of that page and not of the others on purpose: Recent
/// has its own three bands, a folder's own page is already one folder, and Starred is a
/// cross-section that folders would cut up rather than organise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    /// One section per folder, pinned first, with the unfiled boards last.
    #[default]
    Folders,
    /// Every board in one grid — the user's *"free for all"*.
    Flat,
}

/// The library's own memory between frames.
#[derive(Debug, Default)]
pub struct LibraryState {
    pub search: String,
    /// The user's folders, supplied by the app through
    /// [`Chrome::set_spaces`](crate::Chrome::set_spaces).
    pub spaces: Vec<Space>,
    pub scope: Scope,
    /// Which page of the settings is open. Ignored on every other scope.
    pub settings_tab: SettingsTab,
    /// Settings ▸ Account's fields and status. Ignored on every other page.
    pub account: AccountFields,
    pub layout: LayoutMode,
    /// How **All boards** is arranged. Ignored by every other scope.
    pub grouping: Grouping,
    /// Where each space's row was drawn this frame, so a board dragged out of the
    /// grid can be dropped onto one.
    ///
    /// Rebuilt every frame by the sidebar, which is laid out before the boards are, so
    /// the targets are already known by the time a card is under the pointer. A stale
    /// rectangle would file a board into whatever now occupies that patch of screen.
    drop_targets: Vec<(String, Rect)>,
}

impl LibraryState {
    /// The space the current scope names, if it names one.
    ///
    /// `None` for a space that is no longer there, which happens whenever the app
    /// replaces the list while one of them is selected.
    pub fn selected_space(&self) -> Option<&Space> {
        match &self.scope {
            Scope::Space(name) => self.spaces.iter().find(|s| &s.name == name),
            Scope::Recent | Scope::All | Scope::Starred | Scope::Trash | Scope::Settings => None,
        }
    }

    /// Whether a board belongs in the current scope.
    ///
    /// A board is in **at most one** space: the sidebar counts, the *Move to* tick and
    /// the space column in the list all read the first space that claims it, and
    /// [`LibraryEvent::MoveToSpace`] carries one `Option<String>` rather than a set,
    /// so there is no gesture in the interface that can put a board in two.
    fn in_scope(&self, card: &BoardCard) -> bool {
        // **Deleted boards are in exactly one scope.** Filtered here rather than by the app
        // so there is one answer: a board in the trash must not appear in Recent, in All
        // boards, in Starred or in the folder it was filed under — and it stays in that
        // folder's list on disk the whole time, so that a restore puts it back where it was
        // rather than dropping it into no folder at all.
        if card.deleted.is_some() {
            return self.scope == Scope::Trash;
        }
        match &self.scope {
            // Neither holds boards. The trash holds only *deleted* ones, caught above; the
            // settings page holds none at all.
            Scope::Trash | Scope::Settings => false,
            Scope::Recent | Scope::All => true,
            Scope::Starred => card.starred,
            // A scope naming a space that has been deleted shows nothing rather than
            // silently falling back to everything, which would look like the filter
            // had been ignored.
            Scope::Space(name) => self
                .spaces
                .iter()
                .find(|s| &s.name == name)
                .is_some_and(|s| s.contains(&card.path)),
        }
    }

    /// The heading over the main pane.
    fn title(&self) -> &str {
        match &self.scope {
            Scope::Recent => "Recent",
            Scope::All => "All boards",
            Scope::Starred => "Starred",
            Scope::Trash => "Recently deleted",
            Scope::Settings => "Settings",
            // The name the user clicked, whether or not the space still exists: a
            // heading that changed to "Space" the instant one was deleted would read
            // as a rendering fault rather than as the deletion.
            Scope::Space(name) => name,
        }
    }

    /// What an empty pane should say, given why it is empty.
    fn nothing_here(&self) -> &'static str {
        if !self.search.trim().is_empty() {
            "No boards match that search"
        } else {
            match self.scope {
                Scope::Starred => "No starred boards yet",
                // Stated as a *promise*, not as an absence. This is the one screen where
                // being empty is good news, and where the user's question is not "where are
                // my boards" but "is the one I deleted still here".
                Scope::Trash => "Nothing deleted — boards you delete wait here until you empty it",
                // Never reached: the settings page draws its own body and never falls
                // through to the empty-grid message.
                Scope::Settings => "",
                Scope::Space(_) => "Nothing in this folder yet",
                Scope::Recent | Scope::All => "No boards here",
            }
        }
    }
}

/// Case-insensitive substring match on the title.
///
/// Substring rather than fuzzy: board titles are short and typed by the same person
/// who is searching them, and a fuzzy match on "map" that surfaces "Marketing plan"
/// above "Mind map" is worse than no match at all.
pub fn matches(card: &BoardCard, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    card.title.to_lowercase().contains(&query.to_lowercase())
}

/// "just now", "12 minutes ago", "3 days ago".
///
/// Relative rather than absolute, because the question this line answers is "is this
/// the one I had open yesterday", not "what was the date".
pub fn relative_time(modified: SystemTime, now: SystemTime) -> String {
    let Ok(elapsed) = now.duration_since(modified) else {
        // A board whose mtime is in the future — a copied file, a clock change —
        // reads as current rather than as a negative age.
        return "just now".to_owned();
    };
    let seconds = elapsed.as_secs();
    let plural = |n: u64, unit: &str| {
        if n == 1 { format!("1 {unit} ago") } else { format!("{n} {unit}s ago") }
    };
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3_599 => plural(seconds / 60, "minute"),
        3_600..=86_399 => plural(seconds / 3_600, "hour"),
        86_400..=2_591_999 => plural(seconds / 86_400, "day"),
        2_592_000..=31_535_999 => plural(seconds / 2_592_000, "month"),
        _ => plural(seconds / 31_536_000, "year"),
    }
}

/// The folder a board is filed under, if any.
///
/// First match wins. A board can only be in one folder — `Library::move_to_space` removes
/// it from every other on the way in — so a second match would be a corrupt sidecar, and
/// showing the first is a better answer than showing both.
fn folder_of<'a>(board: &BoardCard, spaces: &'a [Space]) -> Option<&'a str> {
    spaces
        .iter()
        .find(|space| space.boards.contains(&board.path))
        .map(|space| space.name.as_str())
}

/// How many cards fit across `available` points, at `gutter` between them.
///
/// The whole reason the first band is *one row* rather than a fixed number of boards:
/// *"the amount of boards on that row will depend on the width of the viewport"*. A count
/// chosen in advance is either short of the window on a wide one or wrapped onto a second
/// row on a narrow one, and a "row" that wraps is not a row.
///
/// `n` cards need `n` widths and `n - 1` gutters, which is why the gutter is added to both
/// sides of the division rather than only to the card.
fn columns_that_fit(available: f32, gutter: f32) -> usize {
    (((available + gutter) / (CARD_WIDTH + gutter)).floor() as usize).max(1)
}

/// The three bands the Recent page is laid out in, in the supplied description:
///
/// > *"on the top there would be one row that is the most recent boards that i opened …
/// > and then the next row will be pinned boards if i have any pinned boards or folders,
/// > if there is none pinned it will be every other … by the regular recent boards"*
///
/// Pure, and split out from the drawing for that reason: which board lands in which band is
/// the part with rules in it, and none of those rules need a window to check.
///
/// **A starred board is in Pinned even when it is also in the top row**, and it is the one
/// place a board is deliberately drawn twice. This was written the other way first, on the
/// reasoning that two cards for one board makes the page lie about how many boards there
/// are; the user overruled it — *"the boards that are starred, even if they are under
/// recently opened, if they are starred they should still be under pinned"* — and they are
/// right about what the section is for. Pinned is where you go to find the things you
/// pinned. A starred board silently missing from it because it happens to have been opened
/// this morning makes the section unreliable, and an unreliable section is worse than a
/// repeated card: you stop looking there.
///
/// The tail is still exclusive of both, so a board is in at most **two** bands and never
/// three.
#[derive(Debug, Default, PartialEq)]
struct RecentBands<'a> {
    /// Exactly one row of them, most recently modified first.
    first_row: Vec<&'a BoardCard>,
    /// **Every** starred board, top row or not. Pinned *folders* are drawn beside these but
    /// are not boards, so they are not here.
    pinned: Vec<&'a BoardCard>,
    /// Everything in neither band above, still in recency order.
    rest: Vec<&'a BoardCard>,
}

fn recent_bands<'a>(visible: &[&'a BoardCard], columns: usize) -> RecentBands<'a> {
    let mut bands = RecentBands::default();
    for (index, board) in visible.iter().enumerate() {
        if board.starred {
            bands.pinned.push(board);
        }
        if index < columns {
            bands.first_row.push(board);
        } else if !board.starred {
            bands.rest.push(board);
        }
    }
    bands
}

fn item_count_label(count: u64) -> String {
    if count == 1 { "1 item".to_owned() } else { format!("{count} items") }
}

/// Card width, and the reason the grid is laid out by hand rather than with
/// `Grid`: the column count follows the window, and `Grid` needs it up front.
const CARD_WIDTH: f32 = space::of(59);
/// Card height — **measured, not chosen**, and the two have to be kept together by hand.
///
/// A card's content is a 108-point picture, one line of title, one metadata row and the
/// leading between them: 155 points, plus [`CARD_PADDING`] and the card's border on both
/// sides, is 173. This is the next multiple of the unit above that. A slot shorter than its
/// content clips the metadata row against the padding; a slot much taller leaves a gap the
/// eye reads as a mistake, which is what the grid looked like before — 16 points between
/// columns and 31 between rows.
///
/// Nothing forces a card to this size, which is why every part of that stack is fixed: the
/// title is truncated to one line and the folder shares the metadata row rather than taking
/// one of its own. The `debug_assert` in [`card`] is what catches a change to any of them.
const CARD_HEIGHT: f32 = space::of(44);

/// The id a board's card interacts under.
///
/// Absolute rather than salted onto whatever `Ui` happens to be building the grid: the
/// card no longer *is* a `Ui`, so there is no scope to hang a salt on, and one board
/// draws one card per frame so there is nothing to collide with.
fn card_id(band: Band, path: &Path) -> Id {
    Id::new(("vellum-board", band.tag(), path))
}

/// Which section of the library a card is being drawn in.
///
/// It exists to keep egui's ids apart, and it exists **because a board can now be drawn
/// twice**: a starred board is in the Recent page's top row and under Pinned, at the user's
/// request. Two widgets sharing one `Id` is an id clash — egui gives the interaction to
/// whichever registered last, so the hover ring would light on one card while the click
/// landed on the other, and a drag would pick up a card the pointer was not on.
///
/// `card_id`'s doc used to say *"a board appears once per frame, so there is nothing to
/// collide with"*. That was true when it was written and stopped being true two sections
/// later, which is the kind of comment worth distrusting on sight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    /// The one grid every scope but Recent draws, and Recent's own tail.
    Boards,
    /// Recent's top row.
    Recent,
    /// Recent's Pinned section — the only band that repeats a card.
    Pinned,
}

impl Band {
    const fn tag(self) -> &'static str {
        match self {
            Self::Boards => "boards",
            Self::Recent => "recent",
            Self::Pinned => "pinned",
        }
    }
}

/// Edge length of the small square buttons that live inside a row — the star, the
/// pin, the overflow menu.
const ROW_BUTTON: f32 = space::of(5);

/// The steps that get a Miro board across.
///
/// Written out rather than described, because `docs/04-ui-reference.md` §5 records
/// that the user found the copy-paste flow confusing when it was explained in prose.
///
/// The fourth step answers *"okay so where do i import rtb file then?"*, and it names
/// **both** routes because they are for different jobs. Import from Miro now opens a real
/// file picker, which is right for the board in front of you; the archives folder is right
/// for the other fifty-seven, where being asked to pick a file per board is the chore the
/// folder exists to remove. It is last because the first three work without either: a paste
/// with no archive still brings every widget, just without image pixels.
pub const IMPORT_STEPS: [&str; 4] = [
    "Open the board in Miro",
    "Press Cmd+A, then Cmd+C",
    "Come back here and press Cmd+V",
    "For pictures: Board ▸ Import from Miro asks for that board's .rtb backup. \
     Bringing many across? Drop them all in the archives folder instead \
     — Application Support ▸ Vellum ▸ archives",
];

pub(crate) fn show(
    ui: &mut Ui,
    palette: Palette,
    state: &mut LibraryState,
    boards: &[BoardCard],
    now: SystemTime,
    settings_view: &SettingsView,
    events: &mut EventSink,
) {
    sidebar(ui, palette, state, boards, events);

    let frame = egui::Frame::new()
        .fill(palette.backdrop)
        .inner_margin(egui::Margin::symmetric(space::of(6) as i8, space::of(5) as i8));

    egui::CentralPanel::default_margins().frame(frame).show(ui, |ui| {
        header(ui, palette, state, events);
        ui.add_space(space::of(4));

        // The settings page holds no boards, so it returns before the grid is built at all
        // rather than filtering an empty list through it and landing on *"No boards here"*.
        if state.scope == Scope::Settings {
            settings(
                ui,
                palette,
                &mut state.settings_tab,
                settings_view,
                &mut state.account,
                events,
            );
            return;
        }

        let mut visible: Vec<&BoardCard> = boards
            .iter()
            .filter(|c| state.in_scope(c) && matches(c, &state.search))
            .collect();
        if state.scope == Scope::Recent {
            visible.sort_by_key(|c| std::cmp::Reverse(c.modified));
        } else {
            visible.sort_by_key(|card| card.title.to_lowercase());
        }

        if visible.is_empty() {
            if boards.is_empty() {
                empty_state(ui, palette, events);
            } else {
                ui.add_space(space::of(6));
                ui.label(egui::RichText::new(state.nothing_here()).color(palette.muted));
            }
            return;
        }

        // Copied out before the closure so the rows can read the spaces while the
        // layout is being decided from the same borrow.
        let layout = state.layout;
        let spaces = state.spaces.as_slice();
        // Where the sidebar drew its spaces, a few lines ago in this same frame, so a
        // board dragged out of the grid knows what it is over.
        let targets = state.drop_targets.as_slice();
        // Drag-to-scroll is `DragScroll::OnTouch` by default, so a mouse dragging a
        // card out of this list to file it in a space cannot also pan the list. Left
        // at the default deliberately: `docs/06-mouse-controls.md` defers touch and
        // trackpad, and this is the one place where forcing it off would matter.
        // Which folder a tile in the Pinned band was clicked into, applied *after* the
        // scroll area — `state.spaces` is borrowed immutably for the whole of it, and the
        // scope lives on the same `state`. The same shape `move_to_space` already uses.
        let mut opened: Option<String> = None;
        let scope = state.scope.clone();
        let grouping = state.grouping;
        let searching = !state.search.trim().is_empty();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match layout {
            LayoutMode::Grid => {
                let gutter = space::of(4);
                let columns = columns_that_fit(ui.available_width(), gutter);
                // A plain function rather than a closure: a closure that captured `events`
                // would hold it mutably for as long as it existed, and the Pinned band
                // needs it too — for the cards it draws beside the folder tiles.
                let grid =
                    |ui: &mut Ui, boards: &[&BoardCard], band: Band, events: &mut EventSink| {
                        board_grid(
                            ui, palette, boards, band, spaces, targets, now, gutter, events,
                        );
                    };

                match (&scope, grouping, searching) {
                    // **The Recent page's three bands.** Not while searching: a search is
                    // a question about every board at once, and slicing the answer into
                    // "recent", "pinned" and "the rest" hides matches under headings the
                    // query had nothing to do with.
                    (Scope::Recent, _, false) => {
                        let bands = recent_bands(&visible, columns);
                        let folders: Vec<&Space> =
                            spaces.iter().filter(|space| space.pinned).collect();

                        section_header(ui, palette, "Recently opened");
                        grid(ui, &bands.first_row, Band::Recent, events);

                        // Omitted whole when there is nothing pinned, rather than drawn
                        // empty: a heading with nothing under it reads as something that
                        // failed to load.
                        if !folders.is_empty() || !bands.pinned.is_empty() {
                            section_header(ui, palette, "Pinned");
                            let wrap = egui::Layout::left_to_right(egui::Align::Min)
                                .with_main_wrap(true);
                            ui.with_layout(wrap, |ui| {
                                ui.spacing_mut().item_spacing = Vec2::splat(gutter);
                                // Folders first, then the starred boards. A folder is the
                                // bigger thing and holds the boards; putting it after them
                                // would read as an afterthought.
                                for space in &folders {
                                    if folder_tile(ui, palette, space).clicked() {
                                        opened = Some(space.name.clone());
                                    }
                                }
                                for board in &bands.pinned {
                                    card(
                                        ui, palette, board, Band::Pinned, spaces, targets,
                                        now, events,
                                    );
                                }
                            });
                        }

                        if !bands.rest.is_empty() {
                            section_header(ui, palette, "All boards");
                            grid(ui, &bands.rest, Band::Boards, events);
                        }
                    }

                    // **All boards, grouped.** One section per folder in the sidebar's own
                    // order — pinned first — and the unfiled boards last under a heading of
                    // their own, so a board with no folder is somewhere rather than nowhere.
                    (Scope::All, Grouping::Folders, false) => {
                        let mut filed = 0usize;
                        for space in spaces {
                            let held: Vec<&BoardCard> = visible
                                .iter()
                                .copied()
                                .filter(|board| space.contains(&board.path))
                                .collect();
                            if held.is_empty() {
                                continue;
                            }
                            filed += held.len();
                            section_header(ui, palette, &space.name);
                            grid(ui, &held, Band::Boards, events);
                        }
                        let unfiled: Vec<&BoardCard> = visible
                            .iter()
                            .copied()
                            .filter(|board| folder_of(board, spaces).is_none())
                            .collect();
                        if !unfiled.is_empty() {
                            // Only headed when something above it was: with no folders at
                            // all, "Unfiled" is a label on every board you own.
                            if filed > 0 {
                                section_header(ui, palette, "Not in a folder");
                            }
                            grid(ui, &unfiled, Band::Boards, events);
                        }
                    }

                    // Every other page, and every search: one grid.
                    _ => grid(ui, &visible, Band::Boards, events),
                }
            }
            LayoutMode::List => {
                for board in &visible {
                    list_row(ui, palette, board, spaces, targets, now, events);
                }
            }
        });

        if let Some(name) = opened {
            state.scope = Scope::Space(name);
        }
    });
}

/// One wrapping grid of cards.
///
/// `horizontal_wrapped` centres its contents on the cross axis, so cards of different
/// heights float to different vertical offsets and the row reads as misaligned. A grid hangs
/// from a common top edge, so the layout is spelled out rather than taking egui's default.
#[allow(clippy::too_many_arguments)]
fn board_grid(
    ui: &mut Ui,
    palette: Palette,
    boards: &[&BoardCard],
    band: Band,
    spaces: &[Space],
    targets: &[(String, Rect)],
    now: SystemTime,
    gutter: f32,
    events: &mut EventSink,
) {
    let wrap = Layout::left_to_right(Align::Min).with_main_wrap(true);
    ui.with_layout(wrap, |ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(gutter);
        for board in boards {
            card(ui, palette, board, band, spaces, targets, now, events);
        }
    });
}

/// A pinned folder, drawn in the grid at a card's own size.
///
/// The size is the point: it sits in the Pinned band beside starred boards, and a tile that
/// was a different height would break the row the band exists to be. Everything about its
/// geometry comes from [`card`]'s — one slot, the same frame at the same padding.
///
/// It reads as a *container* rather than as a board through what fills it: a folder mark
/// where a board has its preview picture, on the raised tint rather than a picture's own,
/// and a count of boards where a board has a count of items.
fn folder_tile(ui: &mut Ui, palette: Palette, space: &Space) -> Response {
    let (_, rect) = ui.allocate_space(vec2(CARD_WIDTH, CARD_HEIGHT));
    let response = ui.interact(rect, Id::new(("vellum-folder", &space.name)), Sense::click());

    let frame = card_frame(palette);
    let content = rect.shrink(CARD_PADDING + frame.stroke.width);
    ui.painter().add(frame.paint(content));

    let mut tile = ui.new_child(
        UiBuilder::new()
            .id_salt(("folder", &space.name))
            .max_rect(content)
            .layout(Layout::top_down(Align::Min)),
    );
    let ui = &mut tile;
    ui.spacing_mut().item_spacing.y = space::UNIT;

    let (well, _) = ui.allocate_exact_size(vec2(ui.available_width(), space::of(27)), Sense::hover());
    let corner = CornerRadius::same(radius::SMALL);
    ui.painter().rect_filled(well, corner, palette.raised);
    let glyph = Rect::from_center_size(well.center(), Vec2::splat(space::of(9)));
    Icon::Folder.paint(&ui.painter().clone(), glyph, palette.muted, ICON_STROKE);
    ui.painter().rect_stroke(well, corner, palette.hairline_stroke(), StrokeKind::Inside);

    ui.add(
        egui::Label::new(egui::RichText::new(&space.name).strong().color(palette.text))
            .truncate(),
    );
    ui.label(
        numeric(if space.boards.len() == 1 {
            "1 board".to_owned()
        } else {
            format!("{} boards", space.boards.len())
        })
        .color(palette.faint),
    );

    if response.hovered() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(radius::MEDIUM),
            Stroke::new(palette.hairline_width(), palette.info),
            StrokeKind::Inside,
        );
    }
    response.on_hover_text(format!("Open {}", space.name))
}

/// The left rail: search, the three standing scopes, then Spaces.
/// Whether one of these boards' cards is being dragged right now.
///
/// Asked of `egui` rather than remembered in [`LibraryState`], and that is what makes the drop
/// highlight exact rather than one frame stale: the sidebar is laid out *before* the cards, so
/// a flag the card pass sets would arrive a frame late — and the frame it is late on is the one
/// the drag begins, which is when the user is looking for the target.
///
/// A card's `Id` is derived from its path, so the ids can be reconstructed here; there is no
/// way to ask `egui` what kind of widget the dragged id belongs to.
fn card_is_dragging(ui: &Ui, boards: &[BoardCard]) -> bool {
    // Every band, because a board can be drawn in two of them and either card can be the
    // one in flight. Asking only about `Band::Boards` would leave the sidebar's drop
    // highlight dark for a drag that started on the Recent page's top row.
    boards.iter().any(|board| {
        [Band::Boards, Band::Recent, Band::Pinned]
            .into_iter()
            .any(|band| ui.ctx().is_being_dragged(card_id(band, &board.path)))
    })
}

fn sidebar(
    ui: &mut Ui,
    palette: Palette,
    state: &mut LibraryState,
    boards: &[BoardCard],
    events: &mut EventSink,
) {
    let dragging = card_is_dragging(ui, boards);
    let frame = egui::Frame::new()
        .fill(palette.surface)
        .inner_margin(egui::Margin::symmetric(space::of(3) as i8, space::of(3) as i8));

    egui::Panel::left("vellum-library-sidebar")
        .frame(frame)
        .exact_size(SIDEBAR_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .show(ui, |ui| {
            // The mark, at the top of the one screen that is allowed to carry it, with
            // its clear space kept on every side.
            let size = space::of(6);
            ui.horizontal(|ui| {
                ui.add_space(crate::mark::clear_space(size));
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
                crate::mark::paint(&ui.painter().clone(), rect, palette);
                ui.add_space(crate::mark::clear_space(size));
                // Lowercase, tightly tracked, in the interface face — the lockup from
                // `assets/logo/README.md`, so the app reads as native rather than
                // branded.
                ui.label(
                    egui::RichText::new("velm")
                        .size(crate::theme::text::PANEL_TITLE)
                        .color(palette.text)
                        .extra_letter_spacing(-0.7),
                );
            });
            ui.add_space(space::of(4));

            if search_field(ui, palette, Id::new("vellum-library-search"), &mut state.search) {
                events.push(UiEvent::Library(LibraryEvent::SearchChanged(state.search.clone())));
            }
            ui.add_space(space::of(2));

            // Deleted boards are not counted anywhere but in the trash. A "Recent 6" that
            // includes two boards you deleted is a number that disagrees with the page it
            // labels, which is how a count stops being read at all.
            let total = boards.iter().filter(|b| b.deleted.is_none()).count();
            let starred =
                boards.iter().filter(|b| b.starred && b.deleted.is_none()).count();
            let deleted = boards.iter().filter(|b| b.deleted.is_some()).count();
            scope_row(ui, palette, state, &Scope::Recent, "Recent", Some(total));
            // *All boards* always shows everything, whatever is filed where: it is the
            // scope a user falls back to when they cannot remember where they put
            // something, so it is the one that must never filter.
            scope_row(ui, palette, state, &Scope::All, "All boards", Some(total));
            scope_row(ui, palette, state, &Scope::Starred, "Starred", Some(starred));
            // Last of the standing scopes, and **always shown** rather than appearing when
            // something is in it: a trash you can only find once you have lost something is
            // one nobody knows exists at the moment they need it.
            scope_row(ui, palette, state, &Scope::Trash, "Recently deleted", Some(deleted));

            ui.add_space(space::of(2));
            hairline(ui, palette);
            spaces_header(ui, palette, events);

            // Pinned first, then the rest in the order the app supplied — which is the
            // order the user arranged them in, and is not ours to re-sort.
            let mut order: Vec<usize> = (0..state.spaces.len()).collect();
            order.sort_by_key(|i| !state.spaces[*i].pinned);
            state.drop_targets.clear();
            for index in order {
                space_row(ui, palette, state, index, dragging, events);
            }
            if state.spaces.is_empty() {
                ui.add_space(space::UNIT);
                ui.label(
                    egui::RichText::new("No folders yet")
                        .size(crate::theme::text::LABEL)
                        .color(palette.faint),
                );
            }

            // **Pinned to the foot of the panel**, not merely last in the list.
            //
            // Two readings of *"move the settings to the bottom"*, and the first one was
            // wrong: putting it under the folders left it floating in the middle of a tall
            // empty sidebar, still reading as one more row in a list. The bottom of the
            // *panel* is where an application's settings live — the place your eye goes
            // last and always finds the same thing — and that is what was asked for.
            //
            // `bottom_up` over the space the folders left, so the row sits on the panel's
            // floor however many folders there are, rather than at a measured offset that
            // would be wrong for every count but one. Note the reversed order: in a
            // bottom-up layout the **first** thing added is the lowest, so this reads
            // gap, row, gap, rule and paints rule, gap, row, gap.
            //
            // The comment this replaced already argued for being below the folders and the
            // code had never done it — it is not a way of looking at boards, it is the one
            // row here that changes the application, and the rule above it says so.
            //
            // A count of zero would read as *"no settings"*, so it carries none.
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.add_space(space::of(2));
                scope_row(ui, palette, state, &Scope::Settings, "Settings", None);
                ui.add_space(space::of(2));
                hairline(ui, palette);
            });
        });
}

/// The Spaces caption, and the `+` that makes one.
fn spaces_header(ui: &mut Ui, palette: Palette, events: &mut EventSink) {
    ui.add_space(space::of(3));
    ui.horizontal(|ui| {
        ui.label(crate::theme::section_label("Folders", palette));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, palette, Icon::Plus, ROW_BUTTON, false)
                .on_hover_text("New folder")
                .clicked()
            {
                events.push(UiEvent::Library(LibraryEvent::CreateSpace));
            }
        });
    });
    ui.add_space(space::UNIT);
}

/// One selectable row in the sidebar, with its count set in the numeric face so the
/// column of numbers lines up down the rail.
fn scope_row(
    ui: &mut Ui,
    palette: Palette,
    state: &mut LibraryState,
    scope: &Scope,
    label: &str,
    // `None` for a row that counts nothing. Settings is the only one: a **0** beside it
    // would read as "no settings", which is the opposite of true.
    count: Option<usize>,
) {
    let selected = &state.scope == scope;
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), space::of(6)), Sense::click());
    if ui.is_rect_visible(rect) {
        row_background(ui, palette, rect, selected, response.hovered());
        let inner = rect.shrink2(vec2(space::of(2), 0.0));
        let color = if selected { palette.on_accent_soft } else { palette.text };
        ui.painter().text(
            inner.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(crate::theme::text::BODY),
            color,
        );
        if let Some(count) = count {
            ui.painter().text(
                inner.right_center(),
                egui::Align2::RIGHT_CENTER,
                count.to_string(),
                egui::FontId::monospace(crate::theme::text::NUMERIC),
                palette.faint,
            );
        }
    }
    if response.clicked() {
        state.scope = scope.clone();
    }
}

/// One space: selects on click, takes a board dropped on it, and carries its own
/// rename / pin / delete menu.
///
/// The count gives its place up to a pin toggle and a `⋮` while the pointer is on the
/// row, which is what Miro does and what keeps a 192-point rail from having to hold a
/// name, a number and three buttons at once.
/// One space in the sidebar: its pin, its name, its count, and its buttons.
///
/// `dragging` is whether a board is in flight, so the row can show itself as a drop target
/// *under* its own label — see [`drop_background`].
fn space_row(
    ui: &mut Ui,
    palette: Palette,
    state: &mut LibraryState,
    index: usize,
    dragging: bool,
    events: &mut EventSink,
) {
    let (name, count, pinned) = {
        let space = &state.spaces[index];
        (space.name.clone(), space.boards.len(), space.pinned)
    };
    let selected = matches!(&state.scope, Scope::Space(chosen) if chosen == &name);

    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), space::of(6)), Sense::click());
    state.drop_targets.push((name.clone(), rect));

    let inner = rect.shrink2(vec2(space::of(2), 0.0));
    let menu_at = Rect::from_center_size(
        egui::pos2(inner.right() - ROW_BUTTON / 2.0, inner.center().y),
        Vec2::splat(ROW_BUTTON),
    );
    let pin_at = menu_at.translate(vec2(-ROW_BUTTON, 0.0));
    // Read from the pointer rather than from the row's own `hovered`, so the row stays
    // open while the pointer is on one of the buttons it just revealed — **and while
    // its menu is open**, because the pointer has to leave the row to reach the menu
    // and withdrawing the `⋮` at that moment takes the menu with it.
    let menu_id = Id::new(("vellum-space-menu", &name));
    let hot = ui.rect_contains_pointer(rect) || Popup::is_id_open(ui.ctx(), menu_id);

    // The board in flight is over this row: show it as the target, *before* the label is
    // drawn, so the name stays readable. See `drop_background`.
    let drop = dragging && ui.rect_contains_pointer(rect);

    // The flourish for a board that just landed here. Read before the paint so the
    // three backgrounds are one decision rather than three overlapping ones.
    let landed = landing_progress(ui.ctx(), &name);

    if ui.is_rect_visible(rect) {
        if drop {
            drop_background(ui, palette, rect);
        } else if let Some(progress) = landed {
            landed_background(ui, palette, rect, progress);
        } else {
            row_background(ui, palette, rect, selected, response.hovered());
        }
        let color =
            if selected && !drop && landed.is_none() { palette.on_accent_soft } else { palette.text };
        let mut left = inner.left();
        if pinned {
            // A pin rather than a dot: `docs/04-ui-reference.md` §5 shows pinned
            // spaces marked, and a mark that looks like punctuation reads as a typo.
            let glyph = Rect::from_center_size(
                egui::pos2(left + ROW_BUTTON / 2.0, inner.center().y),
                Vec2::splat(space::of(3)),
            );
            Icon::Pin.paint(&ui.painter().clone(), glyph, palette.faint, ICON_STROKE);
            left += ROW_BUTTON;
        }
        ui.painter().text(
            egui::pos2(left, inner.center().y),
            egui::Align2::LEFT_CENTER,
            &name,
            egui::FontId::proportional(crate::theme::text::BODY),
            color,
        );
        if !hot {
            ui.painter().text(
                egui::pos2(menu_at.left() - space::UNIT, inner.center().y),
                egui::Align2::RIGHT_CENTER,
                count.to_string(),
                egui::FontId::monospace(crate::theme::text::NUMERIC),
                palette.faint,
            );
        }
    }

    // Registered after the row, so a click on a button is a click on the button rather
    // than on the row underneath it.
    if hot {
        let pin = sub_button(ui, palette, ("space-pin", index), pin_at, Icon::Pin);
        if pin
            .on_hover_text(if pinned { "Unpin" } else { "Pin to top" })
            .clicked()
        {
            events.push(UiEvent::Library(LibraryEvent::SetSpacePinned {
                space: name.clone(),
                pinned: !pinned,
            }));
        }
        let more = sub_button(ui, palette, ("space-menu", index), menu_at, Icon::More);
        let more = more.on_hover_text("Folder options");
        Popup::menu(&more)
            .id(menu_id)
            .show(|ui| space_menu(ui, palette, &name, pinned, events));
    }
    // Right-clicking the row itself opens the same menu, because that is where a hand
    // goes first and a second implementation would be a second set of entries.
    Popup::context_menu(&response).show(|ui| space_menu(ui, palette, &name, pinned, events));

    if response.clicked() {
        state.scope = Scope::Space(name);
    }
}

fn space_menu(
    ui: &mut Ui,
    palette: Palette,
    name: &str,
    pinned: bool,
    events: &mut EventSink,
) {
    ui.set_min_width(space::of(34));
    ui.set_max_width(space::of(72));
    if ui.button("Rename…").clicked() {
        events.push(UiEvent::Library(LibraryEvent::RenameSpace(name.to_owned())));
    }
    if ui.button(if pinned { "Unpin" } else { "Pin to top" }).clicked() {
        events.push(UiEvent::Library(LibraryEvent::SetSpacePinned {
            space: name.to_owned(),
            pinned: !pinned,
        }));
    }
    hairline(ui, palette);
    if ui
        .add(egui::Button::new(egui::RichText::new("Delete folder…").color(palette.danger)))
        .on_hover_text("Deletes the folder, never the boards in it")
        .clicked()
    {
        events.push(UiEvent::Library(LibraryEvent::DeleteSpace(name.to_owned())));
    }
}

/// How long the "it landed here" flourish runs.
///
/// Short on purpose. Long enough to be seen when you were looking at the row you aimed
/// at, over before you could be waiting for it — and **bounded**, which is the whole
/// difference between this and the caret blink this app deliberately does not have. A
/// blink needs a timer driving redraws while nothing is happening; this asks for
/// repaints for a third of a second after something did, and then stops.
const LANDING: f64 = 0.45;

/// Where the last board landed, and when.
///
/// In egui's own scratch memory rather than in [`LibraryState`]: the cards are drawn
/// inside a closure that already holds `state.spaces` and `state.drop_targets`
/// immutably, so there is no `&mut` to write to at the moment the drop happens. Keyed
/// memory sidesteps the borrow entirely and is what egui offers it for.
fn landing_id() -> Id {
    Id::new("velm-library-landing")
}

fn note_landing(ctx: &egui::Context, space: &str) {
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| d.insert_temp(landing_id(), (space.to_owned(), now)));
}

/// How far through the flourish `space` is, `0.0` to `1.0`, or `None` when it is not the
/// row that just received a board — or the flourish has finished.
fn landing_progress(ctx: &egui::Context, space: &str) -> Option<f32> {
    let (name, at): (String, f64) = ctx.data(|d| d.get_temp(landing_id()))?;
    if name != space {
        return None;
    }
    let elapsed = ctx.input(|i| i.time) - at;
    if !(0.0..LANDING).contains(&elapsed) {
        // Dropped from memory once it is over, so an idle library is not asking egui
        // for a value on every frame for the rest of the session.
        ctx.data_mut(|d| d.remove::<(String, f64)>(landing_id()));
        return None;
    }
    // The only repaint request in the library, and it stops when the flourish does.
    ctx.request_repaint();
    Some((elapsed / LANDING) as f32)
}

/// The "it landed here" flourish: the drop tint fading out under a ring that fades with
/// it.
///
/// *"if i drop it on that specific space it will do like a small something to indicate
/// that it has been moved"*. Painted where [`drop_background`] is and for the same
/// reason — **before the row's label**, so the confirmation cannot cover the name of the
/// space it is confirming. That is the bug this file already fixed once; a fade is no
/// more entitled to sit on top of the text than a highlight was.
fn landed_background(ui: &Ui, palette: Palette, rect: Rect, progress: f32) {
    // Fades out rather than in: the row was already tinted while the board hovered over
    // it, so starting at full and decaying continues that state instead of flashing a
    // new one at the moment of release.
    let fade = 1.0 - progress;
    let corner = CornerRadius::same(radius::SMALL);
    ui.painter().rect_filled(rect, corner, palette.info_soft.gamma_multiply(fade));
    // The ring grows slightly as it fades, which is what reads as *released* rather than
    // as *still hovering*. Two points over the whole run — enough to notice, not enough
    // to shift the row's neighbours, which it cannot do anyway since nothing is
    // re-laid-out.
    ui.painter().rect_stroke(
        rect.expand(progress * 2.0),
        corner,
        Stroke::new(palette.hairline_width() * 2.0, palette.info.gamma_multiply(fade)),
        StrokeKind::Outside,
    );
}

/// The drop-target fill and ring behind a sidebar row, under its text.
///
/// **Under**, and that is the whole point. This used to be painted by `drag_to_space` onto a
/// separate `Order::Middle` layer, which sits above the panel every row is drawn in — so the
/// tint went over the space's name and *"the name of the space dissapears"* while you dragged
/// a board onto it. No choice of layer fixes that, because a panel's background and its text
/// are the same layer; the highlight has to be painted by the row itself, before its label.
fn drop_background(ui: &Ui, palette: Palette, rect: Rect) {
    let corner = CornerRadius::same(radius::SMALL);
    ui.painter().rect_filled(rect, corner, palette.info_soft);
    ui.painter().rect_stroke(
        rect,
        corner,
        Stroke::new(palette.hairline_width(), palette.info),
        StrokeKind::Inside,
    );
}

/// The selected/hovered fill behind a sidebar or list row.
fn row_background(ui: &Ui, palette: Palette, rect: Rect, selected: bool, hovered: bool) {
    let corner = CornerRadius::same(radius::SMALL);
    if selected {
        ui.painter().rect_filled(rect, corner, palette.accent_soft);
    } else if hovered {
        ui.painter().rect_filled(rect, corner, palette.hover);
    }
}

/// A small icon button placed at an exact rectangle inside a hand-painted row.
///
/// The row is one interactive rectangle and the button is another on top of it; both
/// are needed, because the row selects and the button does something else.
fn sub_button(
    ui: &mut Ui,
    palette: Palette,
    salt: impl std::hash::Hash + std::fmt::Debug,
    rect: Rect,
    icon: Icon,
) -> Response {
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(salt)
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    icon_button(&mut child, palette, icon, rect.width(), false)
}

/// A star that shows its state and toggles it.
fn star_button(
    ui: &mut Ui,
    palette: Palette,
    board: &BoardCard,
    band: Band,
    rect: Rect,
    events: &mut EventSink,
) {
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(("star", band.tag(), &board.path))
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    // The filled star is the state, not the colour: `docs/05-design-language.md` §6
    // requires the accent never to be the only carrier of meaning.
    let icon = if board.starred { Icon::StarFilled } else { Icon::Star };
    let response =
        icon_button(&mut child, palette, icon, rect.width(), board.starred)
            .on_hover_text(if board.starred { "Unstar" } else { "Star" });
    if response.clicked() {
        events.push(UiEvent::Library(LibraryEvent::SetStarred {
            path: board.path.clone(),
            starred: !board.starred,
        }));
    }
}

/// The main pane's heading: what is being shown, and how.
fn header(ui: &mut Ui, palette: Palette, state: &mut LibraryState, events: &mut EventSink) {
    ui.horizontal(|ui| {
        ui.label(screen_title(state.title()).color(palette.text));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Nothing on the right at all on the settings page. *New board*, *Import from
            // Miro* and the grid/list toggle are three answers to "what do I do with my
            // boards", and this is the one page in the library that is not about boards —
            // the trash makes the same swap two arms down for the same reason.
            if state.scope == Scope::Settings {
                return;
            }
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("New board").color(palette.on_accent),
                    )
                    .fill(palette.accent)
                    .corner_radius(CornerRadius::same(radius::SMALL)),
                )
                .clicked()
            {
                events.command(crate::command::Command::NewBoard);
            }
            if state.scope == Scope::Trash {
                // In place of New board and Import, which are both about making boards and
                // have nothing to do on this page. Danger-coloured because it is the one
                // button in the library that ends in files being removed.
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Empty Recently deleted").color(palette.danger),
                    ))
                    .on_hover_text("Deletes every board in here for good")
                    .clicked()
                {
                    events.push(UiEvent::Library(LibraryEvent::EmptyTrash));
                }
                return;
            }
            if ui.button("Import from Miro").on_hover_ui(import_hint).clicked() {
                events.command(crate::command::Command::ImportFromMiro);
            }

            ui.add_space(space::of(2));
            for (layout, icon, hint) in [
                (LayoutMode::List, Icon::List, "List"),
                (LayoutMode::Grid, Icon::Grid, "Grid"),
            ] {
                if icon_button(ui, palette, icon, space::of(6), state.layout == layout)
                    .on_hover_text(hint)
                    .clicked()
                {
                    state.layout = layout;
                }
            }

            // Only on All boards, and that is the point rather than a shortcut: every other
            // scope either has its own arrangement or *is* one folder, so a grouping control
            // there would be a switch that changes nothing — which is worse than no switch,
            // because it invites the question of why it did not work.
            if state.scope == Scope::All {
                ui.add_space(space::of(2));
                for (grouping, icon, hint) in [
                    (Grouping::Flat, Icon::Grid, "Every board in one grid"),
                    (Grouping::Folders, Icon::Folder, "Group by folder"),
                ] {
                    if icon_button(ui, palette, icon, space::of(6), state.grouping == grouping)
                        .on_hover_text(hint)
                        .clicked()
                    {
                        state.grouping = grouping;
                    }
                }
            }
        });
    });
}

/// The three steps, as a tooltip and as the empty state's body.
fn import_hint(ui: &mut Ui) {
    for (n, step) in IMPORT_STEPS.iter().enumerate() {
        ui.label(format!("{}.  {step}", n + 1));
    }
}

/// What the screen shows with nothing to list at all.
///
/// The mark, a line, and the import steps — left-aligned. `docs/05` §2 rules out
/// centred hero copy and marketing voice, and this is the one screen where both would
/// otherwise creep in.
fn empty_state(ui: &mut Ui, palette: Palette, events: &mut EventSink) {
    ui.add_space(space::of(8));
    let size = space::of(12);
    ui.horizontal(|ui| {
        ui.add_space(crate::mark::clear_space(size));
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
        crate::mark::paint(&ui.painter().clone(), rect, palette);
    });
    ui.add_space(crate::mark::clear_space(size));

    ui.label(panel_title("No boards yet").color(palette.text));
    ui.add_space(space::UNIT);
    ui.label(egui::RichText::new("Start one, or bring a Miro board across.").color(palette.muted));

    ui.add_space(space::of(4));
    section_header(ui, palette, "Import from Miro");
    for (n, step) in IMPORT_STEPS.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(numeric(format!("{}.", n + 1)).color(palette.faint));
            ui.label(egui::RichText::new(*step).color(palette.muted));
        });
    }

    ui.add_space(space::of(4));
    ui.horizontal(|ui| {
        if ui
            .add(
                egui::Button::new(egui::RichText::new("New board").color(palette.on_accent))
                    .fill(palette.accent)
                    .corner_radius(CornerRadius::same(radius::SMALL)),
            )
            .clicked()
        {
            events.command(crate::command::Command::NewBoard);
        }
        if ui.button("Import from Miro").clicked() {
            events.command(crate::command::Command::ImportFromMiro);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut Ui,
    palette: Palette,
    board: &BoardCard,
    band: Band,
    spaces: &[Space],
    targets: &[(String, Rect)],
    now: SystemTime,
    events: &mut EventSink,
) {
    // **Allocate first.** This is the line that makes the grid a grid. The enclosing
    // layout asks for `with_main_wrap(true)`, but egui tests for a wrap in exactly one
    // place — `Layout::next_frame`, reachable only through `Ui::allocate_space`. This
    // used to place the card with `scope_builder`, which never allocates, so the wrap
    // was never tested: cards marched off the right edge of the window one after
    // another and the vertical-only `ScrollArea` clipped them with no way to scroll
    // across. *"boards should not go off screen"*.
    let (_, rect) = ui.allocate_space(vec2(CARD_WIDTH, CARD_HEIGHT));

    // Interact **before** the body, not after. egui registers a widget in `Ui::new_child`
    // — before a `scope_builder` closure runs — and for overlapping widgets the one
    // registered *last* wins the click. The star inside `preview` is registered by the
    // body, so the card has to claim its rect first or it would swallow the star.
    //
    // An absolute `Id` rather than an `id_salt` on the parent so a test can look a
    // card's rect up from the context and assert where it landed. A board appears once
    // per frame, so there is nothing to collide with.
    let response = ui.interact(rect, card_id(band, &board.path), Sense::click_and_drag());

    // **The card is its slot.** It used to be a `Frame` wrapped around its content, so its
    // height was whatever the content came to — a board filed in a folder drew a taller
    // white box than the board beside it, and the leftover slot became a gap under every
    // card: measured, 16 points between columns and 31 between rows. *"i just want it to
    // look organized"*.
    //
    // Painting the frame rather than showing one is what makes that exact. Two other routes
    // were measured and both are worse: leaving it to wrap and sizing the slot to the
    // content lands a point out — 173 painted against a 172 slot, so the rows overlap by one
    // — and `set_min_height` inside the frame makes egui give the wrapping row the claimed
    // height *on top of* the space already allocated, a 177-point gap between rows.
    //
    // `card_frame` is still the single definition of how a card looks; only who calls it
    // moved. It is handed the rect the *content* gets and expands to the card — by its inner
    // margin **and by its stroke width**, which is the part worth stating: shrinking by the
    // padding alone painted the card a point proud on every side. Taken from the frame
    // rather than assumed, so a heavier card border cannot quietly put that back.
    let painted = rect;
    let frame = card_frame(palette);
    let content = rect.shrink(CARD_PADDING + frame.stroke.width);
    debug_assert_eq!(frame.widget_rect(content), rect, "the card missed its slot");
    ui.painter().add(frame.paint(content));
    {
        // The layout is stated rather than inherited. Without it the card's contents
        // inherit the grid's *wrapping left-to-right* layout, so the preview, the title and
        // the metadata row stacked vertically only because each one overflowed the row and
        // wrapped — which also made `ui.available_width()` inside `preview` mean something
        // different from what it reads like.
        let mut card = ui.new_child(
            UiBuilder::new()
                .id_salt(("card", band.tag(), &board.path))
                .max_rect(content)
                .layout(Layout::top_down(Align::Min)),
        );
        let ui = &mut card;

        // **The card states its own spacing.** The grid sets a 16-point gutter between
        // cards and an inner `Ui` inherits it, so every gap *inside* a card was 16 too: 16
        // under the picture, 16 under the title. Measured, that is 24 points of the card's
        // height spent on air, and it is what floated the text block into the middle of the
        // card instead of hanging it off the picture. A gutter between cards and the
        // leading inside one are two measurements that happened to be one value.
        ui.spacing_mut().item_spacing.y = space::UNIT;
        preview(ui, palette, board, band, events);
        // The title hangs directly off the picture — *"move the titles per card more
        // above"*. The gap is the layout's own item spacing and nothing else; the eight
        // points added here used to push every title down.
        //
        // **One line, always.** A title that wrapped made its own card taller than the ones
        // beside it, which is the raggedness `CARD_HEIGHT` exists to remove — and at 236
        // points wide, a title long enough to wrap is one nobody reads to the end of.
        ui.add(
            egui::Label::new(egui::RichText::new(&board.title).strong().color(palette.text))
                .truncate(),
        );
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::UNIT;
            ui.label(numeric(item_count_label(board.item_count)).color(palette.faint));
            // **When it was deleted, not when it was last touched.** On this one screen the
            // question is not "how fresh is this" but "how long have I got", and a trashed
            // board's modified time is frozen at whatever it was before it went in — so
            // showing it would put an unchanging date under every card in the trash.
            ui.label(
                egui::RichText::new(match board.deleted {
                    Some(at) => format!("deleted {}", relative_time(at, now)),
                    None => relative_time(board.modified, now),
                })
                .size(crate::theme::text::LABEL)
                .color(palette.faint),
            );
            // *"if the board is in a folder underneath the title it should show which
            // folder it is in"*, but *"dont put that much space for folders — it should
            // look the same"*. So it shares the metadata row rather than taking one of its
            // own: a filed card is exactly as tall as an unfiled one, which a row of its
            // own could not manage.
            //
            // Laid out from the **opposite edge**, so the count and the date keep the same
            // column on every card whether or not there is a folder to name. Truncated,
            // because a long folder name is not worth pushing the date off its own row for.
            if let Some(folder) = folder_of(board, spaces) {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(folder)
                                .size(crate::theme::text::LABEL)
                                .color(palette.muted),
                        )
                        .truncate(),
                    );
                    let glyph = ui.allocate_response(
                        egui::Vec2::splat(crate::theme::text::LABEL),
                        egui::Sense::hover(),
                    );
                    Icon::Folder.paint(
                        &ui.painter().clone(),
                        glyph.rect,
                        palette.faint,
                        crate::widgets::ICON_STROKE,
                    );
                });
            }
        });
        // Every card is the same height **by construction** — a fixed picture, one line of
        // title, one metadata row — so `CARD_HEIGHT` is a measurement of this stack rather
        // than a box it is forced into. This is what keeps the two honest, and it fires in
        // any debug build that draws a card, which is every test that opens the library.
        debug_assert!(
            ui.min_rect().height() <= content.height(),
            "a card's content grew to {} in a card with room for {}",
            ui.min_rect().height(),
            content.height(),
        );
    }

    if response.hovered() {
        // Hover is `monitor-cyan`, per §3. The red is reserved for selection, and a
        // card that turns red under the pointer claims to be selected.
        //
        // On the frame's own rect and at the frame's own radius, so the ring lands *on* the
        // card's edge and replaces its hairline rather than drawing a second, larger box
        // around it.
        ui.painter().rect_stroke(
            painted,
            CornerRadius::same(radius::MEDIUM),
            Stroke::new(palette.hairline_width(), palette.info),
            StrokeKind::Inside,
        );
    }
    if response.double_clicked() || response.clicked() {
        // **In the trash a click restores rather than opens**, and it is the only sensible
        // reading of the gesture: the one thing anybody wants from a board they deleted is
        // to have it back. Opening it would leave it deleted while it was on screen, which
        // is a state nothing else in the app has. The board reappears in Recent, where it
        // can be opened the same way as any other.
        events.push(UiEvent::Library(if board.deleted.is_some() {
            LibraryEvent::Restore(board.path.clone())
        } else {
            LibraryEvent::Open(board.path.clone())
        }));
    }

    drag_to_space(ui, palette, board, &response, targets, events);
    context_menu(&response, palette, board, spaces, events);
}

/// One board as a dense row: name, space, count, when it was last touched, star.
fn list_row(
    ui: &mut Ui,
    palette: Palette,
    board: &BoardCard,
    spaces: &[Space],
    targets: &[(String, Rect)],
    now: SystemTime,
    events: &mut EventSink,
) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), space::of(8)), Sense::click_and_drag());
    let inner = rect.shrink2(vec2(space::of(2), 0.0));
    let star_at = Rect::from_center_size(
        egui::pos2(inner.right() - ROW_BUTTON / 2.0, inner.center().y),
        Vec2::splat(ROW_BUTTON),
    );

    if ui.is_rect_visible(rect) {
        row_background(ui, palette, rect, false, response.hovered());
        let glyph = Rect::from_center_size(
            egui::pos2(inner.left() + space::of(2), inner.center().y),
            Vec2::splat(space::of(4)),
        );
        Icon::Frame.paint(&ui.painter().clone(), glyph, palette.faint, ICON_STROKE);

        ui.painter().text(
            egui::pos2(glyph.right() + space::of(3), inner.center().y),
            egui::Align2::LEFT_CENTER,
            &board.title,
            egui::FontId::proportional(crate::theme::text::BODY),
            palette.text,
        );

        // The space a board is in, so the list answers the question the sidebar poses.
        let space_name =
            spaces.iter().find(|s| s.contains(&board.path)).map_or("—", |s| s.name.as_str());
        ui.painter().text(
            egui::pos2(inner.right() - space::of(52), inner.center().y),
            egui::Align2::LEFT_CENTER,
            space_name,
            egui::FontId::proportional(crate::theme::text::LABEL),
            palette.faint,
        );
        ui.painter().text(
            egui::pos2(inner.right() - space::of(32), inner.center().y),
            egui::Align2::LEFT_CENTER,
            item_count_label(board.item_count),
            egui::FontId::monospace(crate::theme::text::NUMERIC),
            palette.faint,
        );
        ui.painter().text(
            egui::pos2(star_at.left() - space::of(2), inner.center().y),
            egui::Align2::RIGHT_CENTER,
            relative_time(board.modified, now),
            egui::FontId::proportional(crate::theme::text::LABEL),
            palette.faint,
        );
        hairline(ui, palette);
    }

    star_button(ui, palette, board, Band::Boards, star_at, events);

    if response.double_clicked() || response.clicked() {
        events.push(UiEvent::Library(LibraryEvent::Open(board.path.clone())));
    }
    drag_to_space(ui, palette, board, &response, targets, events);
    context_menu(&response, palette, board, spaces, events);
}

/// Drag a board onto a space in the sidebar to file it there.
///
/// The gesture Miro has, and the one a folder implies: *put this in there*. The other
/// two routes — the row's own *Move to ▸* and Board ▸ Move to — stay, because a drag
/// across a full window is not always the shortest way, but neither of them is what a
/// hand reaches for with the sidebar already in view.
///
/// Nothing is filed here: this reports [`LibraryEvent::MoveToSpace`] with one space,
/// which is also what keeps a board in **at most one** — there is no shape of event
/// that could add it to a second.
fn drag_to_space(
    ui: &Ui,
    palette: Palette,
    board: &BoardCard,
    response: &Response,
    targets: &[(String, Rect)],
    events: &mut EventSink,
) {
    if !response.dragged() && !response.drag_stopped() {
        return;
    }
    let Some(pointer) = ui.ctx().pointer_interact_pos() else { return };
    let over = targets.iter().find(|(_, rect)| rect.contains(pointer));

    if response.drag_stopped() {
        // A drag that ends anywhere else is a drag the user thought better of. It is
        // not an error and it does not deserve a toast.
        if let Some((name, _)) = over {
            events.push(UiEvent::Library(LibraryEvent::MoveToSpace {
                path: board.path.clone(),
                space: Some(name.clone()),
            }));
            note_landing(ui.ctx(), name);
        }
        return;
    }

    ui.ctx().set_cursor_icon(if over.is_some() {
        CursorIcon::Copy
    } else {
        CursorIcon::Grabbing
    });
    // The board itself, carried. The *highlight* is still painted by the row — see
    // `drop_background` for why it cannot be painted from here — but the proxy is a
    // different thing and belongs on a layer above everything: it is the object in the
    // user's hand, and it is supposed to pass over what it is being carried across.
    drag_proxy(ui, palette, &board.title, pointer, over.is_some());
}

/// The card in flight: a small rectangle under the pointer carrying the board's name.
///
/// *"when i start to drag it … a small rectangle will move"*. Before this, a drag was
/// reported only by the cursor changing shape and by the target row lighting up — so
/// picking a card up looked identical to failing to pick it up, and the two are exactly
/// the states a drag has to tell apart.
///
/// Offset **down and to the right** of the pointer rather than centred on it, so the
/// proxy never covers the row being aimed at. That is the same failure the drop
/// highlight had (*"the name of the space dissapears"*) arriving by a different route,
/// and an object carried on top of its own target is the one place a foreground layer
/// is still wrong.
///
/// Its border says what will happen: the drop colour over a space, a plain hairline
/// anywhere else. One glance, no reading.
fn drag_proxy(ui: &Ui, palette: Palette, title: &str, pointer: egui::Pos2, over_space: bool) {
    let size = vec2(space::of(34), space::of(7));
    let rect = Rect::from_min_size(pointer + vec2(space::of(3), space::of(3)), size);
    // Above every panel, because the grid it came from is clipped to the central panel
    // and a proxy that vanished at the sidebar's edge would disappear halfway to its
    // target — which is precisely where it is most needed.
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        Id::new("velm-library-drag-proxy"),
    ));
    let corner = CornerRadius::same(radius::SMALL);
    painter.rect_filled(rect, corner, palette.surface);
    painter.rect_stroke(
        rect,
        corner,
        Stroke::new(
            if over_space { palette.hairline_width() * 2.0 } else { palette.hairline_width() },
            if over_space { palette.info } else { palette.border },
        ),
        StrokeKind::Inside,
    );
    // Cut to the room it has, with an ellipsis rather than mid-letter — and measured
    // rather than counted in characters, because a proportional face makes "WWWW" four
    // times the width of "iiii". One line by construction: a title that grew the
    // rectangle would stop it being the *small* one the user asked for.
    let inner = rect.shrink2(vec2(space::of(2), 0.0));
    let mut job = egui::text::LayoutJob::simple_singleline(
        title.to_owned(),
        egui::FontId::proportional(crate::theme::text::LABEL),
        palette.text,
    );
    job.wrap = egui::text::TextWrapping {
        max_width: inner.width(),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(inner.left(), inner.center().y - galley.rect.height() / 2.0),
        galley,
        palette.text,
    );
}

fn context_menu(
    response: &Response,
    palette: Palette,
    board: &BoardCard,
    spaces: &[Space],
    events: &mut EventSink,
) {
    Popup::context_menu(response).show(|ui| {
        ui.set_min_width(space::of(38));
        ui.set_max_width(space::of(72));
        // A board in the trash gets two verbs and no others. Rename, Star, Move to and
        // Duplicate all describe a board you have; offering them on one you have deleted
        // asks the user to decide whether the deletion was real.
        if board.deleted.is_some() {
            if ui.button("Restore").clicked() {
                events.push(UiEvent::Library(LibraryEvent::Restore(board.path.clone())));
            }
            hairline(ui, palette);
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("Delete permanently…").color(palette.danger),
                ))
                .on_hover_text("This is the only thing in Velm that removes a board file")
                .clicked()
            {
                events.push(UiEvent::Library(LibraryEvent::Purge(board.path.clone())));
            }
            return;
        }
        if ui.button("Open").clicked() {
            events.push(UiEvent::Library(LibraryEvent::Open(board.path.clone())));
        }
        if ui.button("Rename…").clicked() {
            events.push(UiEvent::Library(LibraryEvent::Rename {
                path: board.path.clone(),
                title: board.title.clone(),
            }));
        }
        if ui.button(if board.starred { "Unstar" } else { "Star" }).clicked() {
            events.push(UiEvent::Library(LibraryEvent::SetStarred {
                path: board.path.clone(),
                starred: !board.starred,
            }));
        }
        move_to_menu(ui, palette, board, spaces, events);
        if ui.button("Duplicate").clicked() {
            events.push(UiEvent::Library(LibraryEvent::Duplicate(board.path.clone())));
        }
        hairline(ui, palette);
        if ui
            .add(egui::Button::new(egui::RichText::new("Delete…").color(palette.danger)))
            .clicked()
        {
            events.push(UiEvent::Library(LibraryEvent::Delete(board.path.clone())));
        }
    });
}

/// *Move to ▸* — the same submenu the Board menu carries, on the row the user is
/// already pointing at.
fn move_to_menu(
    ui: &mut Ui,
    palette: Palette,
    board: &BoardCard,
    spaces: &[Space],
    events: &mut EventSink,
) {
    let current = spaces.iter().find(|s| s.contains(&board.path)).map(|s| s.name.as_str());
    let button = egui::Button::new("Move to");
    if spaces.is_empty() {
        ui.add_enabled(false, button)
            .on_disabled_hover_text(crate::command::reason::NO_SPACES);
        return;
    }

    let mut chosen: Option<Option<String>> = None;
    let (response, _) = egui::containers::menu::SubMenuButton::from_button(button).ui(ui, |ui| {
        ui.set_min_width(space::of(34));
        ui.set_max_width(space::of(72));
        for space in spaces {
            let row = ui.button(space.name.as_str());
            if current == Some(space.name.as_str()) {
                let box_ = Rect::from_center_size(
                    egui::pos2(row.rect.right() - space::of(2), row.rect.center().y),
                    Vec2::splat(space::of(3)),
                );
                Icon::Check.paint(&ui.painter().clone(), box_, palette.accent, ICON_STROKE);
            }
            if row.clicked() {
                chosen = Some(Some(space.name.clone()));
            }
        }
        hairline(ui, palette);
        if ui
            .add_enabled(current.is_some(), egui::Button::new("No folder"))
            .on_disabled_hover_text("This board is not in a folder")
            .clicked()
        {
            chosen = Some(None);
        }
    });
    // Drawn rather than set as text, for the reason `crate::menu` gives: egui's own
    // disclosure arrow is a dingbat from the emoji fallback font.
    let box_ = Rect::from_center_size(
        egui::pos2(response.rect.right() - space::of(2), response.rect.center().y),
        Vec2::splat(space::of(3)),
    );
    Icon::ChevronRight.paint(&ui.painter().clone(), box_, palette.muted, ICON_STROKE);

    if let Some(space) = chosen {
        events.push(UiEvent::Library(LibraryEvent::MoveToSpace {
            path: board.path.clone(),
            space,
        }));
    }
}

fn preview(
    ui: &mut Ui,
    palette: Palette,
    board: &BoardCard,
    band: Band,
    events: &mut EventSink,
) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), space::of(27)), Sense::hover());
    let corner = CornerRadius::same(radius::SMALL);
    // **The canvas colour, not `raised`** — this slot is a picture of a board, so the
    // letterbox beside a board that does not fit it has to be the colour a board is.
    //
    // It was `raised`, and that was correct until feedback 27: `raised` *was* `PEARL`, and
    // `PEARL` *was* the canvas, so the bars and the thumbnail's own background were the same
    // value by coincidence. Splitting `PAPER` off for the near-white board moved one of them
    // and left the other behind, so every card drew a `#F2F2F2` picture inside `#EBEEF0`
    // bars — *"the background of the screenshotrs are partially the new bakground color
    // p[artially not"*. Nothing was stale; the two halves were reading different tokens.
    //
    // A board that has chosen its **own** background colour still shows bars in the
    // palette's canvas rather than in that colour. Narrower and pre-existing: fixing it
    // needs the colour on `BoardCard`, which means reading each board's document to list the
    // library — the one thing `docs/01-architecture.md` §5's index exists to avoid.
    ui.painter().rect_filled(rect, corner, palette.canvas);

    match board.thumbnail {
        Some(thumb) => {
            // Fit rather than fill: a board is any aspect ratio at all, and cropping
            // a wide board to a card-shaped hole hides the part that identifies it.
            let (tw, th) = (thumb.size[0] as f32, thumb.size[1] as f32);
            let scale = (rect.width() / tw).min(rect.height() / th).min(1.0);
            let fitted = Rect::from_center_size(rect.center(), vec2(tw * scale, th * scale));
            let mut mesh = egui::Mesh::with_texture(thumb.texture);
            mesh.add_rect_with_uv(
                fitted,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                // Unmodulated: this is a multiplier of one, not a colour.
                crate::theme::UNTINTED,
            );
            ui.painter().add(egui::Shape::mesh(mesh));
        }
        None => {
            // No thumbnail yet — a board saved before the renderer existed, or one
            // whose preview is still being generated. The mark stands in, in the
            // faint ink, because it is already the shape of an empty frame.
            let glyph = Rect::from_center_size(rect.center(), Vec2::splat(space::of(7)));
            crate::mark::paint_mono(&ui.painter().clone(), glyph, palette.border);
        }
    }
    ui.painter().rect_stroke(rect, corner, palette.hairline_stroke(), StrokeKind::Inside);

    let star_at = Rect::from_min_size(
        egui::pos2(rect.right() - ROW_BUTTON - space::UNIT, rect.top() + space::UNIT),
        Vec2::splat(ROW_BUTTON),
    );
    star_button(ui, palette, board, band, star_at, events);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Context;
    use std::time::Duration;

    /// A card's preview slot is filled with the colour a **board** is, so the letterbox
    /// beside a board that does not fit the slot matches the thumbnail's own background.
    ///
    /// Asserted as an identity between two palette tokens rather than against a hex, because
    /// the bug this replaces was two tokens that used to be the same value drifting apart:
    /// `raised` and `canvas` were both `PEARL` until the near-white board split `PAPER` off,
    /// and nothing said they had to stay together — because they did not. What has to stay
    /// together is the slot and the canvas, which is what this says.
    #[test]
    fn a_preview_slot_is_the_colour_a_board_is() {
        // Light only, deliberately: `PAPER` is `PEARL` in the frozen dark cut — the split
        // exists because the *light* board went near-white — so asserting the two differ
        // there would be asserting something the design says is false.
        assert_ne!(
            Palette::LIGHT.canvas,
            Palette::LIGHT.raised,
            "if these are equal again the assertion below stops meaning anything"
        );
        // The slot's fill, read back off the painted frame rather than trusted: the
        // interaction tests exist because a constant in a test can agree with a constant in
        // the source while the widget paints something else entirely.
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let palette = Palette::LIGHT;
        let card = card_at("garage", 60, SystemTime::UNIX_EPOCH);
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut events = EventSink::default();
            preview(ui, palette, &card, Band::Boards, &mut events);
        });
        let filled: Vec<egui::Color32> = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) if rect.rect.width() > 40.0 => Some(rect.fill),
                _ => None,
            })
            .collect();
        assert!(
            filled.contains(&palette.canvas),
            "the preview slot is filled with {:?}, none of which is the canvas {:?}",
            filled,
            palette.canvas
        );
        assert!(
            !filled.contains(&palette.raised),
            "the slot is still being filled with `raised`, which is no longer the board's colour"
        );
    }

    fn card_at(title: &str, seconds_ago: u64, now: SystemTime) -> BoardCard {
        BoardCard {
            path: PathBuf::from(format!("/boards/{title}.vellum")),
            title: title.to_owned(),
            item_count: 596,
            modified: now - Duration::from_secs(seconds_ago),
            starred: false,
            deleted: None,
            thumbnail: None,
        }
    }

    /// *"on the top there would be one row that is the most recent boards that i opened
    /// and the amount of boards on that row will depend on the width of the viewport."*
    ///
    /// The top band is **exactly** one row — not a fixed number of boards. A count chosen
    /// in advance is short of the window on a wide one and wrapped onto a second row on a
    /// narrow one, and a row that wraps is not a row.
    #[test]
    fn the_top_band_is_one_row_however_wide_the_window_is() {
        let gutter = space::of(4);
        // One card and one gutter's worth of slack: four cards need four widths and three
        // gutters, so the fourth fits in less than four full pitches.
        let four = CARD_WIDTH * 4.0 + gutter * 3.0;
        assert_eq!(columns_that_fit(four, gutter), 4);
        assert_eq!(columns_that_fit(four - 1.0, gutter), 3, "a card that does not fit is a row down");
        assert_eq!(columns_that_fit(four + gutter, gutter), 4, "a gutter alone is not a card");

        // Never zero. A window narrower than one card still draws the card, clipped, rather
        // than drawing nothing and looking broken.
        assert_eq!(columns_that_fit(10.0, gutter), 1);
        assert_eq!(columns_that_fit(0.0, gutter), 1);
    }

    /// The three bands: the most recent fill one row, **every** starred board is under
    /// Pinned whether or not it is also up there, and everything else follows.
    ///
    /// The repeat is the point and this test used to assert the opposite. *"the boards that
    /// are starred, even if they are under recently opened, if they are starred they should
    /// still be under pinned."* Pinned is where you go to find what you pinned; one missing
    /// because it happens to be recent makes the whole section untrustworthy.
    #[test]
    fn a_starred_board_is_pinned_even_when_it_is_also_the_most_recent() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut boards: Vec<BoardCard> = (0..7)
            .map(|n| card_at(&format!("b{n}"), n * 60, now))
            .collect();
        // The newest board is starred, and so is one well down the list.
        boards[0].starred = true;
        boards[5].starred = true;
        let visible: Vec<&BoardCard> = boards.iter().collect();

        let bands = recent_bands(&visible, 3);
        let names = |band: &[&BoardCard]| -> Vec<String> {
            band.iter().map(|b| b.title.clone()).collect()
        };
        assert_eq!(names(&bands.first_row), ["b0", "b1", "b2"], "one row, most recent first");
        assert_eq!(
            names(&bands.pinned),
            ["b0", "b5"],
            "b0 is starred *and* in the top row, and belongs in both",
        );
        assert_eq!(names(&bands.rest), ["b3", "b4", "b6"], "still in recency order");

        // Every board is somewhere, and the tail repeats neither band above it — so the
        // only board drawn twice is a starred one, and only across those two sections.
        let mut seen: Vec<&str> = bands
            .first_row
            .iter()
            .chain(&bands.pinned)
            .chain(&bands.rest)
            .map(|b| b.title.as_str())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), boards.len(), "a board went missing: {seen:?}");
        assert!(
            bands.rest.iter().all(|b| !b.starred),
            "a starred board is in the tail as well as in Pinned",
        );
    }

    /// A window wide enough for everything leaves the other two bands empty, and the page
    /// is then a single row — which is correct, not a bug to pad around.
    #[test]
    fn a_wide_window_puts_every_board_in_the_top_row() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let boards: Vec<BoardCard> =
            (0..3).map(|n| card_at(&format!("b{n}"), n * 60, now)).collect();
        let visible: Vec<&BoardCard> = boards.iter().collect();
        let bands = recent_bands(&visible, 10);
        assert_eq!(bands.first_row.len(), 3);
        assert!(bands.pinned.is_empty() && bands.rest.is_empty());
    }

    #[test]
    fn search_is_case_insensitive_and_an_empty_query_matches_everything() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let board = card_at("Site plan 530", 0, now);
        assert!(matches(&board, ""));
        assert!(matches(&board, "   "));
        assert!(matches(&board, "site"));
        assert!(matches(&board, "530"));
        assert!(!matches(&board, "roadmap"));
    }

    #[test]
    fn relative_times_step_through_the_units_and_singularise() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let at = |secs| relative_time(now - Duration::from_secs(secs), now);
        assert_eq!(at(0), "just now");
        assert_eq!(at(59), "just now");
        assert_eq!(at(60), "1 minute ago");
        assert_eq!(at(3_599), "59 minutes ago");
        assert_eq!(at(3_600), "1 hour ago");
        assert_eq!(at(86_400), "1 day ago");
        assert_eq!(at(2_592_000), "1 month ago");
        assert_eq!(at(31_536_000), "1 year ago");
    }

    /// A file copied from another machine can carry an mtime in the future; the
    /// library must not render "18446744073709551615 years ago".
    #[test]
    fn a_future_modification_time_reads_as_current() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(relative_time(now + Duration::from_secs(600), now), "just now");
    }

    #[test]
    fn item_counts_singularise() {
        assert_eq!(item_count_label(0), "0 items");
        assert_eq!(item_count_label(1), "1 item");
        assert_eq!(item_count_label(596), "596 items");
    }

    /// The scope filter is the whole of what makes Spaces first class rather than
    /// decorative, so it is tested without a window.
    #[test]
    fn a_space_scope_shows_only_the_boards_in_that_space() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let plan = card_at("Site plan", 900, now);
        let books = card_at("Reading", 200_000, now);
        let mut state = LibraryState {
            spaces: vec![
                Space::new("Cars", [plan.path.clone()]).pinned(),
                Space::new("Books", [books.path.clone()]),
            ],
            ..LibraryState::default()
        };

        state.scope = Scope::Recent;
        assert!(state.in_scope(&plan) && state.in_scope(&books));
        assert_eq!(state.title(), "Recent");

        state.scope = Scope::Space("Cars".to_owned());
        assert!(state.in_scope(&plan));
        assert!(!state.in_scope(&books));
        assert_eq!(state.title(), "Cars");
        assert_eq!(state.selected_space().map(|s| s.name.as_str()), Some("Cars"));

        // Pinning re-sorts the list under the selection. Held by index, this used to
        // move the filter onto whichever space had taken slot zero.
        state.spaces.swap(0, 1);
        assert!(state.in_scope(&plan), "the filter followed the space, not the slot");
        assert!(!state.in_scope(&books));
    }

    #[test]
    fn the_starred_scope_shows_only_starred_boards() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let mut plan = card_at("Site plan", 900, now);
        let books = card_at("Reading", 200_000, now);
        plan.starred = true;

        let state = LibraryState { scope: Scope::Starred, ..LibraryState::default() };
        assert!(state.in_scope(&plan));
        assert!(!state.in_scope(&books));
        assert_eq!(state.title(), "Starred");
        assert_eq!(state.nothing_here(), "No starred boards yet");
    }

    /// The app replaces the space list whenever the store changes underneath it. A
    /// scope naming a space that has gone must show nothing rather than quietly
    /// widening to everything, which would look like the filter had been ignored.
    ///
    /// And the boards it held are **still there**, unfiled: deleting a folder deletes
    /// the folder. That is the whole of the promise the delete confirmation makes, and
    /// it is checked here rather than trusted.
    #[test]
    fn deleting_a_space_leaves_its_boards_intact_and_unfiled() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let board = card_at("Site plan", 900, now);
        let mut state = LibraryState {
            spaces: vec![Space::new("Cars", [board.path.clone()])],
            scope: Scope::Space("Cars".to_owned()),
            ..LibraryState::default()
        };
        assert!(state.in_scope(&board));

        // What the app does when it answers `DeleteSpace`: the space goes, the board
        // does not.
        state.spaces.clear();
        assert!(!state.in_scope(&board), "the filter did not widen to everything");
        assert_eq!(state.selected_space(), None);
        assert_eq!(state.title(), "Cars");
        assert_eq!(state.nothing_here(), "Nothing in this folder yet");

        state.scope = Scope::All;
        assert!(state.in_scope(&board), "All boards always shows everything");
        state.scope = Scope::Recent;
        assert!(state.in_scope(&board));
    }

    /// `docs/04-ui-reference.md` §5's *↑improve*: the screen opens on what the user
    /// almost always wants, which is the board they were just in.
    #[test]
    fn the_library_opens_on_recent_in_a_grid() {
        let state = LibraryState::default();
        assert_eq!(state.scope, Scope::Recent);
        assert_eq!(state.layout, LayoutMode::Grid);
    }

    fn run(boards: &[BoardCard], state: &mut LibraryState) -> Vec<UiEvent> {
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut events = EventSink::default();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let settings = SettingsView {
                accent: crate::theme::Accent::Teal,
                glass_opacity: 160,
                transparency_blocked: false,
                link_previews: true,
                align_objects: true,
                snap_to_grid: false,
            };
            show(ui, Palette::LIGHT, state, boards, now, &settings, &mut events);
        });
        events.take()
    }

    /// The settings page holds **no boards**, whatever is on disk.
    ///
    /// The half that would fail silently: a scope that fell through to the grid would show
    /// every board in the library under a heading reading *Settings*, which looks like a
    /// filter that did not work rather than like a page that is missing. `in_scope` is the
    /// one place that is decided, so it is where it is asked.
    #[test]
    fn the_settings_page_holds_no_boards() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let mut starred = card_at("Site plan", 900, now);
        starred.starred = true;
        let boards = [starred, card_at("Roadmap", 200_000, now)];

        let settings = LibraryState { scope: Scope::Settings, ..LibraryState::default() };
        for board in &boards {
            assert!(!settings.in_scope(board), "a board reached the settings page");
        }
        // And it is not simply refusing everything: the same boards are in Recent.
        let recent = LibraryState::default();
        assert!(boards.iter().all(|board| recent.in_scope(board)));

        assert_eq!(settings.title(), "Settings");
        assert!(settings.selected_space().is_none(), "settings is not a folder");
    }

    /// Every tab of the settings page draws, and none of them emits anything untouched.
    ///
    /// Written over `SettingsTab::ALL` rather than over the one page there is, so a second
    /// page is covered the day it is added: each page is its own set of live controls, and a
    /// control that fired on its own would rewrite a preference every frame the page was up.
    #[test]
    fn every_settings_tab_draws_and_emits_nothing_untouched() {
        for tab in SettingsTab::ALL {
            let mut state = LibraryState {
                scope: Scope::Settings,
                settings_tab: tab,
                ..LibraryState::default()
            };
            let events = run(&[], &mut state);
            assert!(events.is_empty(), "the {tab:?} tab emitted {events:?} on its own");
            assert_eq!(state.settings_tab, tab, "the {tab:?} tab switched by itself");
            assert!(!tab.label().is_empty() && !tab.subtitle().is_empty(), "{tab:?}");
        }
    }

    /// All three account states draw, and none of them signs anybody in or out on its own.
    ///
    /// The Account page is the first one in this crate with a *submit*, so the failure worth
    /// pinning is a button that fires from being drawn. `SigningIn` and `SignedIn` are the
    /// two states `--show settings:Account` cannot photograph, which makes this the only
    /// automated cover they have.
    #[test]
    fn every_account_state_draws_and_signs_nobody_in_by_itself() {
        for state in [AccountState::SignedOut, AccountState::SigningIn, AccountState::SignedIn] {
            let mut library = LibraryState {
                scope: Scope::Settings,
                settings_tab: SettingsTab::Account,
                account: AccountFields {
                    server: "boards.example.com".to_owned(),
                    username: "sam".to_owned(),
                    password: "a password".to_owned(),
                    state,
                    signed_in_as: "sam".to_owned(),
                    signed_in_to: "https://boards.example.com/".to_owned(),
                    message: Some("That username and password do not match.".to_owned()),
                },
                ..LibraryState::default()
            };
            let events = run(&[], &mut library);
            assert!(events.is_empty(), "the {state:?} page emitted {events:?} on its own");
            assert_eq!(library.account.state, state, "the page changed its own state");
            assert_eq!(
                library.account.password, "a password",
                "the page cleared the password without a click"
            );
        }
    }

    /// `LibraryState` derives `Debug` and now holds a password, so the redaction has to hold
    /// here as well as on `Secret`. Same shape as `event::tests`, one level up.
    #[test]
    fn the_library_state_does_not_print_a_password() {
        let library = LibraryState {
            account: AccountFields {
                password: "hunter2-and-a-half".to_owned(),
                username: "sam".to_owned(),
                ..AccountFields::default()
            },
            ..LibraryState::default()
        };
        let printed = format!("{library:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("redacted"), "{printed}");
        assert!(printed.contains("sam"), "the rest is still loggable: {printed}");
    }

    /// It draws, and it emits nothing untouched.
    ///
    /// Worth its own test beyond `every_scope_and_layout_draws`: this page is the only one
    /// in the library built out of live controls — a slider, swatches, toggles — and a
    /// control that fired on its own would rewrite a preference every frame the page is up.
    #[test]
    fn the_settings_page_draws_and_emits_nothing_untouched() {
        let mut state = LibraryState { scope: Scope::Settings, ..LibraryState::default() };
        let events = run(&[], &mut state);
        assert!(events.is_empty(), "the settings page emitted {events:?} on its own");
    }

    fn with_search(search: &str) -> LibraryState {
        LibraryState { search: search.to_owned(), ..LibraryState::default() }
    }

    #[test]
    fn the_library_draws_empty_populated_and_filtered_without_emitting_anything() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let boards = [card_at("Site plan", 900, now), card_at("Roadmap", 200_000, now)];
        assert!(run(&[], &mut with_search("")).is_empty(), "no boards at all");
        assert!(run(&boards, &mut with_search("")).is_empty(), "two boards");
        assert!(run(&boards, &mut with_search("site")).is_empty(), "filtered to one");
        assert!(run(&boards, &mut with_search("nothing here")).is_empty(), "filtered to none");
    }

    /// Every scope and both layouts have to draw. The list view in particular paints
    /// by hand rather than through widgets, so nothing else would catch it.
    #[test]
    fn every_scope_and_layout_draws() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let mut boards = [card_at("Site plan", 900, now), card_at("Roadmap", 200_000, now)];
        boards[0].starred = true;
        let spaces = vec![
            Space::new("Cars", [boards[0].path.clone()]).pinned(),
            Space::new("Archive", []),
        ];
        for scope in [
            Scope::Recent,
            Scope::All,
            Scope::Starred,
            Scope::Space("Cars".to_owned()),
            Scope::Space("Archive".to_owned()),
            Scope::Space("Deleted".to_owned()),
            // The settings page draws its own body and never reaches the grid. In this list
            // because it is a scope, and a scope that panicked on one layout would otherwise
            // be found by a user rather than by a test.
            Scope::Settings,
        ] {
            for layout in [LayoutMode::Grid, LayoutMode::List] {
                let mut state = LibraryState {
                    spaces: spaces.clone(),
                    scope: scope.clone(),
                    layout,
                    ..LibraryState::default()
                };
                let events = run(&boards, &mut state);
                assert!(events.is_empty(), "{scope:?}/{layout:?} emitted {events:?}");
            }
        }
    }

    #[test]
    fn a_card_with_a_thumbnail_renders() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
        let mut board = card_at("Site plan", 900, now);
        board.thumbnail = Some(Thumbnail { texture: TextureId::Managed(0), size: [1920, 640] });
        assert!(run(&[board], &mut with_search("")).is_empty());
    }

    /// The instructions the user asked for, in the order they have to be followed.
    ///
    /// The fourth step names the archives folder. It is asserted rather than assumed
    /// because it is the **only** place in the app that answers "where do I put a
    /// `.rtb`" — there is no file dialog anywhere in the build — so losing it silently
    /// would leave a user holding a backup with nothing to do with it.
    #[test]
    fn the_import_instructions_are_the_documented_steps() {
        assert_eq!(IMPORT_STEPS.len(), 4);
        assert!(IMPORT_STEPS[0].contains("Miro"));
        assert!(IMPORT_STEPS[1].contains("Cmd+A") && IMPORT_STEPS[1].contains("Cmd+C"));
        assert!(IMPORT_STEPS[2].contains("Cmd+V"));
        assert!(
            IMPORT_STEPS[3].contains(".rtb") && IMPORT_STEPS[3].contains("archives"),
            "the last step must name the folder: {}",
            IMPORT_STEPS[3]
        );
    }
}

// ----- the settings page ---------------------------------------------------------
//
// *"add a settings page in the home page so that we can do and put all of the settings that
// are needed to live in the dedicated settings page there."*

/// Everything the settings page reads.
///
/// # Why its own struct rather than [`crate::menu::MenuHeader`]
///
/// `MenuHeader` borrows `Chrome::library.spaces`, and the library panel needs
/// `&mut Chrome::library` — the two cannot overlap, which is exactly what the comment above
/// `library::show`'s call site already records. Every field here is `Copy`, so the settings
/// page can be drawn inside the library's own borrow without cloning the spaces into a header
/// sixty times a second to get around it.
#[derive(Debug, Clone, Copy)]
pub struct SettingsView {
    pub accent: crate::theme::Accent,
    /// How see-through the floating chrome is, 0–255.
    pub glass_opacity: u8,
    /// Whether the OS's Reduce Transparency is on, in which case the slider is disabled
    /// rather than silently doing nothing.
    pub transparency_blocked: bool,
    pub link_previews: bool,
    pub align_objects: bool,
    pub snap_to_grid: bool,
}

/// The settings page: every preference in the application, on one screen.
///
/// # Every control emits the same event its menu row does
///
/// That is the rule that makes a second route safe. Preferences ▸ Transparency and the
/// slider here both push `UiEvent::TransparencyChanged`; the accent rows here and there both
/// push `AccentChanged`. Nothing on this page knows how a setting is *stored*, so the two
/// routes cannot come to disagree about what a setting means — which is the failure a second
/// copy of a control normally produces.
///
/// # It says why, not just what
///
/// Each switch carries the sentence its menu row carries as a tooltip, drawn **under** the
/// control rather than behind a hover. A page you are reading has room for the reason; a
/// menu row does not, which is why the menu keeps the hover. A switch whose cost is invisible
/// from the switch is the version of that failure nobody notices, because the control works
/// perfectly and the consequence arrives an hour later.
fn settings(
    ui: &mut Ui,
    palette: Palette,
    tab: &mut SettingsTab,
    view: &SettingsView,
    account: &mut AccountFields,
    events: &mut EventSink,
) {
    // The strip, then one line saying what this page is for. Above the scroll area, so it
    // stays put while a long page moves under it — a tab strip that scrolls away is one you
    // have to go back up to use.
    //
    // **Skipped entirely while there is one page.** A strip with a single tab is a control
    // whose only possible outcome is the state you are already in; drawing it costs a row and
    // invites the click that proves it does nothing. The subtitle below is what was carrying
    // the information, so it stays either way.
    if SettingsTab::ALL.len() > 1 {
        ui.horizontal(|ui| {
            for page in SettingsTab::ALL {
                if ui.selectable_label(*tab == page, page.label()).clicked() {
                    *tab = page;
                }
            }
        });
        ui.add_space(space::UNIT);
    }
    ui.label(
        egui::RichText::new(tab.subtitle())
            .color(palette.muted)
            .size(crate::theme::text::LABEL),
    );
    ui.add_space(space::of(2));
    hairline(ui, palette);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(space::of(140));
        match tab {
            SettingsTab::General => general_settings(ui, palette, view, events),
            SettingsTab::Account => account_settings(ui, palette, account, events),
        }
    });
}

/// Settings ▸ Account: the server, who you are on it, and the button that joins the two.
///
/// # Signing in is optional and this page says so
///
/// *"update the app so that the signing in is optional in the settings and i have to give a
/// url sign in and password … so i want it to connect to the correct server not just any
/// server"*. An application with nothing typed here behaves exactly as it did: no thread, no
/// socket and no request. Nothing on this page is required to open, edit or save a board.
///
/// # Every state is drawn, including the two that are waiting
///
/// A form whose only feedback is that nothing happened is a form people press twice. So a
/// request in flight disables the fields and says *Signing in*, and a refusal puts one
/// sentence under the button. The sentences come from the app, because the app is the half
/// that saw the answer.
fn account_settings(
    ui: &mut Ui,
    palette: Palette,
    account: &mut AccountFields,
    events: &mut EventSink,
) {
    let editable = account.state == AccountState::SignedOut;

    section(ui, palette, "Server");
    if account.state == AccountState::SignedIn {
        // Static text, not a field. The address a session was minted against cannot be
        // edited under it: the cookie belongs to that host and to no other.
        setting(
            ui,
            palette,
            "Address",
            "Your boards sync here. Sign out to use a different server.",
            |ui| {
                ui.label(
                    egui::RichText::new(account.signed_in_to.as_str()).color(palette.muted),
                );
            },
        );
    } else {
        setting(
            ui,
            palette,
            "Address",
            "The web address of your Velm server. Ask the person who runs it. Start it with \
             https, or your password crosses the network as plain text.",
            |ui| {
                ui.add_enabled_ui(editable, |ui| {
                    text_field(
                        ui,
                        palette,
                        Id::new("vellum-account-server"),
                        &mut account.server,
                        "boards.example.com",
                        false,
                    );
                });
            },
        );
    }

    section(ui, palette, "Sign in");
    if account.state == AccountState::SignedIn {
        setting(
            ui,
            palette,
            "Signed in as",
            "Only the boards this account owns or was shared will sync.",
            |ui| {
                ui.label(egui::RichText::new(account.signed_in_as.as_str()).color(palette.text));
            },
        );
        if ui.button("Sign out").clicked() {
            events.push(UiEvent::SignOutRequested);
        }
    } else {
        setting(ui, palette, "Username", "The name you use on that server.", |ui| {
            ui.add_enabled_ui(editable, |ui| {
                text_field(
                    ui,
                    palette,
                    Id::new("vellum-account-username"),
                    &mut account.username,
                    "Username",
                    false,
                );
            });
        });
        // Enter on the password field submits, the way it does in every sign-in form.
        // Latched out of the closure because `setting` owns the layout the field is drawn in.
        let mut entered = false;
        setting(
            ui,
            palette,
            "Password",
            "Velm sends it to your server once, to start a session. It is written to no \
             file on this machine.",
            |ui| {
                ui.add_enabled_ui(editable, |ui| {
                    let field = text_field(
                        ui,
                        palette,
                        Id::new("vellum-account-password"),
                        &mut account.password,
                        "Password",
                        true,
                    );
                    // **Read before anything requests focus.** Nothing on this page does,
                    // and that is why this works — `TextEdit` signals Enter only by
                    // surrendering focus, and a `request_focus` above this line would take it
                    // back synchronously and make this false for ever. The rename dialog
                    // carries the long version; `text_field`'s own doc carries the rule.
                    entered = field.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                });
            },
        );

        let complete = !account.server.trim().is_empty()
            && !account.username.trim().is_empty()
            && !account.password.is_empty();
        let label = if account.state == AccountState::SigningIn { "Signing in…" } else { "Sign in" };
        let pressed = ui.add_enabled(editable && complete, egui::Button::new(label)).clicked();
        if (pressed || entered) && editable && complete {
            // **`take`, not `clone`.** The field is cleared on the same frame the event is
            // pushed, which is the whole of what this page can honestly promise about the
            // password: it stops being on screen and stops being one of the copies this
            // crate holds. `Secret`'s own doc lists the copies that remain.
            let server = account.server.trim().to_owned();
            let username = account.username.trim().to_owned();
            let password = Secret::new(std::mem::take(&mut account.password));
            events.push(UiEvent::SignInRequested { server, username, password });
        }
    }

    if let Some(message) = account.message.as_deref() {
        ui.add_space(space::UNIT);
        ui.label(
            egui::RichText::new(message).color(palette.muted).size(crate::theme::text::LABEL),
        );
    }

    ui.add_space(space::of(3));
    ui.label(
        egui::RichText::new(
            "To make an account, open your server's sign-in page in a browser. You need a \
             code from the person who runs the server.",
        )
        .color(palette.faint)
        .size(crate::theme::text::LABEL),
    );

    ui.add_space(space::of(8));
}

/// Appearance and board behaviour — the application's own settings.
fn general_settings(
    ui: &mut Ui,
    palette: Palette,
    view: &SettingsView,
    events: &mut EventSink,
) {
    {
        section(ui, palette, "Appearance");
        setting(
            ui,
            palette,
            "Accent colour",
            "Selection, the active tool and the app's own mark. Each has been checked for \
             contrast against the two surfaces it lands on.",
            |ui| {
                // Reversed for `choices`' reason — the enclosing layout runs right to left,
                // so Teal · Red · Blue was drawn Blue · Red · Teal. Not routed through
                // `choices` because a swatch is painted rather than labelled.
                ui.horizontal(|ui| {
                    for accent in crate::theme::Accent::ALL.into_iter().rev() {
                        // A swatch drawn in the colour it names: "Teal" and "Blue" are two
                        // words to somebody who has not seen either.
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(space::of(9), space::of(5)),
                            egui::Sense::click(),
                        );
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(radius::SMALL),
                            accent.swatch(),
                        );
                        if accent == view.accent {
                            ui.painter().rect_stroke(
                                rect.expand(2.0),
                                CornerRadius::same(radius::SMALL + 2),
                                egui::Stroke::new(2.0, palette.text),
                                egui::StrokeKind::Middle,
                            );
                        }
                        if response.on_hover_text(accent.label()).clicked() {
                            events.push(UiEvent::AccentChanged(accent));
                        }
                    }
                });
            },
        );
        setting(
            ui,
            palette,
            "Transparency",
            "How see-through the floating panels are. It never beats the system's own \
             Reduce Transparency, and it stops short of invisible.",
            |ui| {
                let mut opacity = view.glass_opacity;
                let slider = ui.add_enabled(
                    !view.transparency_blocked,
                    egui::Slider::new(&mut opacity, 90..=255).show_value(false),
                );
                if view.transparency_blocked {
                    slider.on_disabled_hover_text(
                        "Your system has Reduce Transparency turned on, which wins.",
                    );
                // On release, not per frame: `Library::persist` writes the sidecar
                // synchronously, and a drag would write it sixty times a second.
                } else if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    events.push(UiEvent::GlassOpacityChanged(opacity));
                }
            },
        );

        section(ui, palette, "Board");
        switch(
            ui,
            palette,
            "Align objects",
            "Miro's relative snapping: blue guides that suggest alignments and equal \
             spacing as you drag. Loose rather than strict — hold ⌘ to suspend it \
             mid-drag.",
            view.align_objects,
            |_| events.command(crate::command::Command::ToggleAlignObjects),
        );
        switch(
            ui,
            palette,
            "Snap to grid",
            "Lands moves, resizes and new items on the grid lines you can actually see. \
             Strict, which is why it is a separate switch and off by default.",
            view.snap_to_grid,
            |_| events.command(crate::command::Command::SnapToGrid),
        );
        switch(
            ui,
            palette,
            "Fetch link previews",
            "Pasted links fetch their title, icon and picture. Only the cards on screen, \
             three requests a frame, and never a card you have edited by hand.",
            view.link_previews,
            |_| events.command(crate::command::Command::ToggleLinkPreviews),
        );

        ui.add_space(space::of(8));
    }
}

/// A settings band's name.
fn section(ui: &mut Ui, palette: Palette, title: &str) {
    ui.add_space(space::of(5));
    ui.label(
        egui::RichText::new(title)
            .size(crate::theme::text::LABEL)
            .color(palette.muted)
            .strong(),
    );
    ui.add_space(space::UNIT);
    hairline(ui, palette);
    ui.add_space(space::of(2));
}

/// One setting: its name, its control, and the sentence saying what it does.
///
/// The sentence is **drawn**, not hovered. A menu row has no room for it and hides it behind
/// a hover; a page has room, and the whole reason this page exists is that a switch whose
/// consequence is invisible is one people turn on and then report as a defect.
fn setting(
    ui: &mut Ui,
    palette: Palette,
    name: &str,
    why: &str,
    control: impl FnOnce(&mut Ui),
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(name).color(palette.text).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
    ui.label(
        egui::RichText::new(why).color(palette.muted).size(crate::theme::text::LABEL),
    );
    ui.add_space(space::of(3));
}

/// A setting whose control is one switch.
fn switch(
    ui: &mut Ui,
    palette: Palette,
    name: &str,
    why: &str,
    on: bool,
    toggle: impl FnOnce(bool),
) {
    let mut value = on;
    setting(ui, palette, name, why, |ui| {
        // A labelled toggle rather than a bare checkbox: the label says which state the
        // switch is *in*, which a tick does not on a page where several sit together.
        if ui.selectable_label(value, if value { "On" } else { "Off" }).clicked() {
            value = !value;
        }
    });
    if value != on {
        toggle(value);
    }
}
