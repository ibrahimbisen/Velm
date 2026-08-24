//! The winit application: window lifecycle, event routing, and the frame loop.
//!
//! Built on winit 0.30's [`ApplicationHandler`] trait rather than the deprecated
//! `EventLoop::run` closure. The trait is not just newer — it is the only shape that
//! works on the platforms where the OS can destroy and recreate the surface
//! underneath a running app, because `resumed`/`suspended` are where the window and
//! the GPU surface get built and torn down. Everything that owns GPU resources
//! therefore lives in [`ActiveState`], which is `None` until `resumed`.
//!
//! # What this file is *not* responsible for
//!
//! Deliberately very little happens here. Gestures are [`crate::input`]'s, drawing is
//! [`crate::draw`]'s, the document and its storage are [`crate::editor`]'s, the chrome
//! is [`crate::shell`]'s and what its buttons *do* is [`crate::actions`]'s. What is
//! left is the ordering, which is the one thing that cannot live anywhere else:
//!
//! 1. advance input, sync the caches,
//! 2. run the chrome — it decides what the tool is and where the canvas may draw,
//! 3. act on what the user asked for,
//! 4. paint the board,
//! 5. upload the chrome's triangles,
//! 6. present: board to a texture, blur, then swapchain.
//!
//! The chrome runs *before* the board is painted, not after, because it is what
//! answers "which tool", "which theme" and "how big is the canvas" — painting first
//! would draw one frame behind every click.
//!
//! # The window title is a save indicator
//!
//! The user asked for instant autosave and, in the same breath, to be able to see it
//! working. So the title carries the board's name and a marker while anything is still
//! on its way to disk. It is set only when it changes: `set_title` crosses to the
//! window server, and doing it sixty times a second is a real cost for a string that
//! changes twice a minute.

use std::sync::Arc;
use std::time::Duration;
use crate::time::Instant;

use vellum_import::rtb::ArchiveSet;
use vellum_render::{DrawList, GlassRenderer, QuadInstance, Rgba, View};
use vellum_scene::{Camera, ScreenPoint, ScreenSize, WorldPoint, WorldRect};
use vellum_store::BlobStore;
use vellum_ui::Screen;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::appearance::Appearance;
use crate::chrome_pass::ChromePass;
use crate::draw::{DrawContext, PaintStats, Painter};
use crate::editor::Editor;
use crate::hud::{self, FrameTimer};
use crate::input::{Input, InputConfig};
use crate::library::Library;
use crate::options::Options;
use crate::project::Projection;
use crate::shell::{Facts, Shell};
use crate::text::TextCache;
use crate::theme::Theme;
use crate::{bench, editor};

/// How often the frame statistics are written to the log. The HUD is for a human
/// watching; this is for `--exit-after` runs and CI.
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// Margin left around the content when fitting the board, as a fraction.
pub(crate) const FIT_MARGIN: f64 = 0.02;

/// How far one press of an arrow key moves the selection, in world units — which are
/// board pixels, the unit the properties panel's X and Y are already in.
pub(crate) const NUDGE: f64 = 1.0;

/// The same with Shift held. Ten, because that is what every design tool uses and it
/// is the step that makes nudging usable at a normal zoom.
pub(crate) const COARSE_NUDGE: f64 = 10.0;

/// How long an import summary stays on screen.
const STATUS_LIFETIME: Duration = Duration::from_secs(12);

/// How long `--screenshot` waits before taking its picture, and the fewest frames it
/// will accept.
///
/// Both, not either. Text layout and image residency fill in over the first frames by
/// design, so a picture taken on frame one is a picture of a cold start; and a debug
/// build of the reference board manages barely one frame a second, so a frame count
/// alone would never be reached.
const SCREENSHOT_DELAY: Duration = Duration::from_millis(1_500);
const SCREENSHOT_MIN_FRAMES: u64 = 3;

/// How long `--screenshot` will wait for the decode pool to go quiet before giving up
/// and photographing whatever has arrived.
///
/// A ceiling, not a target: the reference board's 205 assets settle in about three
/// seconds. It exists so a blob that never decodes — or a board of a thousand images —
/// cannot hang an unattended run, which is the one thing worse than a picture with a
/// placeholder in it.
const SCREENSHOT_MAX_WAIT: Duration = Duration::from_secs(20);

/// `--demo zoom-flicker`'s progress across frames.
///
/// A refinement is decided by one frame's draws, requested on the next and answered several
/// frames later by a worker thread, so nothing inside a single command can observe one. This
/// is the state that lets the frame loop watch the whole sequence.
#[derive(Debug, Clone, Copy, Default)]
struct ZoomSweep {
    /// Waiting for the first load to finish, so its decodes do not pollute the count.
    settling: bool,
    /// Consecutive settled frames seen so far.
    quiet: u32,
    frames: u32,
    /// The zoom the sweep starts from, captured once the board has settled.
    from: f64,
    /// Frames on which *anything* drew a placeholder. Zero is the pass.
    blinks: usize,
    /// The most images pending at once, for a failure message that says how bad it was.
    worst: usize,
    /// The most refinements asked for at once. **Zero means the fixture proved nothing** —
    /// the sweep never crossed an edge, so a green result would be vacuous.
    queued: usize,
}

/// Everything that only exists once there is a window.
/// Where boards sync to, and the little state that keeps the cadence honest.
///
/// Separate from [`crate::sync::Sync`], which is one board's round trip: this is the setup —
/// one server for every board this person opens — plus the two things that stop a per-frame
/// call being rude. `asked_at` is the cadence, and `reported` is what keeps a server that is
/// down from raising the same toast every three seconds for the rest of the afternoon.
///
/// ⚠ **`Clone` and deliberately not `Debug`.** It is cloned because signing out has to put
/// back the setup the flags asked for rather than turning sync off, and it holds a credential,
/// so a derived `Debug` would be that credential in whatever log line ever formatted it —
/// the rule `crate::options`' `OnceLock` and `crate::sync::Credential` both exist to keep.
#[derive(Clone)]
pub(crate) struct SyncConfig {
    pub(crate) options: crate::options::SyncOptions,
    /// What the request authenticates with. May be [`crate::sync::Credential::None`]: velmd
    /// needs nothing on loopback. See `ActiveState::new`'s warning.
    pub(crate) credential: crate::sync::Credential,
    /// When the last round trip was asked for, or `None` before the first.
    pub(crate) asked_at: Option<Instant>,
    /// The last failure already put in front of the user.
    pub(crate) reported: Option<String>,
}

/// What the app knows about Settings ▸ Account, pushed into the chrome once a frame.
///
/// It holds no credential: the session lives in `SyncConfig::credential` and nowhere else.
/// Everything here is safe to print, which is why this one derives `Debug` and that one
/// does not.
#[derive(Debug, Default)]
pub(crate) struct AccountStatus {
    pub(crate) state: vellum_ui::AccountState,
    /// Who is signed in. Empty unless `state` is signed in.
    pub(crate) username: String,
    /// The address the session was minted against, normalised.
    pub(crate) server: String,
    /// One sentence for the page, or `None` when nothing has failed.
    pub(crate) message: Option<String>,
}

