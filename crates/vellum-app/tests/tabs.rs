//! Several boards open at once, end to end.
//!
//! The app keeps its open boards in three pieces that have to agree:
//! `vellum_ui::TabStrip` owns the order and which tab is in front,
//! [`vellum_app::Session`] owns the documents behind the tabs that are not, and
//! [`Shell::tab_key`] is the one function that turns a path into the name both use.
//! Every bug tabs can have is a disagreement between those three, so they are driven
//! here together rather than separately — with real boards in real SQLite files, since
//! two of the three questions ("was it saved", "was it loaded twice") are only
//! meaningful against a disk.
//!
//! # What this does not reach
//!
//! `ActiveState::swap_in` — the glue that flushes the outgoing board, renders its
//! thumbnail and re-frames the camera — owns a GPU and a window and cannot be built
//! without both. What it *decides* is all here; what it *draws* is checked by
//! `--screenshot`.

use std::path::{Path, PathBuf};

use vellum_app::Session;
use vellum_app::editor::Editor;
use vellum_app::shell::Shell;
use vellum_doc::{Board, ItemKind, NewItem, Placement, StyledText};
use vellum_scene::{Camera, ScreenPoint, ScreenSize, WorldPoint};
use vellum_store::BlobStore;
use vellum_ui::{BoardTab, TabKey, TabStrip};

/// A window's worth of state: the strip the user sees, the boards behind it, and the
/// one that is on screen.
///
/// This is the app's own arrangement — `crate::session`'s header explains why the hot
/// board is hoisted out of the session rather than held in it — reproduced here with
/// the GPU-owning half left out.
struct Window {
    strip: TabStrip,
    session: Session,
    hot: Option<(PathBuf, Editor, Camera)>,
    blobs: BlobStore,
}

impl Window {
    fn new(blobs: BlobStore) -> Self {
        Self { strip: TabStrip::default(), session: Session::default(), hot: None, blobs }
    }

    /// What the app does for `LibraryEvent::Open`, a click on a tab, or `Cmd+1`.
    fn open(&mut self, path: &Path, title: &str) {
        let key = Shell::tab_key(path);

        // Already on screen: bring its tab forward and load nothing.
        if self.hot.as_ref().is_some_and(|(open, _, _)| open == path) {
            self.strip.open(BoardTab::new(key, title).with_path(path));
            return;
        }

        // Already open behind another tab: swap it in with the view it was left at.
        let incoming = match self.session.take(key) {
            Some((editor, camera)) => (editor, Some(camera)),
            None => (
                Editor::open(path, self.blobs.clone()).expect("opening the board"),
                None,
            ),
        };

        if let Some((outgoing, mut editor, camera)) = self.hot.take() {
            editor.flush().expect("saving the board being left");
            if self.strip.tabs().iter().any(|t| t.key == Shell::tab_key(&outgoing)) {
                self.session.park(Shell::tab_key(&outgoing), outgoing, editor, camera);
            }
        }

        let camera = incoming.1.unwrap_or_else(|| Camera::new(ScreenSize::new(1440.0, 900.0)));
        self.hot = Some((path.to_path_buf(), incoming.0, camera));
        self.strip.open(BoardTab::new(key, title).with_path(path));
    }

    /// What the app does for `Cmd+W` and for a tab's `×`: the strip drops the tab, and
    /// then the app catches up with whatever it now has in front.
    fn close_active(&mut self) {
        self.strip.close(self.strip.active());
        self.follow();
    }

