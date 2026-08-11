//! Several boards open at once.
//!
//! *"i have the tabs on top of the application"*, and a tab that reloads its board
//! from SQLite every time it is clicked is a tab in name only: the camera snaps back
//! to the start view, the selection is gone and the undo history is empty. What makes
//! tabs feel like tabs is that **switching back finds the board exactly as it was
//! left** — same view, same selection, same history — and that is a memory question
//! before it is an interface one.
//!
//! # Where the boards live
//!
//! The board on screen is *hot*: it stays in [`ActiveState::editor`] and
//! [`ActiveState::camera`], which is where every one of the two hundred call sites in
//! `crate::actions` already reads it from. This module owns the **rest** — one
//! [`Parked`] per open tab that is not in front — and the app swaps between the two.
//!
//! That split is deliberate rather than incidental. A `Session` that owned the hot
//! board as well would turn `self.editor` into `self.session.editor_mut()` in every
//! command, break the disjoint-field borrows the frame path depends on
//! (`Editor::frame_parts` and `Surface::parts` are held at the same time), and buy
//! nothing: there is exactly one board on screen, and hoisting it is what makes that
//! a type-level fact instead of a convention.
//!
//! The invariant, which [`Session::retain`] and the tests below both check:
//!
//! > **Every board tab on the strip is either the hot board or parked here, and
//! > nothing is parked here that has no tab.**
//!
//! # Order is not ours
//!
//! `vellum_ui::tabs` is explicit that *"the app owns which boards are open; the strip
//! owns the order they sit in and which one is in front"*. So this is a **set** keyed
//! by [`TabKey`], not a list — the `Vec` inside is an implementation detail with no
//! meaning, and asking it for "the third tab" is a question only the strip can answer.
//! Reordering a tab therefore costs the app nothing, which is why
//! `UiEvent::ReorderTabs` is ignored.
//!
//! # What parking does and does not cost
//!
//! - **The document stays resident.** That is the point.
//! - **Textures do not leak.** `crate::assets` maps a content hash to a
//!   [`vellum_render::TextureId`], and the ids are handed out monotonically and never
//!   recycled, so a parked board's mapping cannot come back pointing at a *different*
//!   image. `Assets::texture` checks `TextureManager::contains` before trusting one,
//!   and a parked board that lost its textures to the shared eviction budget simply
//!   re-uploads them when it comes forward.
//! - **A parked board cannot be edited**, because it is not on screen and nothing but
//!   the frame path reaches an editor. So its thumbnail, taken as it was parked, is
//!   still accurate when its tab is closed — which is what lets a close flush without
//!   having to make the board hot again to re-render it.
//! - **Its autosave thread keeps running.** Parking flushes first, so a parked board
//!   has nothing outstanding; the thread is joined by `Autosave`'s `Drop` when the tab
//!   closes.
//!
//! [`ActiveState::editor`]: crate::app::ActiveState
//! [`ActiveState::camera`]: crate::app::ActiveState

use std::path::{Path, PathBuf};

use vellum_scene::Camera;
use vellum_ui::TabKey;

use crate::editor::Editor;

/// One open board that is not the one on screen.
///
/// Everything a tab has to remember lives in here: the document and its undo history
/// and selection are the [`Editor`]'s, and the view is the [`Camera`]'s.
#[derive(Debug)]
pub struct Parked {
    key: TabKey,
    path: PathBuf,
    editor: Editor,
    camera: Camera,
}

impl Parked {
    pub const fn key(&self) -> TabKey {
        self.key
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn editor(&self) -> &Editor {
        &self.editor
    }

    pub const fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub const fn camera(&self) -> Camera {
        self.camera
    }

    /// Whether everything this board holds is already on disk — the tab's dot.
    pub fn is_durable(&self) -> bool {
        self.editor.is_durable()
    }

    /// Blocks until it is. Called when the tab closes and when the app quits.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.editor.flush()
    }
}