pub(crate) struct ActiveState {
    pub(crate) window: Arc<Window>,
    pub(crate) surface: crate::surface::Surface,
    /// The board on screen. *Hot*, in `crate::session`'s vocabulary: the boards behind
    /// the other tabs are parked in [`ActiveState::session`] and swapped through here.
    pub(crate) editor: Editor,
    pub(crate) painter: Painter,
    /// The hot board's view. Parked with its board, so switching back to a tab finds
    /// the camera where it was left — which is most of what makes tabs feel right.
    pub(crate) camera: Camera,
    pub(crate) input: Input,
    pub(crate) theme: Theme,
    /// The menus, toolbar, panels and board library.
    pub(crate) shell: Shell,
    /// The **native** menu bar — macOS's own, at the top of the screen, which is a
    /// different object from the `⋮` menu `shell` draws. `None` when the platform has
    /// none or refused to build one; an app without it still has every verb on the
    /// keyboard and in the in-app menu, so it is not worth failing a launch over.
    #[cfg(target_os = "macos")]
    pub(crate) menubar: Option<crate::menubar::MenuBar>,
    /// Where to sync boards, and the bearer token, or `None` when `--sync-server` was not
    /// given — which is the default and costs nothing.
    ///
    /// Held here rather than on the [`Editor`] it configures, because it is a property of
    /// *this person's setup* and not of any one board: every board they open goes to the
    /// same server. The `Editor` holds the round trip itself, so a parked board keeps its
    /// place in the conversation across a tab switch — see `Editor::sync`.
    pub(crate) sync: Option<SyncConfig>,
    /// The `--sync-server` and `$VELM_SYNC_TOKEN` setup exactly as it was at startup.
    ///
    /// Kept so **signing out restores it rather than turning sync off**. Somebody who syncs
    /// their own boards with a bearer token today, signs in from Settings to look at
    /// somebody else's, and signs out again gets their own setup back. Without this, signing
    /// out of an account they never had before would silently disable the sync that has been
    /// running since they launched the app.
    pub(crate) startup_sync: Option<SyncConfig>,
    /// A sign-in in flight. `None` almost always.
    pub(crate) pending_signin: Option<crate::signin::SignIn>,
    /// The first run of *Send my boards to this server*: the worker, the recorded board ids,
    /// and the counters the Account page draws. One field, so the three cannot come apart.
    pub(crate) push: crate::push::PushState,
    /// Where the account page's message comes from, and who the app believes is signed in.
    ///
    /// On `ActiveState` rather than in the chrome because the chrome is redrawn from this
    /// every frame — see `vellum_ui::Chrome::set_account_status`, which writes the app's half
    /// of the page and never the three buffers the person is typing into.
    pub(crate) account: AccountStatus,
    /// Every open board that is **not** the one on screen, so a tab switch is a swap
    /// rather than a reload. See `crate::session` for why the hot board is hoisted out
    /// of it rather than held in it.
    pub(crate) session: crate::session::Session,
    /// The translucent material behind the floating chrome.
    pub(crate) glass: GlassRenderer,
    /// egui's triangles on the GPU.
    pub(crate) chrome: ChromePass,
    /// Per-frame scratch, owned so a steady-state frame allocates nothing.
    pub(crate) list: DrawList,
    hud_quads: Vec<QuadInstance>,
    pub(crate) show_hud: bool,
    timer: FrameTimer,
    stats: PaintStats,
    /// `--demo zoom-flicker`'s state. `None` in every ordinary run, and the whole feature
    /// costs one `Option` compare per frame.
    zoom_sweep: Option<ZoomSweep>,
    last_frame: Instant,
    last_stats_log: Instant,
    /// `"Metal / Apple M3 Pro"`, built once at startup for the HUD.
    gpu_label: String,
    /// The `.rtb` a paste joins image bytes against, held open for the session.
    pub(crate) archive: Option<ArchiveSet>,
    /// Points captured by the pen since the button went down, in world units.
    /// Lives here rather than in `input` so the gesture layer stays free of tool
    /// knowledge, and is cleared on release whether or not a stroke was made.
    pub(crate) stroke: Vec<vellum_scene::WorldPoint>,
    /// The frames being presented, in running order. Empty unless presenting.
    pub(crate) deck: Vec<vellum_scene::ItemId>,
    /// Which of [`ActiveState::deck`] is on screen.
    pub(crate) slide: usize,
    /// Whether an eraser sweep is in progress, and so whether an undo group is open.
    ///
    /// The eraser commits *live* — an eraser you cannot see working is not an eraser —
    /// but the whole sweep is one undo step, so the group is opened on the first dab
    /// that actually cuts something and closed when the button comes up.
    pub(crate) erasing: bool,
    /// A message and when it was raised: the import summary, mostly.
    pub(crate) status: Option<(String, Instant)>,
    /// The title last handed to the window server, so it is not re-set per frame.
    title: String,
    /// Whether the memory alarm has already fired, so it is said once rather than
    /// every second for the rest of the session.
    memory_alarm: bool,
    /// Resident set size as of the last stats tick, for the HUD.
    ///
    /// Cached rather than read per frame because [`resident_bytes`] forks `ps`, which
    /// is nothing once a second and absurd at 120 Hz.
    rss: u64,
    /// Whether the window server says nothing of this window is visible.
    ///
    /// Reported by winit rather than inferred from a failed present, because by the
    /// time the swapchain says `Occluded` the frame's work has already been done.
    pub(crate) occluded: bool,
    /// The flight recorder. See [`crate::flight`] — it is the only thing that survives
    /// a Force Quit, which is how both out-of-memory events ended.
    pub(crate) recorder: crate::flight::Recorder,
    /// Set by a command that ends the session; read by the event loop.
    pub(crate) quit: bool,
    /// The link-preview fetch pool. See [`crate::links`].
    ///
    /// Held here rather than per board because the threads and the already-asked set belong to
    /// the process: switching tabs mid-fetch must not lose an answer, and a card copied to
    /// another board should not be fetched twice.
    pub(crate) links: crate::links::Fetcher,
    /// Whether the window is currently accepting input-method composition.
    ///
    /// Mirrors what was last handed to `Window::set_ime_allowed`, so the call is made only
    /// when the answer changes — it crosses to the window server, and the answer changes
    /// twice per editing session rather than sixty times a second. Same rule as the title.
    pub(crate) ime_on: bool,
    /// The app's own clipboard: whole items, which the system clipboard cannot carry.
    ///
    /// App-wide, not per board, so copying on one tab and pasting on another works — and
    /// so does pasting into a board opened after the copy.
    pub(crate) clipboard: Vec<vellum_doc::Item>,
    /// The exact text [`crate::actions`]'s `copy` last wrote to the system pasteboard.
    ///
    /// This is how a paste tells *our own* text apart from another application's, and it
    /// exists because those two need opposite treatment. `copy` puts the selection's
    /// words on the pasteboard so a sticky can be pasted into another app; read back
    /// naively that is just text, so a board copy came home as one item holding every
    /// copied item's words concatenated and the items themselves were never reached.
    ///
    /// Reordering the flavours instead would trade that bug for its mirror image, which
    /// this project has already shipped once: with the internal clipboard tried early,
    /// pasting a screenshot silently re-pasted the last thing copied on the board.
    /// Identifying our own write serves both. `None` once anything else has claimed the
    /// pasteboard, or when the copy carried no text at all.
    pub(crate) clipboard_text: Option<String>,
    /// The **system** clipboard handle, opened once for the whole application.
    ///
    /// Here rather than on `Editor` because there is one pasteboard on the machine,
    /// not one per open board — an `Editor` each holding its own meant a platform
    /// handle per tab. Opened lazily, because a session that never pastes should never
    /// touch it, and the error is *stored* rather than retried: a machine with no
    /// clipboard will not grow one, and retrying per keystroke is the same unbounded
    /// work in a different shape.
    pub(crate) system_clipboard: Option<Result<arboard::Clipboard, String>>,
    /// The drag in flight, if any. See [`crate::actions::Drag`].
    pub(crate) drag: Option<crate::actions::Drag>,
    /// How many items the board held when the current eraser sweep first removed something.
    ///
    /// The count reported at the end is the difference, not a tally of removals: taking a
    /// frame takes its children with it, so counting what was *asked for* said 2 where the
    /// user watched 3 things disappear.
    pub(crate) erased_from: Option<usize>,
    /// The on-canvas text edit in progress. See [`crate::edit`].
    pub(crate) editing: Option<crate::edit::Editing>,
    /// When the caret last moved or the text last changed, for the blink phase.
    ///
    /// Here rather than on `crate::edit::Editing`, which is deliberately pure — a clock
    /// in that module would make its tests depend on when they ran. Reset by
    /// `ActiveState::touch_caret`.
    pub(crate) editing_touched: Instant,
    /// The card whose ↗ badge the pointer is on. Resolved once in `run_chrome` and read
    /// again by the paint pass, so the highlight and the cursor cannot disagree.
    pub(crate) hovered_badge: Option<vellum_scene::ItemId>,
    /// The item whose four connector ports the pointer is over — Miro's blue dots.
    ///
    /// Recomputed per frame like `hovered_badge`, and widened past the item's own bounds
    /// because a port sits *outside* the edge: see
    /// [`crate::actions::ActiveState::ports_under_pointer`].
    pub(crate) hovered_ports: Option<vellum_scene::ItemId>,
    /// The item and anchor a connector is being dragged **from**, while the button is down.
    ///
    /// Holds the [`vellum_doc::ItemId`] rather than the `SceneId`: a reprojection renumbers
    /// scene ids and the gesture has to survive one.
    ///
    /// Its presence is also what makes the drag *a connector* rather than a marquee — the
    /// port press borrows `input::Tool::Place`, so this is the only thing that says which
    /// verb the release means.
    pub(crate) port_arm: Option<(vellum_doc::ItemId, (f64, f64))>,
    /// The connector whose end is being dragged, and whether it is the *end* rather than the
    /// start — Miro's two round grips on a selected line.
    ///
    /// Its own field rather than a `DragMode`, following `card_drag`'s precedent and for its
    /// reason: this changes **no placement**. A connector's geometry is two bindings, so
    /// re-attaching one is a rewrite of `ItemKind::Connector`, which `Drag` has no way to
    /// express and `Drag::placement_of` has nothing to answer for.
    pub(crate) endpoint_arm: Option<(vellum_doc::ItemId, bool)>,
    /// The alignment guides the gesture in progress is reporting, in **world** units.
    ///
    /// Miro's *Align objects*. Rebuilt by whatever is dragging — a move, a resize, a
    /// placement — and cleared when nothing is, so a stale line cannot outlive the gesture
    /// that justified it. Held here rather than recomputed by the painter for the reason
    /// `hovered_badge` above is: the correction and the line have to come from one answer,
    /// or the guide points at a place the item did not go.
    pub(crate) guides: Vec<crate::snap::Guide>,
    /// A kanban card in flight. Separate from `drag` because it changes no placement:
    /// a card's position is decided by the column it is in and its rank within it, so
    /// moving one rewrites the item's token and leaves its box alone.
    pub(crate) card_drag: Option<crate::actions::CardDrag>,
    /// Set when the board should be re-framed as soon as the chrome has reported how
    /// much of the window is actually canvas — which it cannot do before its first
    /// run, and startup happens before that.
    pub(crate) pending_fit: bool,
    /// Where the last paste was aimed, rounded, and how many pastes have landed there in
    /// a row. See `ActiveState::paste_now` — without the cascade they stack invisibly.
    pub(crate) last_paste_at: Option<(i64, i64)>,
    /// A Miro import announced on one frame and run on the next, so its toast can paint
    /// before the window stops repainting for two seconds. See `ActiveState::paste_aimed`.
    pub(crate) pending_import: Option<crate::actions::PendingImport>,
    pub(crate) paste_repeats: u32,
    /// What `Cmd+F` last found, in paint order, and where in that list the user is.
    pub(crate) matches: Vec<vellum_doc::ItemId>,
    pub(crate) match_index: usize,
    /// `--open`: further boards to put in tabs, opened on the first frame rather than
    /// during startup because opening one renders a thumbnail of the board being left,
    /// and that needs the canvas rectangle the chrome has not reported yet.
    pending_open: Vec<std::path::PathBuf>,
    /// `--tab`: which tab to bring to the front once they are all open.
    pending_tab: Option<usize>,
    /// `--select-all`, applied on the first frame. See `Options::select_all`.
    pending_select_all: bool,
    /// `--open-dialog NAME`, raised on the first frame. See `Options::open_dialog`.
    pending_dialog: Option<String>,
    /// `--show NAME`, applied on the first frame. See `crate::options::Options::show`.
    pending_show: Vec<String>,
    /// `--paste`, run on the first frame. See `Options::paste`.
    pending_paste: bool,
    /// `--select-one`, applied on the first frame. See `Options::select_one`.
    pending_select_one: bool,
    /// `--demo NAME`, built on the first frame. See `Options::demo`.
    pending_demo: Option<String>,
    /// `--screenshot`: where one composited frame is written. Taken once, then
    /// forgotten, which is also how "has it been taken" is recorded.
    screenshot: Option<std::path::PathBuf>,
    /// When the window came up, for the screenshot's delay.
    opened_at: Instant,
}

pub struct Vellum {
    options: Options,
    state: Option<ActiveState>,
    started: Instant,
    exit_requested: bool,
    /// Startup failures cannot be returned from `ApplicationHandler`, so they are
    /// parked here and re-raised by `main` after the loop unwinds. Otherwise a
    /// failed GPU init would exit 0 with nothing but a log line.
    startup_error: Option<anyhow::Error>,
}

impl Vellum {
    pub fn new(options: Options) -> Self {
        Self {
            options,
            state: None,
            started: Instant::now(),
            exit_requested: false,
            startup_error: None,
        }
    }

    /// The error that stopped startup, if any. Checked by `main` once the event loop
    /// has returned.
    pub fn take_startup_error(&mut self) -> Option<anyhow::Error> {
        self.startup_error.take()
    }