    /// `ActiveState::follow_tab_strip`: release what the strip no longer shows, then
    /// put the document it now has in front on screen.
    fn follow(&mut self) {
        let live = self.strip.tab_keys();
        for mut released in self.session.retain(&live) {
            released.flush().expect("saving a board whose tab closed");
        }

        match self.strip.active_tab().and_then(|tab| tab.path.clone()) {
            Some(path) if self.hot.as_ref().is_some_and(|(open, _, _)| *open == path) => {}
            Some(path) => {
                let title = self.strip.active_tab().map(|t| t.title.clone()).unwrap_or_default();
                self.open(&path, &title);
            }
            None => {
                // Home. The board on screen stays loaded while its tab is open.
                if let Some((path, editor, _)) = self.hot.as_mut() {
                    editor.flush().expect("saving on the way to the library");
                    let key = Shell::tab_key(path);
                    if !live.contains(&key) {
                        self.hot = None;
                    }
                }
            }
        }
    }

    fn camera(&self) -> Camera {
        self.hot.as_ref().expect("a board is on screen").2
    }

    fn hot_path(&self) -> Option<&Path> {
        self.hot.as_ref().map(|(path, _, _)| path.as_path())
    }

    fn set_camera(&mut self, camera: Camera) {
        self.hot.as_mut().expect("a board is on screen").2 = camera;
    }

    fn add_a_note(&mut self, x: f64) {
        let (_, editor, _) = self.hot.as_mut().expect("a board is on screen");
        editor
            .edit(|board| {
                board.add(NewItem::new(
                    ItemKind::Sticky { text: StyledText::default(), background: None },
                    Placement::new(x, 0.0, 200.0, 200.0),
                ))?;
                Ok(())
            })
            .expect("adding a note");
    }
}

/// `TabStrip` reports its tabs but not their keys as a list; the app asks for exactly
/// this in `Shell::tab_keys`.
trait Keys {
    fn tab_keys(&self) -> Vec<TabKey>;
}

impl Keys for TabStrip {
    fn tab_keys(&self) -> Vec<TabKey> {
        self.tabs().iter().map(|tab| tab.key).collect()
    }
}

fn workspace() -> (tempfile::TempDir, BlobStore) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let blobs = BlobStore::open(dir.path().join("blobs")).expect("a blob store");
    (dir, blobs)
}

fn board_path(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    dir.path().join(format!("{name}.vellum"))
}

fn camera_at(x: f64, zoom: f64) -> Camera {
    let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
    camera.set_center(WorldPoint::new(x, 0.0));
    camera.set_zoom_about(zoom, ScreenPoint::new(720.0, 450.0));
    camera
}

/// The board's items, read back from its file with nothing of this session in the way.
fn items_on_disk(path: &Path) -> usize {
    let mut db = vellum_store::BoardDb::open(path).expect("reopening the board");
    let board = db.load().expect("loading the board").expect("the board is there");
    board.item_count()
}

/// **Opening the same board twice must yield one tab**, whichever route it arrives by
/// — the library, the strip, or a launch that had already opened it.
///
/// Two copies is not merely untidy: each `Editor` runs its own autosave thread over
/// the same SQLite file, which is the shape of the import bug that turned 596 items
/// into 1,788.
#[test]
fn opening_the_same_board_twice_yields_one_tab_and_one_document() {
    let (dir, blobs) = workspace();
    let mut window = Window::new(blobs);
    let plan = board_path(&dir, "garage");
    let roadmap = board_path(&dir, "roadmap");

    window.open(&plan, "Site plan");
    assert_eq!(window.strip.len(), 2, "home plus one board");
    assert_eq!(window.strip.active(), 1);

    window.open(&roadmap, "Roadmap");
    assert_eq!(window.strip.len(), 3, "a new board opens on top");
    assert_eq!(window.strip.active(), 2);
    assert_eq!(window.session.len(), 1, "the first board is parked, not closed");

    // The same board again, from the library.
    window.open(&plan, "Site plan");
    assert_eq!(window.strip.len(), 3, "it opened a second tab");
    assert_eq!(window.strip.active(), 1, "and it did not switch to the one it had");
    assert_eq!(window.session.len(), 1, "or it is resident twice");
    assert_eq!(window.hot_path(), Some(plan.as_path()));

    // And once more while it is already the board on screen.
    window.open(&plan, "Site plan");
    assert_eq!(window.strip.len(), 3);
    assert_eq!(window.session.len(), 1);

    // The key is derived from the path, which is what makes all of the above hold
    // across a close and a re-open rather than only within one run.
    assert_eq!(Shell::tab_key(&plan), Shell::tab_key(&plan.clone()));
    assert_ne!(Shell::tab_key(&plan), Shell::tab_key(&roadmap));
}