/// The open boards that are not in front.
#[derive(Debug, Default)]
pub struct Session {
    parked: Vec<Parked>,
}

impl Session {
    /// How many boards are resident besides the hot one.
    pub fn len(&self) -> usize {
        self.parked.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parked.is_empty()
    }

    pub fn contains(&self, key: TabKey) -> bool {
        self.index_of(key).is_some()
    }

    fn index_of(&self, key: TabKey) -> Option<usize> {
        self.parked.iter().position(|board| board.key == key)
    }

    /// Puts a board aside, keeping its document, its selection, its undo history and
    /// its camera.
    ///
    /// Parking a key that is already here **replaces** it rather than adding a second
    /// entry. That cannot happen while the hot board is genuinely hot — it is not
    /// parked, by definition — and if it ever did, two live `Editor`s over one SQLite
    /// file is the failure worth refusing rather than the one worth appending to.
    pub fn park(&mut self, key: TabKey, path: PathBuf, editor: Editor, camera: Camera) {
        let board = Parked { key, path, editor, camera };
        match self.index_of(key) {
            Some(index) => self.parked[index] = board,
            None => self.parked.push(board),
        }
    }

    /// Brings a board back, with the view it was left at. `None` if it is not
    /// resident, which is the caller's cue to open it from disk.
    pub fn take(&mut self, key: TabKey) -> Option<(Editor, Camera)> {
        let index = self.index_of(key)?;
        let board = self.parked.remove(index);
        Some((board.editor, board.camera))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Parked> {
        self.parked.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Parked> {
        self.parked.iter_mut()
    }

    /// Drops every parked board whose tab is gone, handing them back so the caller can
    /// flush them before they are released.
    ///
    /// **This is how memory stops growing.** A tab is closed by the strip, which has
    /// already removed it by the time the app hears about it, so the app cannot be told
    /// *which* board went — it has to compare what it holds against what the strip
    /// still shows. Doing it that way also survives the strip closing a tab for a
    /// reason the app never saw.
    pub fn retain(&mut self, live: &[TabKey]) -> Vec<Parked> {
        let mut released = Vec::new();
        let mut index = 0;
        while index < self.parked.len() {
            if live.contains(&self.parked[index].key) {
                index += 1;
            } else {
                released.push(self.parked.remove(index));
            }
        }
        released
    }

    /// Empties the session, for quitting. Every board comes back so every one of them
    /// can be flushed — *"quitting flushes every open board, not just the active
    /// one"*.
    pub fn drain(&mut self) -> Vec<Parked> {
        std::mem::take(&mut self.parked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::Board;
    use vellum_scene::{ScreenSize, WorldPoint};
    use vellum_store::BlobStore;

    fn blobs() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = BlobStore::open(dir.path().join("blobs")).expect("a blob store");
        (dir, store)
    }

    fn board(store: &BlobStore, title: &str) -> Editor {
        let mut board = Board::new();
        board.set_title(title).expect("a title");
        Editor::in_memory(board, store.clone())
    }

    fn camera_at(x: f64, zoom: f64) -> Camera {
        let mut camera = Camera::new(ScreenSize::new(1440.0, 900.0));
        camera.set_center(WorldPoint::new(x, 0.0));
        camera.set_zoom_about(zoom, vellum_scene::ScreenPoint::new(720.0, 450.0));
        camera
    }

    /// The whole promise of a tab: come back to it and the view has not moved.
    #[test]
    fn a_parked_board_comes_back_with_the_camera_it_was_left_at() {
        let (_dir, store) = blobs();
        let mut session = Session::default();
        let camera = camera_at(1_234.0, 0.5);

        session.park(TabKey(1), "/boards/a.vellum".into(), board(&store, "A"), camera);
        assert_eq!(session.len(), 1);
        assert!(session.contains(TabKey(1)));

        let (editor, restored) = session.take(TabKey(1)).expect("the board is resident");
        assert_eq!(restored, camera, "the view moved while the tab was in the background");
        assert_eq!(editor.board().title(), "A");
        assert!(session.is_empty(), "taking a board removes it from the session");
        assert!(session.take(TabKey(1)).is_none(), "and it cannot be taken twice");
    }

    /// Opening a board that is already open must switch to it, never load a second
    /// copy — two live `Editor`s over one SQLite file is the shape of the import
    /// duplication bug.
    #[test]
    fn parking_the_same_board_twice_replaces_it_rather_than_stacking_it() {
        let (_dir, store) = blobs();
        let mut session = Session::default();
        session.park(TabKey(7), "/boards/a.vellum".into(), board(&store, "first"), camera_at(0.0, 1.0));
        session.park(TabKey(7), "/boards/a.vellum".into(), board(&store, "second"), camera_at(9.0, 1.0));

        assert_eq!(session.len(), 1);
        let (editor, camera) = session.take(TabKey(7)).expect("resident");
        assert_eq!(editor.board().title(), "second");
        assert_eq!(camera.center().x, 9.0);
    }

    /// Closing a tab has to release the document, or memory grows with every board the
    /// user has ever looked at. On 8GB that is not a theoretical cost.
    #[test]
    fn boards_whose_tabs_are_gone_are_handed_back_to_be_released() {
        let (_dir, store) = blobs();
        let mut session = Session::default();
        for key in 1..=4_u64 {
            session.park(
                TabKey(key),
                format!("/boards/{key}.vellum").into(),
                board(&store, &format!("Board {key}")),
                camera_at(f64::from(key as u32), 1.0),
            );
        }

        let released = session.retain(&[TabKey(1), TabKey(3)]);
        assert_eq!(session.len(), 2, "two tabs are still open");
        let names: Vec<String> =
            released.iter().map(|b| b.editor().board().title()).collect();
        assert_eq!(names, vec!["Board 2".to_owned(), "Board 4".to_owned()]);
        assert!(session.contains(TabKey(1)) && session.contains(TabKey(3)));
        assert!(!session.contains(TabKey(2)) && !session.contains(TabKey(4)));

        // Nothing to release is the ordinary case and must not disturb anything.
        assert!(session.retain(&[TabKey(1), TabKey(3)]).is_empty());
        assert_eq!(session.len(), 2);

        // And a strip with no board tabs at all releases everything.
        assert_eq!(session.retain(&[]).len(), 2);
        assert!(session.is_empty());
    }

    /// *"Quitting flushes every open board, not just the active one."*
    #[test]
    fn draining_hands_back_every_board_so_every_one_can_be_flushed() {
        let (_dir, store) = blobs();
        let mut session = Session::default();
        for key in 1..=3_u64 {
            session.park(
                TabKey(key),
                format!("/boards/{key}.vellum").into(),
                board(&store, "B"),
                camera_at(0.0, 1.0),
            );
        }
        let mut drained = session.drain();
        assert_eq!(drained.len(), 3);
        assert!(session.is_empty(), "draining leaves nothing behind");
        for parked in &mut drained {
            assert!(parked.is_durable(), "an in-memory board has nothing to lose");
            parked.flush().expect("flushing an in-memory board cannot fail");
        }
        assert!(session.drain().is_empty());
    }

    #[test]
    fn a_parked_board_reports_its_own_path_and_key() {
        let (_dir, store) = blobs();
        let mut session = Session::default();
        session.park(TabKey(5), "/boards/site-plan.vellum".into(), board(&store, "Site plan"), camera_at(0.0, 1.0));
        let parked = session.iter().next().expect("one board");
        assert_eq!(parked.key(), TabKey(5));
        assert_eq!(parked.path(), Path::new("/boards/site-plan.vellum"));
        assert_eq!(session.iter_mut().count(), 1);
    }
}