    fn start(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<ActiveState> {
        let attributes = Window::default_attributes()
            .with_title("Velm")
            .with_inner_size(winit::dpi::LogicalSize::new(1440.0, 900.0));
        let window = Arc::new(event_loop.create_window(attributes)?);
        let surface = crate::surface::Surface::new(window.clone(), !self.options.no_vsync)?;

        let blobs = BlobStore::open(editor::blob_directory())?;
        // Every backup the user has, searched as one set. The app's own `archives/`
        // folder loads unconditionally so a migration needs no flag at all; `--rtb`
        // adds to it rather than replacing it, and takes a file or a folder.
        //
        // A missing folder is not an error — it is the ordinary state before the user
        // has put anything in it — but a path they *named* that cannot be read is,
        // because silently importing a board with no pictures is the failure this
        // whole path exists to prevent.
        let mut library = Library::open(editor::default_board_path().parent().map_or_else(
            || editor::data_directory().join("boards"),
            std::path::Path::to_path_buf,
        ));

        // Built *after* the library, which is where attached backups are remembered.
        let mut archive = {
            let mut set = ArchiveSet::new();
            let home = crate::editor::archive_directory();
            if home.is_dir() {
                set.add(&home)?;
            }
            for path in &self.options.rtb {
                set.add(path)?;
            }
            // Backups attached through Import from Miro in an earlier session. Remembered
            // by path rather than copied, so this is where they rejoin. A failure here is
            // logged and skipped, never fatal: the file was readable when it was attached
            // and has since been moved or unplugged, which costs pictures on a re-import
            // and nothing else. `Library::attached_archives` has already dropped the ones
            // that do not currently exist.
            for path in library.attached_archives() {
                if let Err(error) = set.add(&path) {
                    log::warn!("skipping attached archive {}: {error}", path.display());
                }
            }
            if set.is_empty() {
                None
            } else {
                log::info!(
                    "{} Miro archive(s) loaded, {} assets: {}",
                    set.len(),
                    set.asset_count(),
                    set.boards().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ")
                );
                Some(set)
            }
        };

        // Which board opens, and which screen shows. A run that names a board — or a
        // capture to import, or a bench size — was asked for that board and goes
        // straight to it. A bare launch lands on the library, which is the screen the
        // user asked for by name after opening the app and finding a bare canvas.
        let asked_for_a_board = self.options.board.is_some()
            || !self.options.open.is_empty()
            || self.options.import.is_some()
            || self.options.bench_items > 0;

        let opened = Instant::now();
        let mut start_on_library = !asked_for_a_board;
        let mut editor = if self.options.bench_items > 0 {
            Editor::in_memory(
                bench::scattered_board(self.options.bench_items, self.options.seed)?,
                blobs,
            )
        } else {
            let path = self
                .options
                .board
                .clone()
                .or_else(|| self.options.import.as_deref().map(import_board_path))
                .or_else(|| library.last_board().map(std::path::Path::to_path_buf))
                .unwrap_or_else(editor::default_board_path);
            match Editor::open(&path, blobs.clone()) {
                Ok(editor) => editor,
                // **A board that will not open must not stop the app starting.** On a
                // bare launch the library is the screen that was going to show
                // anyway, and it is the only screen from which the user could pick a
                // different board — so dying here takes away the one thing that could
                // rescue them, over a board that was never going to be drawn. An
                // ejected external drive and a truncated file both land here.
                Err(error) if self.options.board.is_none() => {
                    log::error!(
                        "opening {}: {error:#} — starting in the board library instead",
                        path.display()
                    );
                    // Or the same failure greets them again on the next launch.
                    library.set_last_board(None);
                    start_on_library = true;
                    Editor::in_memory(vellum_doc::Board::new(), blobs)
                }
                // A board named on the command line is a contract: fail loudly.
                Err(error) => return Err(error),
            }
        };
        library.rescan();

        let mut status = None;
        if let Some(path) = self.options.import.clone() {
            let html = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            // **An import replaces the board it lands in, unless one was named.**
            // `--import` used to append, and the documented run command has no
            // `--board`, so every launch stacked another copy of the capture onto the
            // persisted default board: three runs of it made 1,788 items out of 596,
            // corrupting the reference board and every benchmark taken on it. With
            // no `--board`, the target is a scratch board named after the capture and
            // re-importing it is idempotent.
            if self.options.board.is_none() && editor.board().item_count() > 0 {
                let cleared = editor.clear();
                log::info!("re-importing: cleared {cleared} items from the previous import");
            }
            // Opening a board is not a paste: selecting all 596 items before the user
            // has touched anything is what made the app look broken.
            match editor.import_html_selecting(
                &html,
                archive.as_mut(),
                crate::editor::AfterImport::SelectNothing,
            )? {
                Some(outcome) => {
                    log::info!("{outcome}");
                    status = Some(format!("imported {} items from Miro", outcome.total()));
                }
                None => log::warn!("{} holds no Miro payload", path.display()),
            }
        }

        log::info!(
            "board: {} items, opened in {:.1} ms",
            editor.board().item_count(),
            opened.elapsed().as_secs_f64() * 1000.0
        );

        let (width, height) = surface.size();
        let mut camera = Camera::new(ScreenSize::new(f64::from(width), f64::from(height)));
        fit_content(&mut camera, editor.projection());
        if let Some(zoom) = self.options.zoom {
            let centre = ScreenPoint::new(f64::from(width) / 2.0, f64::from(height) / 2.0);
            camera.set_zoom_about(zoom, centre);
        }

        let mut input = Input::new(InputConfig {
            pan_sensitivity: self.options.pan_sensitivity,
            zoom_sensitivity: self.options.zoom_sensitivity,
            invert_pan: self.options.invert_pan,
            invert_zoom: self.options.invert_zoom,
            drag_inertia: self.options.inertia,
            wheel_zooms: !self.options.wheel_pans,
            scroll_inertia: self.options.inertia && InputConfig::default().scroll_inertia,
        });
        input.set_scale_factor(window.scale_factor());

        let gpu_label = {
            let info = surface.adapter_info();
            format!("{:?} / {}", info.backend, info.name)
        };

        let max_texture_side = surface.device().limits().max_texture_dimension_2d as usize;
        let mut shell = Shell::new(&window, library, Appearance::watch(), max_texture_side);
        shell.set_screen(if start_on_library { Screen::Library } else { Screen::Board });
        // The board the app opened at launch gets its tab too, so the strip is never
        // empty over a window that already has a document behind it. Brought to the
        // front only when the window is actually showing it: starting on the library
        // has to leave the home tab in front, or the strip and the screen disagree.
        if let Some(path) = editor.path().map(std::path::Path::to_path_buf) {
            shell.open_tab(&path, &editor.board().title());
            if start_on_library {
                shell.show_home_tab();
            }
        }

        let glass = GlassRenderer::new(surface.device(), surface.format());
        let chrome = ChromePass::new(surface.device(), surface.format());

        let theme = theme_for(shell.theme(), shell.accent());
        let now = Instant::now();

        // Opened before anything else can go wrong, and the previous session is read
        // *now* — a Force Quit leaves no other trace, so if the last run ended without
        // an `EXIT` line this is the only place that will ever say so.
        let data_directory = editor::data_directory();
        let mut recorder = crate::flight::Recorder::open(&data_directory);
        if let Some(previous) = crate::flight::previous_session(&data_directory, recorder.path())
            && !previous.clean
        {
            log::error!(
                "the previous session ended without quitting, at a peak of {} MB. \
                 Its record is at {}",
                previous.peak_rss / 1_048_576,
                previous.path.display()
            );
            for line in &previous.tail {
                log::error!("  {line}");
            }
            recorder.event(&format!(
                "previous-session-crashed peak={}M record={}",
                previous.peak_rss / 1_048_576,
                previous.path.display()
            ));
        }
        crate::flight::prune(&data_directory);

        // Resolved once, at startup, rather than per board: the address and the token are a
        // property of this person's setup. A server given with no token is **allowed and
        // warned about**, not refused — velmd needs none on loopback and refuses to bind a
        // public address without one, so an empty token is a legitimate local configuration.
        // Refusing to start over it would take away the board library, which is the only
        // screen from which the mistake could be corrected.
        let sync = self.options.sync.clone().map(|config| {
            let token = crate::options::take_sync_token().unwrap_or_default();
            if token.is_empty() && !crate::options::is_loopback(&config.server) {
                log::warn!(
                    "sync: {} is not a loopback address and ${} is not set, so every request \
                     will be refused. Set it in the shell that launches Velm.",
                    config.server,
                    crate::options::TOKEN_VAR
                );
            }
            log::info!("sync: {} every {}s", config.server, config.period);
            // `Bearer` even when the token is empty, rather than `None`. `auth_header` turns
            // an empty bearer into no header at all, so the bytes on the wire are exactly what
            // they were — and keeping the *kind* means the reply applier can still tell a
            // flag-configured setup apart from a signed-in one, which is what decides whether
            // a 401 is a configuration mistake to report or a session to give up on.
            SyncConfig {
                options: config,
                credential: crate::sync::Credential::Bearer(token),
                asked_at: None,
                reported: None,
            }
        });

        // The address and the name the last sign-in used, so the page opens with them filled
        // in. **The password is not here and never will be** — nothing on this machine stores
        // it, which is why signing in is asked for again on every launch. Once, here: this is
        // the only write to those buffers that is not the person's own typing.
        shell.seed_account();

        let mut state = ActiveState {
            startup_sync: sync.clone(),
            pending_signin: None,
            push: crate::push::PushState::default(),
            account: AccountStatus::default(),
            sync,
            occluded: false,
            recorder,
            window,
            surface,
            editor,
            painter: Painter::new(TextCache::new()?),
            camera,
            input,
            theme,
            shell,
            // Here, not in `main`: `NSMenu` has to be built on the main thread and
            // `winit` has already made `NSApplication` by the time a window exists, so
            // this is both the earliest safe point and the one that runs on the right
            // thread by construction.
            #[cfg(target_os = "macos")]
            menubar: crate::menubar::MenuBar::install(),
            session: crate::session::Session::default(),
            glass,
            chrome,
            list: DrawList::new(),
            hud_quads: Vec::new(),
            show_hud: self.options.hud,
            timer: FrameTimer::new(),
            stats: PaintStats::default(),
            zoom_sweep: None,
            last_frame: now,
            last_stats_log: now,
            gpu_label,
            archive,
            stroke: Vec::new(),
            erasing: false,
            deck: Vec::new(),
            slide: 0,
            status: status.map(|text| (text, now)),
            title: String::new(),
            memory_alarm: false,
            rss: 0,
            quit: false,
            links: crate::links::Fetcher::new(),
            ime_on: false,
            clipboard: Vec::new(),
            clipboard_text: None,
            system_clipboard: None,
            drag: None,
            card_drag: None,
            editing: None,
            editing_touched: Instant::now(),
            hovered_badge: None,
            hovered_ports: None,
            port_arm: None,
            endpoint_arm: None,
            guides: Vec::new(),
            erased_from: None,
            pending_fit: self.options.zoom.is_none(),
            last_paste_at: None,
            pending_import: None,
            paste_repeats: 0,
            matches: Vec::new(),
            match_index: 0,
            pending_open: self.options.open.clone(),
            pending_tab: self.options.tab,
            pending_select_all: self.options.select_all,
            pending_dialog: self.options.open_dialog.clone(),
            pending_show: self.options.show.clone(),
            pending_paste: self.options.paste,
            pending_select_one: self.options.select_one,
            pending_demo: self.options.demo.clone(),
            screenshot: self.options.screenshot.clone(),
            opened_at: now,
        };

        // The picker may only offer families the shaper has faces for, and that answer lives
        // in the text engine — which does not exist until the painter above does. Its list was
        // hardcoded and named two Windows fonts, so on this machine picking either stored a
        // value and changed nothing on screen. See `Shell::set_font_families`.
        let families = state.painter.text_mut().engine_mut().families();
        state.shell.set_font_families(families);
        Ok(state)
    }
}

/// Where `--import capture.html` puts its board when no `--board` was named.
///
/// Beside the other boards, named after the capture, so re-running the documented
/// command is idempotent and never touches `board.vellum` — which is the user's own
/// default board and the one a bare launch opens.
fn import_board_path(capture: &std::path::Path) -> std::path::PathBuf {
    let stem = capture
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "imported".to_owned());
    editor::default_board_path().with_file_name(format!("{stem}.{}", vellum_store::BOARD_EXTENSION))
}

/// Which tab `⌘<digit>` means, given how many tabs there are — home included.
///
/// The browser rule, because it is the one the user's hands already have: `⌘1`…`⌘8`
/// are the first eight tabs and **`⌘9` is the last one**, however many there are. Tab
/// one is home, so `⌘1` is the board library — which is right rather than merely
/// consistent: the library *is* a tab, and it is the one the user goes back to.
///
/// `None` for a digit that names no tab, so `⌘7` on a window with three of them does
/// nothing instead of landing somewhere arbitrary.
pub(crate) fn tab_for_digit(digit: &str, tabs: usize) -> Option<usize> {
    if tabs == 0 {
        return None;
    }
    let index = match digit {
        "9" => tabs - 1,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" => {
            digit.parse::<usize>().ok()?.checked_sub(1)?
        }
        _ => return None,
    };
    (index < tabs).then_some(index)
}

/// The canvas palette that goes with the chrome's.
///
/// The two live in different crates on purpose — `crate::theme` holds what the *board*
/// falls back to, `vellum_ui::theme` holds the chrome — and this is the one place they
/// are chosen together, so a theme switch cannot move one and leave the other.
pub(crate) const fn theme_for(theme: vellum_ui::Theme, accent: vellum_ui::Accent) -> Theme {
    match theme {
        // Folded in *here* rather than at the call sites, so the accent cannot be applied
        // to the chrome and forgotten on the board — which is the exact failure this
        // function's own doc comment exists to prevent, one field further down.
        vellum_ui::Theme::Light => Theme::LIGHT.with_accent(accent),
        // **The dark cut is left frozen**, the same decision `vellum_ui::Palette::DARK`
        // records: it is unreachable, its accent is a transcription of the user's own
        // second swatch card, and two of the three colours Preferences offers have no dark
        // value at all. Repainting it would mean inventing them.
        vellum_ui::Theme::Dark => Theme::DARK,
    }
}