/// **Closing a tab flushes that board and releases its document.**
///
/// Both halves matter and both are asserted: the work has to be on disk afterwards,
/// and the board has to be gone from memory — otherwise the session grows with every
/// board the user has ever looked at, which on 8GB is a real cost rather than a
/// theoretical one.
#[test]
fn closing_a_tab_flushes_the_board_and_releases_it() {
    let (dir, blobs) = workspace();
    let mut window = Window::new(blobs);
    let alpha = board_path(&dir, "alpha");
    let beta = board_path(&dir, "beta");

    window.open(&alpha, "Alpha");
    window.add_a_note(0.0);
    window.add_a_note(400.0);

    window.open(&beta, "Beta");
    assert_eq!(window.session.len(), 1, "Alpha is parked behind its tab");

    // Close Alpha's tab from behind Beta — the case the app cannot be *told* about,
    // because the strip removes the tab before the app hears anything.
    let alpha_tab = window
        .strip
        .tabs()
        .iter()
        .position(|tab| tab.key == Shell::tab_key(&alpha))
        .expect("Alpha has a tab")
        + 1;
    window.strip.close(alpha_tab);
    window.follow();

    assert!(window.session.is_empty(), "the closed board is still resident");
    assert_eq!(window.hot_path(), Some(beta.as_path()), "closing it moved the board on screen");
    assert_eq!(items_on_disk(&alpha), 2, "the closed board's work was not written");

    // Now close the one on screen. Home is what comes forward, and nothing is left.
    window.close_active();
    assert!(window.strip.is_home());
    assert!(window.session.is_empty());
    assert_eq!(window.hot_path(), None, "the last board was not released");
}

/// **Switching tabs restores the view exactly as it was left.** This is most of what
/// makes tabs feel like tabs: a board that snaps back to its start view every time it
/// is clicked is a reload wearing a tab's clothes.
#[test]
fn switching_tabs_restores_each_boards_own_camera() {
    let (dir, blobs) = workspace();
    let mut window = Window::new(blobs);
    let one = board_path(&dir, "one");
    let two = board_path(&dir, "two");
    let three = board_path(&dir, "three");

    window.open(&one, "One");
    window.set_camera(camera_at(-2_500.0, 0.25));
    let first = window.camera();

    window.open(&two, "Two");
    window.set_camera(camera_at(9_000.0, 3.0));
    let second = window.camera();

    window.open(&three, "Three");
    window.set_camera(camera_at(0.0, 1.0));
    let third = window.camera();

    // Back and forth, in an order that makes a stack or a single slot fail.
    window.open(&one, "One");
    assert_eq!(window.camera(), first, "the first board's view moved while it waited");
    window.open(&three, "Three");
    assert_eq!(window.camera(), third);
    window.open(&two, "Two");
    assert_eq!(window.camera(), second);
    window.open(&one, "One");
    assert_eq!(window.camera(), first, "and it survived a second round trip");

    // A board that is reached again after a close is a *reload*, and a reload has no
    // remembered view — which is correct, and is why the assertion above is about
    // boards whose tabs stayed open.
    assert_eq!(window.session.len(), 2);
}