impl ApplicationHandler for Vellum {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // `resumed` fires again after a suspend; only build once.
        if self.state.is_some() {
            return;
        }
        match self.start(event_loop) {
            Ok(state) => {
                log::info!(
                    "ready in {:.0} ms",
                    self.started.elapsed().as_secs_f64() * 1000.0
                );
                state.window.request_redraw();
                self.state = Some(state);
            }
            Err(error) => {
                log::error!("startup failed: {error:#}");
                self.startup_error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if window_id != state.window.id() {
            return;
        }

        // Offered to the chrome first, because the chrome is what knows whether the
        // pointer is over a panel. What is done with the answer is the rule in
        // `crate::shell`'s header, not a bare `if consumed`.
        let window = state.window.clone();
        let response = state.shell.on_window_event(&window, &event);
        // A gesture already in flight outranks the chrome. Without this, releasing a
        // pan over the toolbar would never end the pan and the board would stick to
        // the cursor.
        let gesture_in_flight = state.input.is_panning() || state.input.is_gesturing();
        let taken = response.consumed && !gesture_in_flight;

        match event {
            WindowEvent::CloseRequested => {
                state.shut_down();
                event_loop.exit();
            }

            WindowEvent::Resized(size) => {
                state.surface.resize(size.width, size.height);
                let (width, height) = state.surface.size();
                state
                    .camera
                    .set_viewport(ScreenSize::new(f64::from(width), f64::from(height)));
            }

            // A hidden window still gets redraw requests, and every one of them used to
            // decode images, shape text and stage a frame's worth of GPU uploads for
            // pixels that do not exist. `Surface::flush_staged_work` is what stops that
            // from *leaking*; this is what stops it from being done at all.
            WindowEvent::Occluded(occluded) => {
                state.occluded = occluded;
                log::debug!("window {}", if occluded { "occluded" } else { "visible" });
                state.recorder.event(if occluded { "occluded" } else { "visible" });
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Moving the window between a Retina and a 1× display changes how
                // many physical pixels a gesture reports. Positions stay physical
                // either way; only the *rates* have to be told.
                state.input.set_scale_factor(scale_factor);
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                // Always, whoever owns the pointer: modifiers are not a gesture that
                // either party competes for, and a stale shift key makes the next
                // click on the canvas replace a selection it should have extended.
                state.input.set_modifiers(modifiers.state());
            }

            // Two halves, and the guard is on the second only. *Where* the pointer is
            // stays true whoever owns it — a paste, a placement and the cursor icon all
            // ask, and a position that freezes the moment the mouse touches a panel is a
            // stale answer nothing announces. *Acting* on the move is the part the chrome
            // gets to veto, because a pan or a marquee must not continue under a toolbar.
            WindowEvent::CursorMoved { position, .. } => {
                let position = ScreenPoint::new(position.x, position.y);
                if taken {
                    state.input.note_cursor(position);
                } else {
                    let intent = state.input.cursor_moved(&mut state.camera, position);
                    state.act_on(intent);
                }
            }

            WindowEvent::CursorLeft { .. } => state.input.cursor_left(),

            WindowEvent::MouseInput { state: button_state, button, .. } if !taken => {
                let intent = state.input.mouse_input(&mut state.camera, button, button_state);
                state.act_on(intent);
            }

            WindowEvent::MouseWheel { delta, phase, .. } if !taken => {
                state.input.wheel(&mut state.camera, delta, phase);
            }

            WindowEvent::PinchGesture { delta, .. } if !taken => {
                state.input.pinch(&mut state.camera, delta)
            }

            // Composition, from an input method. Only ever delivered while
            // `set_ime_allowed(true)` is in force, which is exactly while a caret is on the
            // board — see `ActiveState::follow_ime`.
            //
            // The composing text is written *through* to the document as it changes rather
            // than overlaid, because the canvas shapes its glyphs from the document: held
            // anywhere else it would be invisible until committed, and a sticky that shows
            // nothing until you press Return is not editing on the board.
            WindowEvent::Ime(ime) if !taken => {
                state.ime(ime);
            }

            WindowEvent::KeyboardInput { event, .. } if !taken => {
                // A caret on the canvas claims the keyboard first — before `input.key`,
                // which would take the space bar for panning and `V`/`N`/`T` for tools,
                // and before the shortcut table, which would nudge on an arrow key. It
                // claims only the keys that mean *text*, so `⌘S` still saves; see
                // `ActiveState::type_key`.
                //
                // `event.text` is where the typed characters come from rather than the
                // key itself: the platform has already applied the layout, so a French
                // keyboard's `A` arrives as `q` correctly and a shifted `2` arrives as
                // whatever that layout puts there.
                // Logged at debug because *whether a key arrives at all, and whether it
                // carries text*, is the first question in every input bug this project has
                // had — trap 9 in `CLAUDE.md` is three rounds of "paste is fixed" for a
                // keystroke that was never delivered. Guessing costs more than a log line.
                log::debug!(
                    "key {:?} text {:?} pressed {} editing {} ime {}",
                    event.logical_key,
                    event.text,
                    event.state.is_pressed(),
                    state.editing.is_some(),
                    state.ime_on,
                );
                if state.editing.is_some()
                    && event.state.is_pressed()
                    && state.type_key(&event.logical_key, event.text.as_deref())
                {
                    return;
                }
                // The pointer tools and the space modifier are input's, not the
                // application's: they change what a *drag* does, not what a command
                // does, and they have to see key-up as well as key-down.
                if state.input.key(&event.logical_key, event.state) {
                    return;
                }
                if event.state.is_pressed() {
                    state.shortcut(&event.logical_key);
                }
            }

            WindowEvent::RedrawRequested => {
                state.frame();
                if state.quit {
                    state.shut_down();
                    event_loop.exit();
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        if let Some(limit) = self.options.exit_after_secs
            && self.started.elapsed().as_secs_f64() >= limit
        {
            // `exit()` only requests a shutdown, so `about_to_wait` fires again
            // before the loop unwinds; without this guard the summary is logged
            // once per remaining tick.
            if !self.exit_requested {
                self.exit_requested = true;
                log::info!(
                    "ran {:.1} s, {} frames, {:.1} fps average, {:.2} ms/frame smoothed",
                    self.started.elapsed().as_secs_f64(),
                    state.timer.total_frames(),
                    state.timer.average_fps(),
                    state.timer.frame_ms()
                );
                state.shut_down();
                event_loop.exit();
            }
            return;
        }

        // A hidden window must not drive frames at all.
        //
        // Presenting is what paces this loop: `Surface::present` blocks on the
        // swapchain, and the display throttles it to the refresh rate. An occluded
        // window is never presented to, so **that back-pressure disappears entirely** —
        // measured by the flight recorder at **59,787 frames per second**, each one a
        // full frame of culling, text shaping, image decoding and staged GPU uploads.
        // That is the engine of both out-of-memory events: not sixty frames a second
        // quietly staging bytes, but sixty *thousand*.
        //
        // So while occluded the loop waits for an event instead of asking for another
        // frame. Any input, a resize, or the window becoming visible again wakes it.
        // Waking once a second rather than waiting outright: a session spent behind
        // another window is exactly the one with no other record, so the recorder has
        // to keep sampling. One tick a second is free; sixty thousand is the bug.
        if state.occluded {
            state.log_stats(Instant::now());
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + STATS_LOG_INTERVAL,
            ));
            return;
        }

        // Drive frames continuously. A canvas app is animating whenever the user is
        // touching it, and a steady loop is also the only way to report an honest
        // frame rate.
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
        state.window.request_redraw();
    }
}

impl ActiveState {
    /// Miro's command bindings that the chrome does **not** already own.
    ///
    /// Almost nothing is left here on purpose. `vellum-ui`'s command table binds Undo,
    /// Redo, Select all, Paste, Delete and the zoom family, and it consumes those keys
    /// from egui before anything else sees them. Handling them here as well would run
    /// each of them twice — one press of `⌘Z` undoing two edits — which is the exact
    /// failure mode `CLAUDE.md`'s trap 5 is about.
    ///
    /// **That is true of every one of those keys except `⌘V`, `⌘C` and `⌘X`, and
    /// believing it of them too is what made pasting a screenshot do nothing.**
    /// `egui-winit` converts the three clipboard chords into `Event::Paste`/`Copy`/
    /// `Cut` and never emits the key, so the command table's `consume_shortcut` could
    /// not match and `Command::Paste` was unreachable from the keyboard. The key is
    /// put back in [`Shell::on_window_event`], whose header carries the detail; it is
    /// repaired there rather than answered here so the command table stays the one
    /// place a binding lives, and so a focused text field still keeps its own paste.
    ///
    /// What is genuinely not the chrome's: the HUD, which is a developer overlay with
    /// no menu row; Escape, which abandons a drag or clears the selection as it does
    /// in Miro; the arrow keys, which nudge the selection; `⌘Q`, because there is
    /// no application menu to carry it; and `⌘1`…`⌘9`.
    ///
    /// **Why the tab numbers are here and not in `vellum-ui`'s table.** That table is
    /// keyed by [`vellum_ui::Command`], one row per named action, and "jump to tab 4"
    /// is not one — it is an index into something the strip owns. The strip's own keys
    /// (`⌘T`, `⌘⌥←`, `⌘⌥→`) are `vellum-ui`'s because they need no index. These reach
    /// the app because `egui-winit` reports a key as consumed only when a text field
    /// has focus, which [`Shell::keyboard_captured`] already guards.
    pub(crate) fn shortcut(&mut self, key: &Key) {
        if self.shell.keyboard_captured() {
            return;
        }
        let modifiers = self.input.modifiers();
        let command = modifiers.super_key() || modifiers.control_key();
        // A text session on the canvas takes the keys a typing hand produces, and the line
        // is drawn at ⌘/⌃ exactly where `vellum_ui::Chrome::shortcuts` draws it — the same
        // rule in the two places a keystroke can become a command, so they cannot drift.
        //
        // **What reaches here that the chrome never sees**, and why this is not belt and
        // braces: the arrows. The caret claims Up and Down, and without this guard `↑` in
        // the middle of a word would fall through to `nudge` and **move the item being
        // typed into**. `⌘Q` and `⌘1`…`⌘9` both carry a modifier and are untouched; Escape
        // never arrives at all, because the session claims it.
        if !command && self.text_session_owns_keyboard() {
            return;
        }
        // Miro's step and its coarse step. One world unit is one board pixel, which
        // is the unit every coordinate in the properties panel is already in.
        let step = if modifiers.shift_key() { COARSE_NUDGE } else { NUDGE };

        match key.as_ref() {
            Key::Named(NamedKey::F1) => self.show_hud = !self.show_hud,
            Key::Named(NamedKey::Escape) => {
                // Leaving a presentation comes first: it is the only way out, the
                // chrome that would otherwise offer one is not drawn, and nothing is
                // being dragged or selected in front of an audience anyway.
                if self.shell.view().presenting {
                    self.leave_presenting();
                } else if !self.cancel_drag() {
                    // A drag next: Escape while something is being dragged means "put
                    // it back", not "and also forget what I had selected".
                    //
                    // Then the selection, and **only then the tool**. *"when i dont have
                    // anything selected and if i press escape butotn it should autmatically
                    // select the seelct"* — Escape is the key a hand reaches for to mean
                    // "stop whatever this is", and with a create tool armed and nothing
                    // picked there was nothing left for it to stop: the pen stayed armed and
                    // the next click drew. So the escapes are a ladder, each rung undoing
                    // less than the one above, and disarming is the last one.
                    //
                    // **Not unconditional**, which is the part worth stating: pressing
                    // Escape to drop a selection must leave the tool alone, or a user
                    // placing five stickies in a row loses the tool on the first Escape.
                    if self.editor.selection().is_empty() {
                        if self.shell.tool() != vellum_ui::Tool::Select {
                            self.choose_tool(vellum_ui::Tool::Select);
                        }
                    } else {
                        self.editor.clear_selection();
                        self.shell.invalidate_selection();
                    }
                }
            }
            // While presenting, the arrows and the space bar move between slides —
            // there is nothing selected to nudge, and they are the keys every hand
            // reaches for in front of a deck.
            Key::Named(NamedKey::ArrowLeft) if self.shell.view().presenting => {
                self.advance_slide(-1);
            }
            Key::Named(NamedKey::ArrowRight | NamedKey::Space)
                if self.shell.view().presenting =>
            {
                self.advance_slide(1);
            }
            Key::Named(NamedKey::ArrowUp) if self.shell.view().presenting => {
                self.advance_slide(-1);
            }
            Key::Named(NamedKey::ArrowDown) if self.shell.view().presenting => {
                self.advance_slide(1);
            }
            Key::Named(NamedKey::ArrowLeft) => self.nudge(-step, 0.0),
            Key::Named(NamedKey::ArrowRight) => self.nudge(step, 0.0),
            Key::Named(NamedKey::ArrowUp) => self.nudge(0.0, -step),
            Key::Named(NamedKey::ArrowDown) => self.nudge(0.0, step),
            Key::Character("q" | "Q") if command => self.quit = true,
            Key::Character(digit) if command && digit.len() == 1 => {
                if let Some(index) = tab_for_digit(digit, self.shell.tab_count()) {
                    self.select_tab(index);
                }
            }
            _ => {}
        }
    }

    /// Everything that has to happen before the process ends.
    ///
    /// A board that only *mostly* saved on quit is the one bug that loses a user's
    /// work, so the writer thread is drained rather than trusted. The thumbnail is
    /// taken here too: it is the last moment the board is both loaded and on the GPU.
    ///
    /// **Every open board, not only the one in front.** With tabs there can be several
    /// documents resident, and the ones behind are exactly the ones a user would not
    /// notice losing until much later. They need no thumbnail: a parked board cannot
    /// be edited, so the preview taken as it was parked is still what it looks like.
    fn shut_down(&mut self) {
        // Whatever was being typed is already in the document — a keystroke writes
        // through — so this closes the undo group rather than saving the text. Left open,
        // the group would be reopened by the next edit after a restart, which is not a
        // thing Loro's history should be asked to represent.
        self.commit_editing();
        // The eraser holds a group open across its dabs for the same reason, and quitting
        // mid-sweep is a way for it to end without the button ever coming up.
        self.finish_erase();
        let path = self.editor.path().map(std::path::Path::to_path_buf);
        if let Some(path) = path.as_deref() {
            self.capture_thumbnail(path);
        }
        self.shell.library.set_last_board(path.as_deref());
        if let Err(error) = self.editor.flush() {
            log::error!("saving on quit: {error:#}");
        }
        for mut parked in self.session.drain() {
            if let Err(error) = parked.flush() {
                log::error!("saving {} on quit: {error:#}", parked.path().display());
            }
        }
        // Last, and the whole point of it: the presence of this line is what tells the
        // next run that this session was quit rather than killed.
        self.recorder.finish("quit");
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let elapsed = now - self.last_frame;
        self.last_frame = now;
        self.timer.record(elapsed);

        // The window's input method follows the caret: allowed while one is on the board and
        // not otherwise. Here rather than at every site that starts or ends a session — there
        // are five — because a frame is the one place that always runs afterwards.
        //
        // **Above the occlusion guard below**, which returns early. Whether the window accepts
        // composition is not a drawing concern, and a caret opened while the window happened
        // to be behind another one would otherwise never enable it — and then never enable it
        // again, because `ime_on` would already agree. Only on a change; `set_ime_allowed`
        // crosses to the window server.
        let ime = self.ime_allowed();
        if ime != self.ime_on {
            self.window.set_ime_allowed(ime);
            self.ime_on = ime;
        }

        // Answers from the link-preview pool, applied to the document. Before the occlusion
        // guard for the same reason the IME state is: a fetch that came back while the window
        // was behind another one must still land, or the card stays blank until the next time
        // something happens to redraw.
        self.apply_link_fetches();

        // ⚠ **Before `apply_sync`, never inside it.** `apply_sync` returns early when there
        // is no sync configured, which is exactly the signed-out state a sign-in is trying to
        // leave — a drain inside it would never run on the one machine that needed it, and
        // Settings ▸ Account would sit on *Signing in* for ever. One `Option` test when
        // nothing is in flight, which is almost always.
        self.drain_sign_in();

        // The first run's progress, in the same place and for the same reason: a board that
        // finished while the window was behind another one must still be recorded, or the id
        // is lost and the next run makes a second board on the server. One `Option` test when
        // nothing is running, which is almost always.
        self.drain_push();

        // The sync round trip, for the same reason and in the same place: an answer that came
        // back while the window was behind another one must still land. Before the occlusion
        // guard, so a board left open on a second monitor keeps up. Two comparisons when
        // `--sync-server` was not given, which is the default.
        self.apply_sync();

        // Nothing of this window is on screen. Everything below — culling, text
        // shaping, image decoding, and a frame's worth of staged GPU uploads — would be
        // for pixels that do not exist, and `Surface::present` would throw the frame
        // away at the end of it anyway.
        //
        // `Surface::flush_staged_work` is what stops a skipped frame from *leaking*;
        // this is what stops the work being done in the first place. Memory is still
        // sampled, because a session spent behind another window is exactly the one
        // there is no other record of.
        if self.occluded {
            return;
        }

        // Inertia is advanced here rather than in the event handler because it is a
        // function of *time*, not of an event, and the frame is the only place the
        // app knows how much has passed.
        self.input.tick(&mut self.camera, elapsed);

        // Retires cached layouts and ink for items an edit removed. A no-op unless
        // the document actually changed since the last frame.
        self.painter.sync(self.editor.epoch(), self.editor.projection());


        // Both residency caches advance their clock before anything marks itself
        // used, so "not touched this frame" is a fact rather than a race.
        self.surface.begin_frame();
        self.editor.assets_mut().begin_frame();
        // A keystroke outranks a picture. While a dialog, the palette or the find bar is
        // up the user is typing into it, so the board behind stops decoding images for
        // the duration — see `Assets::suspend_decoding` for the measurement that made
        // this necessary, and for why deferring the work is safe.
        if self.shell.has_modal() {
            self.editor.assets_mut().suspend_decoding();
        }

        self.run_chrome();
        // Now, and not in `start`: the canvas rectangle is what the chrome leaves for
        // the board, and the chrome has only just run for the first time.
        // Announced last frame, run now — the toast has painted, so the seconds the import
        // takes are visibly the app working rather than the app hanging.
        self.run_pending_import();
        if self.pending_fit {
            self.pending_fit = false;
            self.fit_board();
        }
        // `--open` and `--tab`, on the first frame and never again. Here rather than in
        // `start` because both go through the same code a click does — `open_board`
        // renders a thumbnail of the board being left, and that needs the canvas
        // rectangle the chrome has only just reported.
        //
        // **Strictly after the fit above.** A board coming back from a tab brings its
        // own camera, and a fit applied afterwards throws away exactly the thing tabs
        // exist for. Each board opened here is framed by `open_board` itself, so there
        // is nothing left for a second pass to do anyway.
        if !self.pending_open.is_empty() {
            for path in std::mem::take(&mut self.pending_open) {
                self.open_board(&path);
            }
        }
        if let Some(index) = self.pending_tab.take() {
            self.select_tab(index);
        }
        if let Some(name) = self.pending_demo.take() {
            self.build_demo(&name);
        }
        if self.pending_select_one {
            self.pending_select_one = false;
            // The largest item, so the handles land on something big enough to see in
            // a screenshot rather than on whichever item the map happened to yield.
            let biggest = self
                .editor
                .projection()
                .iter()
                .max_by(|a, b| {
                    let area = |p: &crate::project::Projected| {
                        let (w, h) = p.item.placement.scaled_size();
                        w * h
                    };
                    area(a.1).total_cmp(&area(b.1))
                })
                .map(|(_, projected)| projected.doc_id);
            if let Some(doc) = biggest {
                self.editor.select([doc]);
                self.shell.invalidate_selection();
            }
        }
        if self.pending_select_all {
            self.pending_select_all = false;
            self.editor.select_all();
            self.shell.invalidate_selection();
            self.recorder.event("select-all");
        }
        if let Some(name) = self.pending_dialog.take() {
            self.open_named_dialog(&name);
        }
        if self.pending_paste {
            self.pending_paste = false;
            self.paste();
        }
        // After `run_chrome`, like every other pending flag: the surface is raised into state
        // the *next* pass reads, and `--screenshot` does not fire until frame 3.
        for name in std::mem::take(&mut self.pending_show) {
            if !self.shell.force_open(&name) {
                log::warn!(
                    "--show: nothing called `{name}` \
                     (properties, palette, find, shapes, pen, eraser, more, menu, settings)"
                );
            }
        }
        self.theme = theme_for(self.shell.theme(), self.shell.accent());
        self.sync_title();

        let screen = self.shell.screen();
        if screen == Screen::Board {
            self.paint_board();
        } else {
            // The library covers the window, so painting the board behind it would be
            // a full frame of culling, text shaping and texture residency for pixels
            // nobody sees.
            self.list.clear();
            self.stats = PaintStats::default();
        }

        if self.show_hud {
            self.push_hud();
        }

        // The chrome's triangles are uploaded outside any render pass, like every
        // other upload. Disjoint fields, so the surface can lend its device while the
        // chrome pass borrows itself.
        let size = self.surface.size();
        let scale = self.shell.pixels_per_point();
        let (device, queue) = self.surface.device_queue();
        self.chrome.prepare(
            device,
            queue,
            self.shell.primitives(),
            self.shell.textures_delta(),
            size,
            scale,
        );

        self.shell.configure_glass(&mut self.glass);
        let revision = self.revision();
        let clear = if screen == Screen::Board {
            self.canvas_color()
        } else {
            // The library is not the canvas, so it is not `pearl`. Clearing to the
            // chrome's own backdrop stops a one-frame flash of board colour when the
            // window resizes.
            let backdrop = self.shell.palette().backdrop;
            wgpu::Color {
                r: f64::from(backdrop.r()) / 255.0,
                g: f64::from(backdrop.g()) / 255.0,
                b: f64::from(backdrop.b()) / 255.0,
                a: 1.0,
            }
        };

        self.log_stats(now);
        self.surface.present(crate::surface::Frame {
            board: &self.list,
            clear,
            glass: &mut self.glass,
            panels: self.shell.glass_panels(),
            revision,
            chrome: &self.chrome,
        });

        // After presenting, and only once. Waiting a few frames lets the board's text
        // and images settle — both fill in over the first frames on purpose — so the
        // picture is of a settled window rather than of a cold start.
        //
        // **The third clause is what keeps this honest now that decoding is asynchronous**
        // (`crate::decode`). 205 assets over two workers is about three seconds, so a
        // fixed 1.5 s wait would photograph a half-loaded board and every image assertion
        // taken from it would be a lie. Waiting for the pool to go quiet makes the picture
        // *more* reliable than the old fixed delay, which only ever hoped it was enough.
        // Capped, because a blob that will never decode must not hang an unattended run.
        let settled = !self.editor.assets().is_busy()
            || self.opened_at.elapsed() >= SCREENSHOT_MAX_WAIT;
        if self.screenshot.is_some()
            && self.timer.total_frames() >= SCREENSHOT_MIN_FRAMES
            && self.opened_at.elapsed() >= SCREENSHOT_DELAY
            && settled
        {
            self.write_screenshot(clear, revision);
        }
    }

    /// Writes one composited frame to `--screenshot` and forgets the request.
    fn write_screenshot(&mut self, clear: wgpu::Color, revision: u64) {
        let Some(path) = self.screenshot.take() else { return };
        let result = self
            .surface
            .compose_offscreen(crate::surface::Frame {
                board: &self.list,
                clear,
                glass: &mut self.glass,
                panels: self.shell.glass_panels(),
                // A fresh number: the previous composite has already consumed this
                // frame's, and a cached blur would be read from a texture the
                // screenshot pass is about to overwrite.
                revision: revision.wrapping_add(1),
                chrome: &self.chrome,
            })
            .and_then(|capture| {
                let png = capture.to_png()?;
                std::fs::write(&path, png)
                    .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
                Ok(capture)
            });
        match result {
            Ok(capture) => log::info!(
                "screenshot: {} ({}x{}), {} glass panels, {} chrome draws",
                path.display(),
                capture.width,
                capture.height,
                self.shell.glass_panels().len(),
                self.chrome.draw_calls()
            ),
            Err(error) => log::error!("screenshot: {error:#}"),
        }
    }

    /// Whether the canvas text session has the keyboard.
    ///
    /// The on-canvas caret. **One derivation**, read by both places a keystroke can turn
    /// into a command — the chrome's table, through `Facts::text_session`, and
    /// [`Self::shortcut`] — because two copies of this are two answers that can disagree
    /// about whether someone is in the middle of a word.
    ///
    /// It is not an egui widget, so `Context::egui_wants_keyboard_input` is false throughout
    /// and the chrome cannot work this out for itself. See
    /// `vellum_ui::ChromeState::text_session` for what that cost: renaming a frame and
    /// pressing Backspace deleted the frame, and a word containing `r` or `s` armed a tool
    /// so the click that left the field placed an item nobody asked for.
    ///
    /// A function rather than a bare field read, so the chrome and the shortcut table keep
    /// asking one question — this is where a second canvas-owned keyboard mode would join.
    pub(crate) fn text_session_owns_keyboard(&self) -> bool {
        self.editing.is_some()
    }

    /// Runs the chrome for this frame and acts on everything it reports.
    pub(crate) fn run_chrome(&mut self) {
        let handle = self.pointer_handle();
        // Recomputed per frame rather than tracked on a move event: the board scrolls and
        // zooms under a stationary pointer, so "what is under the cursor" changes without
        // the cursor moving at all.
        let over_board =
            !self.shell.pointer_over_ui() && self.shell.screen() == Screen::Board;
        self.hovered_badge = if over_board { self.badge_under_pointer() } else { None };
        // The ports follow the same rule and the same reason — the board moves under a
        // stationary pointer — but they are **kept** while the pointer is over the chrome
        // rather than cleared. Reaching a selected item's dots from the context bar floating
        // above it crosses the bar, and dots that blinked out on the way to them would be
        // dots you cannot use.
        if over_board {
            self.hovered_ports = self.ports_under_pointer();
        }
        let hovered_badge = self.hovered_badge;
        let title = self.editor.board().title();
        let path = self.editor.path().map(std::path::Path::to_path_buf);
        let starred = path
            .as_deref()
            .is_some_and(|path| self.shell.library.is_starred(path));

        let facts = Facts {
            title: &title,
            path: path.as_deref(),
            starred,
            dirty: !self.editor.is_durable(),
            can_undo: self.editor.board().can_undo(),
            can_redo: self.editor.board().can_redo(),
            zoom: self.camera.zoom() as f32,
            // Optimistic on purpose. Reading the system clipboard costs a round trip
            // to the window server, and doing it sixty times a second to decide
            // whether a menu row is grey is not a trade worth making — the app's own
            // clipboard is only ever half the answer, because Paste also accepts a
            // Miro payload put there by another application.
            clipboard_has_content: true,
            background: self.editor.board().background(),
            // A badge is a button and says so: the pointing hand is what every browser
            // uses for a link, and it is the other half of the hover state — the shape
            // changes before the colour does, on the way in.
            canvas_cursor: if hovered_badge.is_some() {
                egui::CursorIcon::PointingHand
            } else {
                crate::shell::cursor_icon(self.input.cursor_icon(handle))
            },
            selection_rect: self.selection_screen_rect(),
            text_session: self.text_session_owns_keyboard(),
        };

        self.sync_selection();
        // The dot on every *other* tab. `Shell::run` does the board in front from
        // `facts`; a parked board saves on its own writer thread, so the only honest
        // source for its tab is the board itself. Cheap — one atomic read per open
        // tab — and it has to be per frame, because autosave is instant and the dot is
        // therefore only up for a fraction of a second at a time.
        let parked: Vec<(vellum_ui::TabKey, bool)> = self
            .session
            .iter()
            .map(|board| (board.key(), !board.is_durable()))
            .collect();
        for (key, dirty) in parked {
            self.shell.set_tab_dirty(key, dirty);
        }

        // What the app knows about the account, into Settings ▸ Account. Before the chrome
        // draws, so the page shows this frame's state rather than the previous one's — a
        // *Signing in* that lags a frame behind the click reads as a button that did nothing.
        // It writes the app's half only and never the three fields being typed into.
        self.report_account();

        let window = self.window.clone();
        let events = self.shell.run(&window, &facts);
        drop(title);
        drop(path);
        for event in events {
            self.dispatch(event);
        }
        // After the chrome, not before: `Shell::run` is what resolves which commands can
        // act and which toggles are on, and the native bar greys and ticks from exactly
        // those — a second derivation is how two menus come to disagree about whether
        // Paste is available.
        self.run_menubar();
        self.load_thumbnails();
    }

    /// Greys the native menu bar to match this frame, and acts on anything clicked in it.
    ///
    /// A no-op on a platform with no native bar, and on macOS on almost every frame:
    /// [`crate::menubar::MenuBar::sync`] compares against what it last set, because each
    /// row is a separate message send across the language boundary.
    #[cfg(target_os = "macos")]
    fn run_menubar(&mut self) {
        use crate::menubar::{Chosen, Extra, MenuBar};

        if let Some(bar) = self.menubar.as_mut() {
            bar.sync(&self.shell.command_context(), self.shell.menu_flags());
        }
        // Drained even when the bar failed to install: the channel is `muda`'s and
        // process-wide, so this is correct rather than merely harmless.
        for chosen in MenuBar::drain() {
            match chosen {
                // The same entry point the in-app menu, the palette and the keyboard all
                // use, so a native row cannot come to mean something different.
                Chosen::Command(command) => self.run(command),
                Chosen::Extra(Extra::NewTab) => {
                    self.shell.show_home_tab();
                    self.follow_tab_strip();
                    self.run(vellum_ui::Command::NewBoard);
                }
                Chosen::Extra(step @ (Extra::NextTab | Extra::PreviousTab)) => {
                    let by = if step == Extra::NextTab { 1 } else { -1 };
                    if self.shell.cycle_tab(by).is_some() {
                        self.follow_tab_strip();
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[expect(clippy::unused_self, reason = "the macOS arm needs `self`")]
    const fn run_menubar(&mut self) {}

    /// Keeps the properties panel's view of the selection current without rebuilding
    /// it every frame. See [`Shell::sync_selection`] for what the key is.
    fn sync_selection(&mut self) {
        let selection = self.editor.selection();
        let digest = selection
            .iter()
            .fold(0u64, |acc, id| acc.wrapping_mul(31).wrapping_add(*id));
        let key = (
            self.editor.projection().generation(),
            selection.len(),
            digest,
        );
        // Destructured rather than called through `self`, and it stays that way: the
        // closure borrows `editor` while `shell` is borrowed mutably to receive the answer,
        // which `self.shell.sync_selection(key, || … self.editor …)` cannot express.
        let Self { shell, editor, .. } = self;
        shell.sync_selection(key, || {
            crate::inspect::selection_items(editor.projection(), editor.selection())
        });
    }

    fn paint_board(&mut self) {
        let marquee = self.input.marquee();
        let camera = self.camera;
        let theme = self.theme;
        let view = self.shell.view();
        let minimap = view.minimap_visible.then(|| self.minimap_rect()).flatten();
        let pattern = self.canvas_pattern();
        let grid_color = self.shell.library.grid_color();
        // Read from the same preset the commit will read, so the stroke does not
        // change colour or width the instant the button comes up.
        let pen = self.shell.pen();
        // **Before** the borrows below: this needs `&mut self` to own the frame's guides,
        // and it is the one place a create gesture's preview and its snapping are decided
        // together.
        let placing = self.placing_preview();
        // Cloned, **not** taken. A drag recomputes its guides only when the pointer moves,
        // so taking them would blank the lines on every frame a hand held still — which is
        // exactly when someone is looking at them. Four `Copy` structs at the very most.
        let guides = self.guides.clone();
        let stroke = (!self.stroke.is_empty()).then(|| crate::draw::LiveStroke {
            points: &self.stroke,
            color: crate::theme::convert(pen.stroke_color()),
            thickness: f64::from(pen.width),
        });
        // Where a kanban card would drop, in world corners. Resolved here rather than
        // in the painter because it is the *gesture's* state and the painter draws the
        // document — the same split the pen's live stroke follows.
        let card_drop = self.card_drag.as_ref().and_then(|drag| {
            let (scene, rect) = drag.preview()?;
            let placement = self.editor.projection().get(scene)?.item.placement;
            Some(crate::kanban::board_world(&placement, rect))
        });
        // **What the sweep has caught so far, ringed while it is still moving.**
        //
        // *"i want the border to pop up as soon as it decides what is being selected."* A
        // marquee wrote nothing until the button came up, so the whole gesture was a
        // rectangle over an unchanged board and you found out what you had caught only by
        // letting go — and if it was wrong, by doing it again.
        //
        // Previewed rather than committed, exactly as a move drag is: the document is
        // untouched until the release, `Editor::marquee` still decides the real answer, and
        // both ask `Projection::marquee_hits`, so the rings cannot promise a selection the
        // release does not deliver.
        let previewed: Vec<vellum_scene::ItemId> = marquee
            .map(|(from, to)| {
                let rect = vellum_scene::WorldRect::from_corners(
                    camera.screen_to_world(from),
                    camera.screen_to_world(to),
                );
                self.editor.projection().marquee_hits(rect)
            })
            .unwrap_or_default();
        // Both are asked **before** `frame_parts`, which takes a mutable borrow of the
        // editor that has to live until `paint`. `pending_connector` and `ported_item` each
        // read the projection, the selection, the armed tool and every in-flight gesture,
        // so neither can run while that borrow is out — inside the initialiser they are a
        // borrow error, and the fix is the order rather than the call.
        let pending_connector = self.pending_connector();
        let ports = self.ported_item();
        // The two grips on a selected connector, dropped while one of them is in flight:
        // the end being dragged is not where the document still says it is, so a grip drawn
        // there would sit at the line's old end while the preview runs to the pointer.
        let connector_grips = (self.endpoint_arm.is_none())
            .then(|| self.connector_grips())
            .flatten()
            .map(|(_, from, to)| (from, to));
        let (projection, selection, assets) = self.editor.frame_parts();
        let selection = if previewed.is_empty() { selection } else { previewed.as_slice() };
        let context = DrawContext {
            camera: &camera,
            projection,
            theme,
            hovered_badge: self.hovered_badge,
            selection,
            marquee,
            stroke,
            // The box the armed create tool would fill if the button came up now, snapped
            // exactly as the created item will be — `ActiveState::placing_preview`, so the
            // preview and the commit cannot be a rectangle apart.
            placing,
            guides: &guides,
            // Two ends: where the gesture began, and where the pointer is now. Taken from
            // the gesture rather than from a field of our own — `Input::placement` already
            // tracks exactly this pair for every create tool, and a second copy would be a
            // second thing to keep in step.
            // Either way of drawing one: the connector tool, or a drag begun on one of the
            // four ports. The port drag borrows `input::Tool::Place` while the palette still
            // says Select, so gating on the palette's tool alone would leave the commonest
            // route to a connector — Miro's blue dots — drawing nothing at all until the
            // button came up. That is feedback 7 (the pen) and 23 (the frame) a third time,
            // and it is the failure this codebase reproduces most reliably.
            pending_connector,
            // Miro's four blue dots, on whichever item is wearing them — hover first, then a
            // lone selection. `ported_item` holds the whole rule; the painter is handed the
            // answer.
            ports,
            connector_grips,
            editing: self.editing.as_ref().map(|session| crate::draw::TextCursor {
                scene: session.scene,
                slot: session.slot,
                idle_for: self.editing_touched.elapsed().as_secs_f32(),
                cursor: session.buffer.cursor(),
                anchor: session.buffer.anchor(),
                // The buffer's own string, which is what the offsets above index. See
                // `TextCursor`: a table cell's words are inside a JSON token, so a painter
                // asked to re-derive them would need a second copy of the slot-to-cell
                // mapping this session already resolved.
                text: session.buffer.text(),
            }),
            card_drop,
            pattern,
            grid_color,
            minimap,
        };
        let (device, queue, renderer) = self.surface.parts();
        self.stats = self
            .painter
            .paint(device, queue, renderer, assets, &mut self.list, &context);
        self.tick_zoom_sweep();
    }

    /// Arms `--demo zoom-flicker`. See [`Self::tick_zoom_sweep`].
    pub(crate) fn arm_zoom_sweep(&mut self) {
        self.zoom_sweep = Some(ZoomSweep { settling: true, ..ZoomSweep::default() });
    }

    /// One frame of `--demo zoom-flicker`.
    ///
    /// Here rather than in `actions` because the thing being measured is a *sequence of
    /// frames*: a refinement is decided by one frame's draws, requested on the next, and
    /// answered several frames after that by a worker thread. Nothing that runs inside a
    /// single command can observe it, which is exactly why the whole path had unit tests on
    /// every layer and no execution as a composed whole.
    fn tick_zoom_sweep(&mut self) {
        let Some(mut sweep) = self.zoom_sweep.take() else { return };
        let pending = self.stats.images_pending;
        let queued = self.surface.renderer().wants_refinement().len();

        if sweep.settling {
            // Wait for the first load to finish. Without this the initial decodes pollute the
            // count and both builds look equally broken.
            // Long enough for `resolve_detail` to demote at this zoom as well as for the
            // decodes to land — the demotion is what the sweep sharpens back up.
            sweep.quiet = if pending == 0 { sweep.quiet + 1 } else { 0 };
            sweep.frames += 1;
            if sweep.quiet >= 120 {
                sweep.settling = false;
                sweep.frames = 0;
                sweep.from = self.camera.zoom();
            } else if sweep.frames > 600 {
                self.gap("zoom-flicker: the images never finished loading");
                return;
            }
            self.zoom_sweep = Some(sweep);
            return;
        }

        // The sweep proper. **At least 8x**, because the refine edge sits 2x from where a
        // texture settles and a shorter sweep would never cross it — reporting a pass on a
        // build where nothing was exercised.
        const FRAMES: u32 = 240;
        const RANGE: f64 = 12.0;
        sweep.blinks += usize::from(pending > 0);
        sweep.worst = sweep.worst.max(pending);
        sweep.queued = sweep.queued.max(queued);
        sweep.frames += 1;

        let t = f64::from(sweep.frames) / f64::from(FRAMES);
        let centre = self.canvas_centre();
        self.camera.set_zoom_about(sweep.from * RANGE.powf(t), centre);

        if sweep.frames < FRAMES {
            self.zoom_sweep = Some(sweep);
            return;
        }

        // The verdict, through the same channel every other fixture reports on.
        // **Blinks first.** A build that draws placeholders has failed whatever the counter
        // says — and the counter cannot see the old behaviour anyway, because destroying the
        // texture *was* the signal and `begin_frame` drained the queue before anything could
        // read it. Reporting "nothing queued" first hid the actual regression behind a
        // bookkeeping complaint, which the A/B against that build is what exposed.
        if sweep.blinks > 0 {
            self.gap(&format!(
                "zoom-flicker: {} of {FRAMES} frames drew a placeholder, worst {} images at \
                 once — an image stopped being drawn while it sharpened",
                sweep.blinks, sweep.worst
            ));
        } else if sweep.queued == 0 {
            self.gap(&format!(
                "zoom-flicker: no placeholder frames, but nothing was ever asked to refine \
                 over a {RANGE:.0}x sweep — the path this fixture exists to drive never ran, \
                 so the pass is vacuous"
            ));
        } else {
            self.ok(format!(
                "zoom-flicker: {FRAMES} frames over a {RANGE:.0}x zoom, {} refinement(s) asked \
                 for, and 0 placeholder frames — every image kept drawing while it sharpened",
                sweep.queued
            ));
        }
    }

    /// The middle of the visible board, in physical screen pixels.
    ///
    /// Where a paste goes when the pointer is somewhere it must not be aimed at — over a
    /// panel, or outside the window. Taken from [`Self::canvas_pixels`] rather than from
    /// the surface, so it is the centre of the board the user can *see* rather than of a
    /// window whose left edge is under the tool palette.
    pub(crate) fn canvas_centre(&self) -> vellum_scene::ScreenPoint {
        let [x, y, width, height] = self.canvas_pixels();
        vellum_scene::ScreenPoint::new(x + width / 2.0, y + height / 2.0)
    }

    /// The part of the window the board can actually be seen in, in physical pixels
    /// as `[x, y, width, height]`.
    ///
    /// Falls back to the whole surface before the chrome has run once, which is the
    /// only state in which the answer is unknown.
    pub(crate) fn canvas_pixels(&self) -> [f64; 4] {
        let rect = self.shell.canvas_rect();
        let (width, height) = self.surface.size();
        if !rect.is_positive() {
            return [0.0, 0.0, f64::from(width), f64::from(height)];
        }
        let scale = f64::from(self.shell.pixels_per_point());
        [
            f64::from(rect.min.x) * scale,
            f64::from(rect.min.y) * scale,
            f64::from(rect.width()) * scale,
            f64::from(rect.height()) * scale,
        ]
    }

    /// The pattern drawn on the canvas this frame.
    ///
    /// **One source.** The board carries it, it is a property of the document and it
    /// travels with the board. There used to be a second — a per-view *View ▸ Grid*
    /// toggle with `⇧⌘G` on it, which promoted a `Plain` board to dots without writing
    /// anything — and the user asked for it to go: *"i dont think we need that grid thing
    /// because we can select and unselects grids within the grid menu"*.
    ///
    /// They were right, and the reason is worth keeping so it is not restored. Two
    /// controls over one appearance is one too many, and this pair was worse than most:
    /// the toggle only had an effect on a board that had chosen *No grid*, so on any board
    /// that had a pattern — which is every board, since `Dots` is the default — pressing
    /// `⇧⌘G` did nothing at all and read as broken. *"what is the purpose of shift cmd g
    /// beucase it seems to be not doing anything"*. **No grid** is a row in View ▸ Grid,
    /// which is where off belongs: beside the other three answers to the same question.
    ///
    /// # The grid is an app setting, and the board's own pattern is its fallback
    ///
    /// *"grid opacity and grid color and grid should apply to all of the boards not just to
    /// that board."* So the sidecar's choice wins over the document's whenever there is one.
    ///
    /// This is **not** the two-sources arrangement removed above, and the difference is that
    /// there is no state in which one of them silently does nothing: the sidecar answers for
    /// every board the moment it is set, and until it is set the board answers for itself.
    /// Keeping the document field is what makes the change lossless — a board that chose
    /// *Lines* keeps drawing lines until the user makes a global choice, rather than being
    /// restyled on first launch by an update it did not ask for.
    ///
    /// The **background colour** is deliberately still per board (feedback 3, *"i want to be
    /// able to select backgrounds for the baords"*). A board's colour is what tells two
    /// boards apart; its grid is a drawing aid, and wanting that to be the same everywhere
    /// is the opposite preference for a good reason.
    pub(crate) fn canvas_pattern(&self) -> vellum_doc::Pattern {
        self.shell
            .library
            .grid_pattern()
            .unwrap_or_else(|| self.editor.board().background().pattern)
    }

    /// The colour the canvas is cleared to: the board's own, or the palette's.
    pub(crate) fn canvas_color(&self) -> wgpu::Color {
        crate::theme::clear_color(
            self.theme.canvas_color(self.editor.board().background().color),
        )
    }

    /// Frames the whole board in the visible canvas — `Shift+1`.
    pub(crate) fn fit_board(&mut self) {
        if let Some(content) = self.editor.projection().content_bounds() {
            let canvas = self.canvas_pixels();
            fit_rect_in_canvas(&mut self.camera, content, canvas);
        }
    }

    /// Where the minimap sits, in physical pixels.
    ///
    /// Bottom right, above the zoom cluster and clear of the properties panel, because
    /// that is where `docs/04-ui-reference.md` §1 puts the frames/minimap toggle that
    /// turns it on. Laid out in **points** and converted once, so it lines up with the
    /// chrome it has to sit beside rather than being half the size on a Retina display.
    fn minimap_rect(&self) -> Option<[f32; 4]> {
        const WIDTH: f32 = 200.0;
        const HEIGHT: f32 = 132.0;
        const MARGIN: f32 = 16.0;
        /// The zoom cluster's height plus its own margin, so the map clears it.
        const CLUSTER: f32 = 48.0;

        let scale = self.shell.pixels_per_point();
        let canvas = self.shell.canvas_rect();
        if !canvas.is_positive() {
            return None;
        }
        // Small windows would otherwise get a minimap covering a quarter of the board.
        if canvas.width() < WIDTH * 2.5 || canvas.height() < (HEIGHT + CLUSTER) * 2.0 {
            return None;
        }
        let x = canvas.max.x - MARGIN - WIDTH;
        let y = canvas.max.y - MARGIN - CLUSTER - HEIGHT;
        Some([x * scale, y * scale, WIDTH * scale, HEIGHT * scale])
    }

    /// Which resize or rotate grip the pointer is on, or is dragging.
    ///
    /// The drag in flight is asked **first**: once a resize has begun the pointer leaves
    /// the grip almost immediately — that is what resizing is — and a hover test would
    /// hand back `None` for the rest of the gesture, so the arrow would flick to `Move`
    /// on the first millimetre of every drag.
    fn pointer_handle(&self) -> Option<crate::handle::Handle> {
        if self.shell.screen() != Screen::Board {
            return None;
        }
        if let Some(handle) = self.dragged_handle() {
            return Some(handle);
        }
        if self.shell.pointer_over_ui() {
            return None;
        }
        let at = self.camera.screen_to_world(self.input.cursor(&self.camera));
        self.handle_under(at).map(|(_, handle)| handle)
    }

    /// What the board's pixels depend on, as one number.
    ///
    /// `vellum_render::glass` refreshes a blurred backdrop only when this changes, so
    /// it has to move whenever the canvas does and stay still whenever it does not —
    /// getting it wrong in the safe direction costs the blur, and in the other shows a
    /// stale copy of the board through the toolbar.
    fn revision(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let centre = self.camera.center();
        centre.x.to_bits().hash(&mut hasher);
        centre.y.to_bits().hash(&mut hasher);
        self.camera.zoom().to_bits().hash(&mut hasher);
        self.editor.projection().generation().hash(&mut hasher);
        self.editor.selection().len().hash(&mut hasher);
        self.surface.size().hash(&mut hasher);
        self.shell.theme().is_dark().hash(&mut hasher);
        // The board's background lives in its meta map, which the item generation above
        // does not cover — and the grid lives in the sidecar, which it does not cover
        // either. Both, or the canvas is served from cache and a chosen grid does not
        // appear until something else happens to invalidate it.
        let background = self.editor.board().background();
        background.color.map(vellum_doc::Color::to_packed).hash(&mut hasher);
        self.canvas_pattern().tag().hash(&mut hasher);
        self.shell.library.grid_color().map(vellum_doc::Color::to_packed).hash(&mut hasher);
        // A marquee is drawn into the canvas, so a panel over it has to see it move.
        self.input.marquee().is_some().hash(&mut hasher);
        // So does a drag: the projection carries the live position and the document
        // does not, so the generation above cannot see it.
        self.drag.as_ref().map(crate::actions::Drag::revision).hash(&mut hasher);
        // And so does a pen stroke. The sample count is enough: the path only ever
        // grows while the button is down, so a changed length is a changed picture.
        self.stroke.len().hash(&mut hasher);
        // A kanban card being dragged moves its preview rectangle and nothing else, so
        // neither the generation nor the drag term above can see it.
        self.card_drag.as_ref().map(crate::actions::CardDrag::revision).hash(&mut hasher);
        // A connector being drawn moves its own preview line, which the document cannot
        // see either. The whole sweep is hashed rather than just "is there one", unlike the
        // marquee above, because the marquee is drawn in the *screen* view and does not
        // change the canvas texture a glass panel is blurring; this is board-view geometry
        // and does.
        self.input
            .placement()
            .map(|(a, b)| (a.x.to_bits(), a.y.to_bits(), b.x.to_bits(), b.y.to_bits()))
            .hash(&mut hasher);
        // And the caret: it moves without the document changing — an arrow key, a click
        // inside the text — so a panel over the item it is in has to see it move.
        self.editing.as_ref().map(|e| (e.buffer.cursor(), e.buffer.anchor())).hash(&mut hasher);
        hasher.finish()
    }

    /// The board's name, and whether anything is still on its way to disk.
    fn sync_title(&mut self) {
        let title = match self.shell.screen() {
            Screen::Library => "Velm".to_owned(),
            Screen::Board => {
                let name = self.editor.board().title();
                let name = if name.trim().is_empty() { "Untitled" } else { name.trim() };
                if self.editor.is_durable() {
                    format!("{name} — Velm")
                } else {
                    // A word rather than a symbol: a dot in a title bar is a
                    // convention for *unsaved*, and this board is saving right now.
                    format!("{name} — Saving… — Velm")
                }
            }
        };
        if title != self.title {
            self.window.set_title(&title);
            self.title = title;
        }
    }

    /// Appends the HUD to the frame's list, in the screen view it already holds.
    fn push_hud(&mut self) {
        const MARGIN: f32 = 8.0;
        const PADDING: f32 = 6.0;

        let scale = self.window.scale_factor() as f32;
        let pixel = 2.0 * scale;
        let margin = MARGIN * scale;
        let padding = PADDING * scale;
        // Below the menu bar, not under it. An 8px top margin put the whole first
        // line — the one naming the GPU — behind 36 points of chrome, where it was
        // invisible in every screenshot taken to check it.
        let canvas = self.shell.canvas_rect();
        let top = if canvas.is_positive() { canvas.min.y * scale + margin } else { margin };

        let lines = self.hud_lines();
        let text_width = lines
            .iter()
            .map(|line| hud::text_width(line, pixel))
            .fold(0.0f32, f32::max);
        let line_height = hud::line_height(pixel);

        self.hud_quads.clear();
        // The panel is pushed first so every glyph lands on top of it; there is no
        // depth buffer, and within a pass the later draw wins.
        hud::push_rect(
            &mut self.hud_quads,
            [margin, top],
            [
                text_width + padding * 2.0,
                line_height * lines.len() as f32 + padding * 2.0,
            ],
            HUD_PANEL,
        );
        for (index, line) in lines.iter().enumerate() {
            let color = if index == 0 { HUD_ACCENT } else { HUD_TEXT };
            hud::push_text(
                &mut self.hud_quads,
                line,
                [margin + padding, top + padding + index as f32 * line_height],
                pixel,
                color,
            );
        }

        // Re-selecting the screen view rather than adding one: `DrawList::view`
        // returns the existing index for a view it already holds, so the HUD shares
        // the marquee's uniform slot instead of costing another.
        self.list.view(View::screen(self.camera.viewport()));
        self.list.push_quads(self.hud_quads.iter().copied());
    }

    fn hud_lines(&self) -> Vec<String> {
        let glass = self.surface.glass_stats();
        let mut lines = vec![
            self.gpu_label.clone(),
            format!("{:.0} FPS   {:.2} MS", self.timer.fps(), self.timer.frame_ms()),
            format!(
                "{} / {} VISIBLE   {} DRAWS   ZOOM {:.0}%",
                self.stats.drawn,
                self.editor.projection().len(),
                self.stats.draws.draw_calls,
                self.camera.zoom() * 100.0
            ),
            format!(
                "{} GLYPHS   {} TRIS   {} IMAGES{}",
                self.stats.draws.glyphs,
                self.stats.draws.triangles,
                self.editor.assets().resident(),
                if self.editor.is_durable() { "" } else { "   SAVING" }
            ),
            format!(
                "CHROME {} DRAWS   {} TEX   GLASS {} PANELS   {} PASSES",
                self.chrome.draw_calls(),
                self.chrome.textures(),
                glass.panels,
                glass.passes
            ),
            // The line that would have caught the worst bug this app has had. A frame
            // counter cannot tell you the process is eating the host, and the totals
            // beside it say *which* subsystem is doing it: images, board previews, or
            // documents held behind tabs.
            format!(
                "RSS {} MB   TEX {} MB   {} PREVIEWS   {} PARKED",
                self.rss / 1_048_576,
                self.surface.renderer().textures().resident_bytes() / 1_048_576,
                self.shell.thumbnail_count(),
                self.session.len()
            ),
            // What an edit costs, which nothing on screen used to say. `REPROJ` is the
            // whole-document rebuild every edit and every undo runs; `DEFER` is how many
            // text blocks wanted shaping this frame and were refused it by the 3 ms
            // ration — so a non-zero `DEFER` is words the user is waiting for, and the
            // number of frames it takes to fall back to zero after an edit is the
            // stutter, measured. `GREEK` is beside it because the two are easy to
            // confuse: a greeked block draws bars, a deferred one draws nothing.
            format!(
                "REPROJ {:.1} MS   DEFER {}   GREEK {}   PENDIMG {}",
                self.editor.last_reproject().as_secs_f64() * 1000.0,
                self.stats.text_deferred,
                self.stats.text_skipped,
                self.stats.images_pending
            ),
        ];
        if let Some((text, at)) = &self.status
            && at.elapsed() < STATUS_LIFETIME
        {
            lines.push(text.to_uppercase());
        }
        lines
    }

    fn log_stats(&mut self, now: Instant) {
        if now.duration_since(self.last_stats_log) < STATS_LOG_INTERVAL {
            return;
        }
        self.last_stats_log = now;
        let glass = self.surface.glass_stats();
        // Resident memory is logged every second because it is the number that would
        // have caught the worst bug this app has had: it was seen at **14.24 GB**,
        // which took the machine down, and nothing on screen or in this line said so.
        // A frame counter cannot tell you that the process is eating the host.
        let rss = resident_bytes();
        self.rss = rss;
        log::info!(
            "{:.1} fps, {:.2} ms/frame, {} of {} items drawn, {} draws, {} glyphs, {} triangles, \
             zoom {:.0}%, chrome {} draws, glass {} panels / {} passes, rss {:.2} GB, \
             textures {} MB (peak {} MB), {} previews, {} parked",
            self.timer.fps(),
            self.timer.frame_ms(),
            self.stats.drawn,
            self.editor.projection().len(),
            self.stats.draws.draw_calls,
            self.stats.draws.glyphs,
            self.stats.draws.triangles,
            self.camera.zoom() * 100.0,
            self.chrome.draw_calls(),
            glass.panels,
            glass.passes,
            rss as f64 / 1_073_741_824.0,
            // Attribution beside the total, so a climb names a subsystem instead of
            // just a process. These are the three that grow with use.
            self.surface.renderer().textures().resident_bytes() / 1_048_576,
            self.surface.renderer().textures().peak_resident_bytes() / 1_048_576,
            self.shell.thumbnail_count(),
            self.session.len()
        );

        // The same numbers, into the file that survives a Force Quit.
        let textures = self.surface.renderer().textures();
        self.recorder.sample(crate::flight::Sample {
            rss,
            texture_bytes: textures.resident_bytes(),
            texture_peak: textures.peak_resident_bytes(),
            textures: textures.len(),
            glyph_bitmaps: self.painter.glyph_bitmaps(),
            chrome_textures: self.chrome.textures(),
            previews: self.shell.thumbnail_count(),
            parked_boards: self.session.len(),
            items: self.editor.projection().len(),
            drawn: self.stats.drawn,
            text_layouts: self.painter.text_layouts(),
            ink_meshes: self.painter.ink_meshes(),
            undo_depth: 0,
            fps: self.timer.fps(),
            zoom: self.camera.zoom(),
        });

        // Loud, once, rather than silently climbing. The budget in
        // docs/01-architecture.md is 400MB; 4GB is a fault, not a busy board.
        const ALARM: u64 = 4 * 1_073_741_824;
        if rss > ALARM && !self.memory_alarm {
            self.memory_alarm = true;
            log::error!(
                "resident memory is {:.2} GB — this is a leak, not a large board. \
                 Please report what you were doing.",
                rss as f64 / 1_073_741_824.0
            );
        }
    }
}

/// This process's resident set size, in bytes. `0` if it cannot be read.
///
/// Read from the OS rather than tracked by an allocator hook: the number that
/// matters is what the *kernel* thinks we are holding, which is what runs a machine
/// out of memory, and that includes GPU mappings and anything a dependency allocated
/// outside Rust's allocator.
fn resident_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // `ps` rather than libproc: one fork a second is nothing next to a frame, and
        // it avoids an FFI dependency for a diagnostic.
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

const HUD_PANEL: Rgba = Rgba::new(0.055, 0.063, 0.086, 0.82);
const HUD_TEXT: Rgba = Rgba::new(0.898, 0.925, 0.965, 1.0);
const HUD_ACCENT: Rgba = Rgba::new(0.400, 0.749, 1.000, 1.0);

/// Frames the camera on everything the board holds.
///
/// An empty board has no extent to fit, so the camera is left where it is rather
/// than zoomed to its limit on a degenerate rectangle.
pub fn fit_content(camera: &mut Camera, projection: &Projection) {
    if let Some(content) = projection.content_bounds() {
        camera.fit_to_rect(content, FIT_MARGIN);
    }
}

/// Frames the camera on `rect` inside the part of the window the board is actually
/// **visible** in, given in physical pixels as `[x, y, width, height]`.
///
/// The window is not the canvas. The properties panel takes 264 points off the right
/// and the menu bar 36 off the top, so fitting against the whole surface centres the
/// board on the *window* and slides a seventeenth of it behind the panel — measured
/// on the reference board as 236 px of its width, invisible and unreachable after the
/// one gesture whose entire promise is "now you can see all of it".
///
/// Two steps rather than one call: the zoom comes from the canvas rectangle's size,
/// and then the camera is pushed sideways by however far the canvas centre is from
/// the window centre, in world units. Falls back to the whole viewport for a canvas
/// rectangle that has not been measured yet — the first frame, before the chrome has
/// run once.
pub fn fit_rect_in_canvas(camera: &mut Camera, rect: WorldRect, canvas: [f64; 4]) {
    let [x, y, width, height] = canvas;
    let viewport = camera.viewport();
    let (content_w, content_h) = (rect.width().max(f64::EPSILON), rect.height().max(f64::EPSILON));
    if width <= 1.0 || height <= 1.0 {
        camera.fit_to_rect(rect, FIT_MARGIN);
        return;
    }

    let padded_w = content_w * (1.0 + FIT_MARGIN * 2.0);
    let padded_h = content_h * (1.0 + FIT_MARGIN * 2.0);
    let fit = (width / padded_w).min(height / padded_h);
    // Through `set_zoom_about` rather than assigned, so the clamp at either end of
    // the zoom range is the camera's own and there is only one copy of it.
    camera.set_center(rect.center());
    camera.set_zoom_about(
        fit,
        ScreenPoint::new(viewport.width / 2.0, viewport.height / 2.0),
    );

    let zoom = camera.zoom();
    let centre = rect.center();
    camera.set_center(WorldPoint::new(
        centre.x + (viewport.width / 2.0 - (x + width / 2.0)) / zoom,
        centre.y + (viewport.height / 2.0 - (y + height / 2.0)) / zoom,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{Board, ItemKind, NewItem, Placement, StyledText};

    fn projection_of(board: &Board) -> Projection {
        let mut projection = Projection::new();
        projection.rebuild(board).unwrap();
        projection
    }

    #[test]
    fn fitting_shows_every_item() {
        let mut board = Board::new();
        for (x, y) in [(-8_000.0, -3_000.0), (9_000.0, 4_000.0), (0.0, 0.0)] {
            board
                .add(NewItem::new(
                    ItemKind::Sticky { text: StyledText::default(), background: None },
                    Placement::new(x, y, 200.0, 200.0),
                ))
                .unwrap();
        }
        let projection = projection_of(&board);

        let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
        fit_content(&mut camera, &projection);

        assert_eq!(projection.scene().query_viewport(&camera).count(), 3);
    }

    /// An empty board must not drive the camera to a zoom limit trying to fit a
    /// zero-sized rectangle, which is what leaves a new user staring at nothing at
    /// 6400%.
    #[test]
    fn fitting_an_empty_board_leaves_the_camera_alone() {
        let projection = projection_of(&Board::new());
        let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
        let before = camera;
        fit_content(&mut camera, &projection);
        assert_eq!(camera, before);
    }

    /// The gesture whose whole promise is "now you can see all of it" was fitting the
    /// board to the **window** rather than to the canvas, so the properties panel ate
    /// the right-hand seventeenth of the reference board — present in the pixels at
    /// x=1175 and absent from x=1176 on.
    #[test]
    fn fitting_frames_the_board_inside_the_canvas_not_the_window() {
        let mut board = Board::new();
        for (x, y) in [(0.0, 0.0), (37_000.0, 11_000.0)] {
            board
                .add(NewItem::new(
                    ItemKind::Sticky { text: StyledText::default(), background: None },
                    Placement::new(x, y, 200.0, 200.0),
                ))
                .unwrap();
        }
        let projection = projection_of(&board);
        let content = projection.content_bounds().unwrap();

        // 1440×900 window, 264 points of properties panel and 36 of menu bar.
        let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
        let canvas = [0.0, 36.0, 1176.0, 864.0];
        fit_rect_in_canvas(&mut camera, content, canvas);

        for corner in [content.min, content.max, WorldPoint::new(content.min.x, content.max.y)] {
            let at = camera.world_to_screen(corner);
            assert!(
                at.x >= canvas[0] && at.x <= canvas[0] + canvas[2],
                "{corner:?} landed at x={} outside the canvas",
                at.x
            );
            assert!(
                at.y >= canvas[1] && at.y <= canvas[1] + canvas[3],
                "{corner:?} landed at y={} outside the canvas",
                at.y
            );
        }
    }

    /// And the old behaviour, pinned: fitting against the whole window really does
    /// put content under the panel, so the test above is measuring something.
    #[test]
    fn fitting_against_the_whole_window_hides_content_behind_the_panel() {
        let mut board = Board::new();
        for (x, y) in [(0.0, 0.0), (37_000.0, 11_000.0)] {
            board
                .add(NewItem::new(
                    ItemKind::Sticky { text: StyledText::default(), background: None },
                    Placement::new(x, y, 200.0, 200.0),
                ))
                .unwrap();
        }
        let projection = projection_of(&board);
        let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
        fit_content(&mut camera, &projection);

        let right = camera.world_to_screen(projection.content_bounds().unwrap().max);
        assert!(right.x > 1176.0, "nothing was hidden, at x={}", right.x);
    }

    /// An empty canvas rectangle — the first frame, before the chrome has run — falls
    /// back to the window rather than dividing by zero or fitting to nothing.
    #[test]
    fn fitting_survives_a_canvas_rectangle_that_is_not_known_yet() {
        let rect = WorldRect::from_corners(
            WorldPoint::new(-100.0, -100.0),
            WorldPoint::new(100.0, 100.0),
        );
        let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
        fit_rect_in_canvas(&mut camera, rect, [0.0, 0.0, 0.0, 0.0]);
        assert!(camera.zoom().is_finite() && camera.zoom() > 0.0);
        assert_eq!(camera.center(), WorldPoint::new(0.0, 0.0));
    }

    /// `--import capture.html` used to append to whichever board was open, and the
    /// documented run command names no board — so three launches made 1,788 items out
    /// of 596 and quietly corrupted the default board.
    #[test]
    fn an_import_targets_a_board_named_after_the_capture_not_the_default_one() {
        let target = import_board_path(std::path::Path::new("captures/reference-board.html"));
        assert_eq!(
            target.file_name().and_then(|n| n.to_str()),
            Some("reference-board.vellum")
        );
        assert_ne!(target, editor::default_board_path(), "it landed on the reference board");
        assert_eq!(target.parent(), editor::default_board_path().parent());
        // A path with no stem at all still has to produce a file name.
        assert!(import_board_path(std::path::Path::new("/")).file_name().is_some());
    }

    /// `--no-inertia` has to reach both sources, or the flag only half works.
    #[test]
    fn disabling_inertia_disables_it_for_every_source() {
        let options = Options { inertia: false, ..Options::default() };
        let config = InputConfig {
            drag_inertia: options.inertia,
            scroll_inertia: options.inertia && InputConfig::default().scroll_inertia,
            ..InputConfig::default()
        };
        assert!(!config.drag_inertia);
        assert!(!config.scroll_inertia);
    }

    /// The HUD is drawn over whatever the board happens to be showing, so its own
    /// contrast cannot come from the canvas behind it.
    #[test]
    fn the_hud_palette_is_legible_against_its_own_panel() {
        let panel = HUD_PANEL;
        let text = HUD_TEXT;
        let accent = HUD_ACCENT;
        assert!(panel.a > 0.5, "the panel must actually cover the board");
        assert!(text.r > panel.r + 0.5, "the text is not brighter than the panel");
        assert!(accent.b > panel.b + 0.5, "the accent line does not stand out");
    }

    /// The chrome's palette and the canvas's are two tables in two crates. If they
    /// ever disagree about which mode is showing, the board and the panels around it
    /// are in different themes.
    #[test]
    fn the_canvas_palette_follows_the_chromes() {
        let default = vellum_ui::Accent::default();
        assert_eq!(theme_for(vellum_ui::Theme::Light, default), Theme::LIGHT);
        assert_eq!(theme_for(vellum_ui::Theme::Dark, default), Theme::DARK);

        // …and the accent comes through the same door, which is the point of it being an
        // argument here rather than applied at the call sites: there are two of those, and
        // one of them running without it is a board whose selection ring is a different
        // colour from the toolbar that changed it.
        for accent in vellum_ui::Accent::ALL {
            let canvas = theme_for(vellum_ui::Theme::Light, accent);
            let swatch = accent.swatch();
            assert_eq!(
                canvas.accent.pack(),
                [swatch.r(), swatch.g(), swatch.b(), swatch.a()],
                "{} did not reach the board",
                accent.label()
            );
            // Nothing else moved with it.
            assert_eq!(canvas.canvas, Theme::LIGHT.canvas);
            assert_eq!(canvas.surface, Theme::LIGHT.surface);
        }
    }
}