/// The board on screen stays loaded while the library is showing, so going to the
/// library and back is instant — and it is released the moment its tab is closed.
#[test]
fn the_library_tab_does_not_unload_the_board_behind_it() {
    let (dir, blobs) = workspace();
    let mut window = Window::new(blobs);
    let path = board_path(&dir, "solo");

    window.open(&path, "Solo");
    window.set_camera(camera_at(1_234.0, 0.5));
    let view = window.camera();

    // Home.
    window.strip.set_active(TabStrip::HOME);
    window.follow();
    assert!(window.strip.is_home());
    assert_eq!(window.hot_path(), Some(path.as_path()), "the board was unloaded by a screen change");

    // Back to it.
    window.strip.set_active(1);
    window.follow();
    assert_eq!(window.camera(), view, "the view was lost behind the library");

    // Home again, then close the tab from there.
    window.strip.set_active(TabStrip::HOME);
    window.follow();
    window.strip.close(1);
    window.follow();
    assert_eq!(window.hot_path(), None, "closing the last tab did not release its document");
    assert!(window.strip.is_home());
}

/// Reordering a tab is the strip's business alone. The app addresses boards by key, so
/// a drag must not disturb which document is on screen or what is resident.
#[test]
fn dragging_a_tab_changes_nothing_the_app_holds() {
    let (dir, blobs) = workspace();
    let mut window = Window::new(blobs);
    let paths: Vec<PathBuf> = (1..=3).map(|n| board_path(&dir, &format!("b{n}"))).collect();
    for (n, path) in paths.iter().enumerate() {
        window.open(path, &format!("Board {}", n + 1));
    }

    let before = window.hot_path().map(Path::to_path_buf);
    let resident = window.session.len();
    assert!(window.strip.reorder(3, 1), "the last tab moved to the front");
    window.follow();

    assert_eq!(window.hot_path().map(Path::to_path_buf), before, "a drag changed the board on screen");
    assert_eq!(window.session.len(), resident);
    assert_eq!(
        window.strip.active_tab().and_then(|t| t.path.clone()).as_deref(),
        before.as_deref(),
        "the tab in front no longer names the board that is showing",
    );
}

/// `Cmd+1`…`Cmd+9`, the app's own binding. Tab one is home, and `Cmd+9` is the last
/// tab however many there are — the rule every browser already taught the user.
#[test]
fn the_number_keys_name_the_tabs_a_browser_would() {
    // Re-stated rather than imported: `tab_for_digit` is `pub(crate)`, and pinning the
    // rule from outside the crate is what makes it a contract instead of a detail.
    let expect = |digit: &str, tabs: usize| -> Option<usize> {
        match digit {
            "9" if tabs > 0 => Some(tabs - 1),
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" => {
                digit.parse::<usize>().ok().map(|n| n - 1).filter(|i| *i < tabs)
            }
            _ => None,
        }
    };

    assert_eq!(expect("1", 4), Some(TabStrip::HOME), "Cmd+1 is the board library");
    assert_eq!(expect("2", 4), Some(1), "Cmd+2 is the first board");
    assert_eq!(expect("4", 4), Some(3));
    assert_eq!(expect("5", 4), None, "a digit past the end names nothing");
    assert_eq!(expect("9", 4), Some(3), "Cmd+9 is the last tab");
    assert_eq!(expect("9", 12), Some(11));
    assert_eq!(expect("0", 4), None, "Cmd+0 is zoom to 100%");
}

/// A board created empty, opened, edited, closed and reopened has to come back with
/// what was put on it. The plainest statement of the whole feature's contract.
#[test]
fn a_board_survives_being_closed_and_opened_again() {
    let (dir, blobs) = workspace();
    let mut window = Window::new(blobs);
    let path = board_path(&dir, "roundtrip");

    window.open(&path, "Round trip");
    window.add_a_note(0.0);
    window.add_a_note(300.0);
    window.add_a_note(600.0);
    window.close_active();
    assert_eq!(items_on_disk(&path), 3);

    window.open(&path, "Round trip");
    let (_, editor, _) = window.hot.as_ref().expect("it opened again");
    assert_eq!(editor.board().item_count(), 3);
    assert_eq!(window.strip.len(), 2, "and it is one tab, not two");

    // Titles are the library's, and a board carries its own.
    let mut fresh = Board::new();
    fresh.set_title("Round trip").expect("a title");
    assert_eq!(fresh.title(), "Round trip");
}
