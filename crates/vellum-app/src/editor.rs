//! The board being edited: the document, where it is stored, and what is selected.
//!
//! Everything here is deliberately free of the window and the GPU. That is not
//! tidiness — it is what lets "paste a Miro board and check it arrived" be an
//! ordinary `cargo test` rather than something only a human with a trackpad can
//! verify, which is exactly what `tests/real_board.rs` does.
//!
//! # Saving is not on the frame path
//!
//! [`vellum_store::Autosave`] owns a writer thread and a database; the UI side does
//! one version comparison, one delta export and one channel send per edit. So
//! [`Editor::record`] is called after *every* mutation, including the ones inside a
//! drag, and nothing waits for a disk. The only places that block are quitting and
//! exporting, which is what [`Editor::flush`] is for.
//!
//! # Pasting from Miro
//!
//! `docs/02-miro-formats.md` establishes that Miro's `.rtb` board content is
//! encrypted and its **clipboard is not**, so the clipboard is the whole import
//! route — and it carries more than any Miro API does, notably freehand ink. The
//! payload rides on the clipboard's HTML flavour, which is why this reaches for
//! `arboard`'s `html()` rather than its `text()`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use vellum_doc::{Board, ItemId as DocId};
use vellum_import::pipeline::ImportOutcome;
use vellum_import::rtb::ArchiveSet;
use vellum_scene::{ItemId as SceneId, WorldPoint, WorldRect};
use vellum_store::{Autosave, BlobStore, BoardDb};

use crate::assets::Assets;
use crate::draw::BoardEpoch;
use crate::project::Projection;

/// Hands out one [`BoardEpoch`] per open board.
///
/// Monotonic and never reused, so a closed board's epoch cannot come back naming a
/// different board — the same property `vellum_render::TextureId` relies on. Starts at
/// zero, which is why `Painter` can use `u64::MAX` as its "nothing painted yet".
static NEXT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Everything about the open board.
pub struct Editor {
    board: Board,
    /// Distinguishes this board from every other open one, for caches that outlive a
    /// board switch. See [`BoardEpoch`].
    epoch: BoardEpoch,
    projection: Projection,
    assets: Assets,
    /// `None` for a board that only exists in memory — a bench board, or a test.
    autosave: Option<Autosave>,
    path: Option<PathBuf>,
    selection: Vec<SceneId>,
    /// How long the last [`Self::reproject`] took, for the HUD.
    ///
    /// Every edit and every undo rebuilds the whole document (see `reproject`), so this is
    /// the number that says what an edit costs on *this* board rather than on a fixture.
    /// Kept rather than logged: an edit happens too often to log and the figure is only
    /// interesting next to the frame time it lands in.
    last_reproject: std::time::Duration,
    /// The round trip to `velmd`, or `None` for a board that syncs with nothing.
    ///
    /// **On the editor rather than on `ActiveState`, and that is the whole reason a tab
    /// switch does not break it.** `crate::session` parks a board by moving its `Editor`
    /// aside and swaps another in, so anything a board must remember across a switch has to
    /// live here: put the sync on the hot state instead and every tab change would either
    /// re-download the whole document or, worse, hand board A's version vector to board B.
    ///
    /// ⚠ It is deliberately *not* driven while the board is parked. A parked board cannot be
    /// edited, so it has nothing to send; what it misses arrives the moment it comes back,
    /// because [`Self::sync_now`] asks with the board's own current version and the server
    /// answers with everything since. Nothing accumulates and nothing is lost.
    sync: Option<crate::sync::Sync>,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("title", &self.board.title())
            .field("items", &self.board.item_count())
            .field("path", &self.path)
            .field("saving", &self.autosave.is_some())
            .finish()
    }
}

impl Editor {
    /// Opens the board at `path`, creating it if it is not there, and starts saving.
    ///
    /// Recovery runs first and is *reported*: `vellum_store::BoardDb` can tell that a
    /// previous session did not finish writing, and a board that silently loses the
    /// last thirty seconds of work is worse than one that says so.
    pub fn open(path: impl AsRef<Path>, blobs: BlobStore) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let mut db = BoardDb::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let recovery = db.recovery();
        if !recovery.is_clean() {
            log::warn!("{}: {recovery:?}", path.display());
            if let Some(recovered) = db.recover().context("recovering the board")? {
                log::info!("recovered: {recovered:?}");
            }
        }

        let board = match db.load().context("loading the board")? {
            Some(board) => board,
            None => {
                let mut board = Board::new();
                let title = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled".to_string());
                board.set_title(&title)?;
                board
            }
        };

        let autosave = Autosave::start(db, &board).context("starting the autosave writer")?;
        let mut editor = Self::in_memory(board, blobs);
        editor.autosave = Some(autosave);
        editor.path = Some(path);
        Ok(editor)
    }

    /// A board with nowhere to save to. For benches and tests.
    pub fn in_memory(board: Board, blobs: BlobStore) -> Self {
        let mut editor = Self {
            board,
            epoch: BoardEpoch(NEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
            projection: Projection::new(),
            assets: Assets::new(blobs),
            autosave: None,
            path: None,
            selection: Vec::new(),
            last_reproject: std::time::Duration::ZERO,
            sync: None,
        };
        editor.reproject();
        editor
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    /// Which open board this is, for caches keyed on [`SceneId`].
    pub const fn epoch(&self) -> BoardEpoch {
        self.epoch
    }

    /// Puts the board aside: flush what is outstanding, then drop what can be rebuilt.
    ///
    /// The projection is a **second full copy of every item** — `Projected` clones the
    /// document's `Item`, ink point vectors and all — plus an R-tree over it, and a
    /// board nobody is looking at has no use for either. Both come back from the
    /// document in one `reproject`, so this costs a switch-in and saves the whole
    /// duplicate for as long as the tab sits in the background.
    ///
    /// The camera, the selection and the undo history are **not** touched: coming back
    /// to a tab exactly as it was left is the promise `crate::session` makes.
    pub fn park(&mut self) -> Result<()> {
        self.flush()?;
        self.projection.shed();
        Ok(())
    }

    /// Brings the board back, rebuilding what [`Self::park`] dropped.
    pub fn unpark(&mut self) {
        if self.projection.is_shed() {
            self.reproject();
        }
    }

    pub fn assets(&self) -> &Assets {
        &self.assets
    }

    pub fn assets_mut(&mut self) -> &mut Assets {
        &mut self.assets
    }

    /// The three things a frame needs, borrowed together.
    ///
    /// One call rather than three accessors because a frame reads the projection and
    /// the selection while *writing* asset residency, and borrowing the editor twice
    /// — once shared, once exclusive — is not something the caller can spell.
    pub fn frame_parts(&mut self) -> (&Projection, &[SceneId], &mut Assets) {
        (&self.projection, &self.selection, &mut self.assets)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn selection(&self) -> &[SceneId] {
        &self.selection
    }

    /// The selection as document ids, which is what every editing API takes.
    ///
    /// In paint order, because that is the order the selection is kept in, so an
    /// operation over several items is deterministic and reads the same way twice.
    pub fn selected_ids(&self) -> Vec<DocId> {
        self.selection
            .iter()
            .filter_map(|id| self.projection.get(*id).map(|p| p.doc_id))
            .collect()
    }

    /// The selection minus anything locked — what a command that *moves* items acts on.
    ///
    /// The same filter [`Editor::delete_selection`] applies internally, offered as a
    /// helper because align, distribute and the four z-order commands each build their
    /// own id list and every one of them was writing straight through a lock. Their
    /// gates are `!all_locked`, so they are only disabled when *everything* selected is
    /// locked, which leaves the case a lock exists to survive — `⌘A` over a board with
    /// one locked item — wide open.
    ///
    /// Order is the selection's, so a caller that depends on paint order (`reorder`)
    /// still gets it.
    pub fn unlocked_selected_ids(&self) -> Vec<DocId> {
        self.selection
            .iter()
            .filter_map(|id| self.projection.get(*id))
            .filter(|projected| !projected.item.style.locked)
            .map(|projected| projected.doc_id)
            .collect()
    }

    /// How many of the selected items are locked, for a caller that wants to say so.
    pub fn locked_selected_count(&self) -> usize {
        self.selection
            .iter()
            .filter_map(|id| self.projection.get(*id))
            .filter(|projected| projected.item.style.locked)
            .count()
    }

    /// Replaces the selection with whatever of `ids` still exists.
    pub fn select(&mut self, ids: impl IntoIterator<Item = DocId>) {
        self.selection = ids
            .into_iter()
            .filter_map(|id| self.projection.scene_id(id))
            .collect();
    }

    /// Runs one edit against the document and puts everything back in step.
    ///
    /// The single door through which the chrome's commands reach the board. It exists
    /// so that "reproject, then record" cannot be forgotten: a mutation that skips the
    /// first draws stale, and one that skips the second is the lost work the user
    /// asked instant autosave to prevent. Both are silent failures, which is exactly
    /// the kind worth making structurally impossible.
    ///
    /// # The failure path is a path
    ///
    /// A closure that fails partway has usually already changed the document — fifteen
    /// callers open an undo group and then apply several fallible edits, so "it failed"
    /// means "some of it happened". Returning early on `?` therefore got both halves of
    /// this function's own promise wrong, and each in a way that outlives the operation:
    ///
    /// - **The undo group stayed open.** `Board::end_undo_group` sits after the last `?`,
    ///   so it never ran; Loro's `group_start` then answers `UndoGroupAlreadyStarted` for
    ///   *every later group*, and since nothing else ever closes one, every grouped
    ///   operation on the board — move, delete, paste, align, restyle — fails from then
    ///   until the app restarts. One transient error became a permanently broken board.
    /// - **The projection was left stale.** Exactly the "draws stale" failure named
    ///   above, and it takes hit-testing with it, so clicks land on where items *were*.
    ///
    /// `end_undo_group` is idempotent — it clears the slot rather than popping a depth —
    /// so calling it when nothing is open is free, and the group cannot leak either way.
    pub fn edit<T>(&mut self, f: impl FnOnce(&mut Board) -> Result<T>) -> Result<T> {
        let value = match f(&mut self.board) {
            Ok(value) => value,
            Err(error) => {
                self.board.end_undo_group();
                self.reproject();
                return Err(error);
            }
        };
        self.reproject();
        self.record()?;
        Ok(value)
    }

    /// [`Self::edit`] for an edit that changes **one item's content and nothing else** —
    /// a keystroke.
    ///
    /// The difference is the projection: `edit` calls [`Projection::rebuild`], which
    /// deep-clones every item out of the CRDT and rebuilds the R-tree. Per character on a
    /// 1,300-item board that is the entire document reconstructed sixty times a second,
    /// and it is what the user meant by the app *"struggling so much to show me what i am
    /// typing"*.
    ///
    /// `refresh_item` **verifies** that the cheap path is legitimate — it re-derives the
    /// item's bounds and refuses if they moved — so a caller that is wrong about "content
    /// only" gets a full rebuild rather than an R-tree that disagrees with the document.
    /// That fallback is the whole reason this is safe to use: the failure mode of
    /// guessing wrong is a click landing where an item is not, which no test would catch.
    pub fn edit_content<T>(&mut self, doc: DocId, f: impl FnOnce(&mut Board) -> Result<T>) -> T
    where
        T: Default,
    {
        match f(&mut self.board) {
            Ok(value) => {
                match self.projection.refresh_item(&self.board, doc) {
                    Ok(true) => {}
                    // Bounds moved, or the item is one whose extent depends on others.
                    Ok(false) => self.reproject(),
                    Err(error) => {
                        log::error!("refreshing an edited item: {error}");
                        self.reproject();
                    }
                }
                if let Err(error) = self.record() {
                    log::error!("saving the board: {error}");
                }
                value
            }
            Err(error) => {
                // Same rescue as `edit`: a closure that failed part way has usually
                // already changed the document, and an open undo group must not leak —
                // trap 11.
                self.board.end_undo_group();
                self.reproject();
                log::error!("editing: {error}");
                T::default()
            }
        }
    }

    /// Opens an undo group without touching the projection.
    ///
    /// The caret and the eraser hold a group open across many calls, and they used to open
    /// and close it through [`Self::edit`] — which reprojects the whole document and asks
    /// the autosave writer to look for a delta. Neither is right here: opening or closing a
    /// group changes **no item**, so there is nothing to project and nothing to save, and
    /// on a 1,000-item board each of those cost a full rebuild plus the invalidation of
    /// every cached layout on the board. Every click away from a sticky paid one.
    ///
    /// Errors are logged rather than returned. The one failure `group_start` has is
    /// *"there is already an active group"*, which the callers cannot act on and which
    /// `ActiveState::settle` exists to prevent — see trap 11.
    pub fn begin_group(&mut self) {
        if let Err(error) = self.board.begin_undo_group() {
            log::error!("opening an undo group: {error}");
        }
    }

    /// Closes the group [`Self::begin_group`] opened. Idempotent, and free when nothing is
    /// open, which is what makes it safe to call from every path a gesture can end on.
    pub fn end_group(&mut self) {
        self.board.end_undo_group();
    }

    /// The restore points the store has kept for this board.
    pub fn restore_points(&self) -> Result<Vec<vellum_store::RestorePoint>> {
        match self.autosave.as_ref() {
            Some(autosave) => Ok(autosave.restore_points()?),
            None => Ok(Vec::new()),
        }
    }

    /// Rebuilds the spatial index from the document.
    ///
    /// Called by every editing path here rather than once a frame: an edit is a
    /// discrete act, and a per-frame check would either rebuild a 596-item board
    /// sixty times a second or need a flag that some future mutator forgets to set.
    pub fn reproject(&mut self) {
        let started = crate::time::Instant::now();
        if let Err(error) = self.projection.rebuild(&self.board) {
            log::error!("projecting the board: {error}");
        }
        self.selection.retain(|id| self.projection.get(*id).is_some());
        self.last_reproject = started.elapsed();
    }

    /// What the last whole-document rebuild cost. See [`Self::last_reproject`].
    pub const fn last_reproject(&self) -> std::time::Duration {
        self.last_reproject
    }

    /// Tells the writer the document changed. Cheap enough for the frame path.
    pub fn record(&mut self) -> Result<()> {
        if let Some(autosave) = self.autosave.as_mut() {
            autosave.record(&self.board).context("saving the board")?;
        }
        Ok(())
    }

    /// Blocks until everything is on disk. For quitting, and nothing else.
    pub fn flush(&mut self) -> Result<()> {
        if let Some(autosave) = self.autosave.as_mut() {
            autosave.flush(&self.board).context("flushing the board")?;
        }
        Ok(())
    }

    /// Whether every change is durable. `true` for an in-memory board, which has
    /// nothing to lose.
    pub fn is_durable(&self) -> bool {
        self.autosave
            .as_ref()
            .is_none_or(|autosave| autosave.stats().is_durable())
    }

    // ----- syncing with velmd ---------------------------------------------

    /// Point this board at a `velmd` server.
    ///
    /// The last call wins, and the previous [`crate::sync::Sync`] is dropped — which stops
    /// its worker, because the worker returns when its `Sender` goes. So re-attaching does
    /// not leak a thread per attach.
    pub fn attach_sync(&mut self, sync: crate::sync::Sync) {
        self.sync = Some(sync);
    }

    /// What the sync is doing — for the HUD, and for the sentence a failure needs.
    pub fn sync_state(&self) -> Option<&crate::sync::Sync> {
        self.sync.as_ref()
    }

    /// Send one round trip, if one is due. Returns whether it went.
    ///
    /// Safe and cheap to call every frame: [`crate::sync::Sync::is_ready`] refuses when one
    /// is already out or a previous failure's backoff has not elapsed, and refusing costs two
    /// compares. That is the whole reason this is a per-frame call rather than a timer — a
    /// timer is a second clock that has to be started, stopped and remembered across a tab
    /// switch, and this codebase already pays for one of those in the agent runtime.
    ///
    /// ⚠ **`vv` and `since` are different questions and the difference is load-bearing.**
    /// `vv` is what *this* board has, and it decides what comes back. `since` is what the
    /// *server* had at the last successful round trip, and it decides what goes up. Passing
    /// one where the other belongs still compiles, still syncs, and quietly sends the entire
    /// document on every request.
    pub fn sync_now(&mut self) -> bool {
        let Some(sync) = self.sync.as_ref() else { return false };
        if !sync.is_ready() {
            return false;
        }
        // Decoded here, before the board is touched, so the borrow of `self.sync` is over by
        // the time `request` needs it mutably. `decode_or_empty` rather than `decode`: bytes
        // that will not parse mean "the server has nothing of ours", which costs one full
        // upload and self-heals, where a hard error would stop this board syncing for good.
        let since = vellum_doc::Version::decode_or_empty(sync.since().unwrap_or_default());
        let vv = self.board.version().encode();
        let delta = match self.board.export_since(&since) {
            Ok(delta) => delta,
            Err(error) => {
                log::warn!("sync: exporting this board's changes failed ({error})");
                return false;
            }
        };
        self.sync.as_mut().is_some_and(|sync| sync.request(vv, delta))
    }

    /// Everything the server has answered since the last call. Never blocks.
    ///
    /// ⚠ **The caller must decide whether it may apply these *before* calling this.** A reply
    /// drained and then dropped is gone until the server happens to resend it, while a reply
    /// left in the channel arrives on a later frame at no cost. `crate::sync`'s header states
    /// the rule and `ActiveState::apply_sync` is the one caller that honours it.
    pub fn drain_sync(&mut self) -> Vec<crate::sync::SyncReply> {
        self.sync.as_mut().map(crate::sync::Sync::drain).unwrap_or_default()
    }

    /// Merge an update from the server into this board.
    ///
    /// Through [`Self::edit`] rather than around it, so the reproject and the autosave record
    /// that every other mutation gets happen here too. A remote change that skipped the
    /// reproject would draw stale *and* hit-test stale, which is the failure `edit`'s own doc
    /// comment exists to make unforgettable.
    ///
    /// **No undo group is opened, and that is correct rather than an omission.**
    /// [`vellum_doc::Board::apply`] is documented non-undoable: undo covers this peer's own
    /// edits, so `⌘Z` can never revert work that arrived from somewhere else. A local undo
    /// deleting a remote edit is the one way a CRDT can still lose somebody's work.
    pub fn apply_remote(&mut self, updates: &[u8]) -> Result<()> {
        // ⚠ **RULE ZERO: one snapshot before anything from a network reaches this file.**
        //
        // `velmd` takes exactly this point before the first change any board ever receives
        // from a browser, and the client needs it more, not less: these are the *originals*.
        // `Board::apply` is documented non-undoable, so `⌘Z` cannot walk a bad merge back,
        // and `edit` records straight through the autosave writer — by the time anything
        // looks wrong it is already on disk. Without this the only way back from a sync
        // pointed at the wrong server, or from two boards that happen to share a file stem,
        // is a copy the user thought to make first.
        //
        // Once per board **ever**, not once per launch: the check is a scan of the board's
        // own restore points for the label, which `BoardDb::restore_points` answers without
        // reading a single snapshot's bytes.
        self.take_restore_point_before_the_first_merge();
        self.edit(|board| {
            board.apply(updates)?;
            Ok(())
        })
    }

    /// What that snapshot is called.
    ///
    /// A **labelled** point rather than an automatic one, for the reason velmd gives for its
    /// own: an automatic point is eligible for pruning, and this is the thing being kept.
    /// The label is also how the check knows it has already been taken.
    const BEFORE_SYNC: &'static str = "before the first change from another machine";

    /// Snapshot the board, unless one is already there.
    ///
    /// **Reported and continued rather than propagated.** A failure here must not stop the
    /// merge: the alternative to an unprotected sync is not a protected one, it is a board
    /// that silently stops receiving — and the user would find out about that much later than
    /// about a warning in the log. It is loud rather than silent for the same reason.
    fn take_restore_point_before_the_first_merge(&mut self) {
        let Some(autosave) = self.autosave.as_mut() else { return };
        match autosave.restore_points() {
            Ok(points) => {
                if points.iter().any(|point| point.label.as_deref() == Some(Self::BEFORE_SYNC)) {
                    return;
                }
            }
            Err(error) => {
                log::error!("sync: cannot read this board's restore points ({error}); \
                             merging anyway, but there is no snapshot to go back to");
                return;
            }
        }
        match autosave.create_restore_point(&self.board, Self::BEFORE_SYNC) {
            Ok(id) => log::info!("sync: restore point {id} taken before the first merge"),
            Err(error) => log::error!(
                "sync: could not take a restore point before merging ({error}); \
                 merging anyway, but there is no snapshot to go back to"
            ),
        }
    }

    // ----- selection -------------------------------------------------------

    /// Selects the topmost item under a world point, or clears the selection when
    /// there is nothing there.
    pub fn pick(&mut self, at: WorldPoint, additive: bool) {
        match self.projection.scene().hit_test(at) {
            Some(id) => {
                if additive {
                    if let Some(index) = self.selection.iter().position(|s| *s == id) {
                        self.selection.remove(index);
                    } else {
                        self.selection.push(id);
                    }
                } else {
                    self.selection.clear();
                    self.selection.push(id);
                }
            }
            None if !additive => self.selection.clear(),
            None => {}
        }
    }

    /// Selects everything a rectangle touches.
    ///
    /// Intersection rather than containment, matching Miro: a marquee that only
    /// selected items it fully enclosed would make selecting a large frame
    /// impossible without zooming out past it.
    pub fn marquee(&mut self, rect: WorldRect, additive: bool) {
        if !additive {
            self.selection.clear();
        }
        // **The same function the live preview draws from** — `Projection::marquee_hits`,
        // which also owns the rule that a frame is only caught when the sweep contains the
        // whole of it. Two copies of that rule would be a ring promising a selection the
        // release does not deliver, which is worse than showing no ring at all.
        let hits = self.projection.marquee_hits(rect);
        // A set rather than `Vec::contains`: an additive marquee over a large board
        // would otherwise be a linear scan inside a loop over the hits, which is
        // 5×10⁷ comparisons for ten thousand items and a visible freeze on mouse-up
        // — from the app's *default* gesture.
        let mut already: std::collections::HashSet<SceneId> =
            self.selection.iter().copied().collect();
        for id in hits {
            if already.insert(id) {
                self.selection.push(id);
            }
        }
    }

    // ----- dragging --------------------------------------------------------

    /// Shows the selection at a new place without writing to the document.
    ///
    /// The live half of a drag. Every frame of a drag would otherwise be a Loro
    /// transaction, a full reprojection and an autosave delta — measured at 57 ms per
    /// frame on a sixteen-thousand-item board — for a position the user has not
    /// committed to yet. [`Projection::moved`] keeps the index and the hit-test in
    /// step for a pair of `O(log n)` operations instead, and
    /// [`Editor::commit_placements`] writes the answer once when the button comes up.
    pub fn preview_placement(&mut self, id: SceneId, placement: vellum_doc::Placement) {
        self.projection.moved(id, placement);
    }

    /// Shows new text on the canvas without writing to the document.
    pub fn preview_text(&mut self, id: SceneId, text: vellum_doc::StyledText) {
        self.projection.retext(id, text);
    }

    /// Writes a batch of placements as **one** undo step.
    ///
    /// `docs/06-mouse-controls.md` §4: one drag is one undo step, however many items
    /// it moved and however many frames it took.
    pub fn commit_placements(
        &mut self,
        placements: &[(DocId, vellum_doc::Placement)],
    ) -> Result<usize> {
        self.commit_move(placements, &[])
    }

    /// [`Self::commit_placements`], plus the frames the moved items now belong to.
    ///
    /// **One edit and one undo group, deliberately.** A move that changes what an item is
    /// parented to is still one thing the user did, so `⌘Z` has to put both halves back
    /// together — and opening a second group for the reparenting would hit
    /// `UndoGroupAlreadyStarted` and break every grouped operation after it (trap 11).
    ///
    /// The reparenting is written **before** the placements. `Board::reparent` moves a node in
    /// the tree and `Placement` is absolute, so the order does not affect where anything lands;
    /// doing it first means a frame that has just adopted an item already owns it when the item
    /// is placed, which is the order `paste_internal` uses for the same reason.
    pub fn commit_move(
        &mut self,
        placements: &[(DocId, vellum_doc::Placement)],
        reparents: &[(DocId, Option<DocId>)],
    ) -> Result<usize> {
        if placements.is_empty() && reparents.is_empty() {
            return Ok(0);
        }
        self.edit(|board| {
            board.begin_undo_group()?;
            for (id, parent) in reparents {
                if let Err(error) = board.reparent(*id, *parent) {
                    // Refused rather than fatal: a cycle, or an item deleted from under the
                    // drag. The move itself still stands, which is the half the user watched.
                    log::debug!("reparenting {id}: {error}");
                }
            }
            let mut written = 0;
            for (id, placement) in placements {
                match board.set_placement(*id, *placement) {
                    Ok(()) => written += 1,
                    // An item deleted from under a drag — by an undo on another
                    // path, or a frame that took its children with it.
                    Err(error) => log::debug!("placing {id}: {error}"),
                }
            }
            board.end_undo_group();
            Ok(written)
        })
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    pub fn select_all(&mut self) {
        self.selection.clear();
        let mut ids: Vec<SceneId> = self.projection.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        self.selection = ids;
    }

    // ----- editing ---------------------------------------------------------

    /// Deletes the selection in one undoable step. Locked items are left behind.
    ///
    /// The lock is honoured *here* rather than in the caller because there is more than
    /// one caller — the Object menu, the `Delete` key and `Backspace` all arrive through
    /// this one function, and a filter in any one of them is a filter the other two skip.
    ///
    /// `Command::Delete`'s own gate only disables the command when **every** selected
    /// item is locked, so it does not cover the case that matters most: `⌘A` over a board
    /// with one locked item is a *mixed* selection, the command stays enabled, and
    /// without this filter the one item the user protected is the one they lose.
    ///
    /// A locked item inside a deleted *frame* still goes, because `Board::remove` takes
    /// the subtree and cannot be asked for part of one. That is a narrower hole than this
    /// one and is recorded in `CLAUDE.md` rather than papered over here.
    ///
    /// # Why the group is opened inside [`Self::edit`]
    ///
    /// It used to be opened on `self.board` directly, which made this the **only** grouped
    /// operation in the workspace outside `edit`'s rescue — so `begin_undo_group`'s `?`
    /// escaped with the group still open and poisoned every later grouped operation for the
    /// life of the session (trap 11). The caller that made it fire is a command run while an
    /// on-canvas text session held its own group open; `ActiveState::run` closes that first
    /// now, and this is the belt to that braces. Both are needed: fixing only the caller
    /// leaves the next caller to rediscover it.
    pub fn delete_selection(&mut self) -> Result<usize> {
        if self.selection.is_empty() {
            return Ok(0);
        }
        let doomed: Vec<DocId> = self
            .selection
            .iter()
            .filter_map(|id| self.projection.get(*id))
            .filter(|projected| !projected.item.style.locked)
            .map(|projected| projected.doc_id)
            .collect();

        let removed = self.edit(|board| {
            board.begin_undo_group()?;
            let mut removed = 0;
            for id in &doomed {
                match board.remove(*id) {
                    Ok(()) => removed += 1,
                    // A frame's children go with it, so a later id in the same batch may
                    // already be gone. That is success, not failure.
                    Err(error) => log::debug!("removing {id}: {error}"),
                }
            }
            board.end_undo_group();
            Ok(removed)
        })?;

        self.selection.clear();
        Ok(removed)
    }

    /// Removes every item, in one undoable step. Returns how many went.
    ///
    /// For re-importing a capture into the board it was last imported into, which is
    /// the only caller: without it `--import` appends and the board grows by a whole
    /// copy of the capture on every launch.
    pub fn clear(&mut self) -> usize {
        let ids = self.board.item_ids();
        if ids.is_empty() {
            return 0;
        }
        let before = self.board.item_count();
        let cleared = self.edit(|board| {
            board.begin_undo_group()?;
            for id in &ids {
                // A frame's children go with it, so a later id in the same batch may
                // already be gone. That is success, not failure — and it is why the
                // count comes from the item total rather than from this loop, which
                // would report only the survivors it happened to reach.
                if let Err(error) = board.remove(*id) {
                    log::debug!("clearing {id}: {error}");
                }
            }
            board.end_undo_group();
            Ok(before.saturating_sub(board.item_count()))
        });
        self.selection.clear();
        cleared.unwrap_or_else(|error| {
            log::error!("clearing the board: {error:#}");
            0
        })
    }

    pub fn undo(&mut self) -> Result<bool> {
        let changed = self.board.undo()?;
        if changed {
            // Loro's undo re-creates a deleted node under a *new* id, so anything
            // holding an id across an undo has to re-resolve. The selection is
            // dropped rather than silently pointing at something else.
            self.selection.clear();
            self.reproject();
            self.record()?;
        }
        Ok(changed)
    }

    pub fn redo(&mut self) -> Result<bool> {
        let changed = self.board.redo()?;
        if changed {
            self.selection.clear();
            self.reproject();
            self.record()?;
        }
        Ok(changed)
    }

    // ----- import ----------------------------------------------------------

    /// Reads the system clipboard and imports it if it holds a Miro payload.
    ///
    /// `Ok(None)` means the clipboard was not Miro's — the ordinary case, and not an
    /// error. A clipboard with no HTML flavour at all is also `Ok(None)`: it means
    /// the user copied something else.
    /// The clipboard handle is the **caller's**, held once for the whole application
    /// by `crate::app::ActiveState`.
    ///
    /// `arboard::Clipboard::new()` wraps a platform handle — an `NSPasteboard` on
    /// macOS — and its own documentation says to build it once and keep it. It used to
    /// be built per call, which leaked a handle per keystroke, and Velm was seen at
    /// **14.24 GB** after a run of pastes. Keeping one per `Editor` fixed the per-paste
    /// leak but still meant one platform handle per *open board*; one per process is
    /// what the original fix was aiming at.
    pub fn paste_from_clipboard(
        &mut self,
        clipboard: &mut arboard::Clipboard,
        archive: Option<&mut ArchiveSet>,
    ) -> Result<Option<ImportOutcome>> {
        let Ok(html) = clipboard.get().html() else {
            log::info!("the clipboard holds no HTML flavour, so no Miro payload");
            return Ok(None);
        };
        self.import_html(&html, archive)
    }

    /// Imports a Miro clipboard payload that is already in hand.
    ///
    /// Split from [`Self::paste_from_clipboard`] so the whole import path is
    /// testable from a captured payload on disk, which is what the reference board
    /// in `captures/` is for.
    pub fn import_html(
        &mut self,
        html: &str,
        archive: Option<&mut ArchiveSet>,
    ) -> Result<Option<ImportOutcome>> {
        self.import_html_selecting(html, archive, AfterImport::SelectPasted)
    }

    /// [`Self::import_html`] with explicit control over the resulting selection.
    pub fn import_html_selecting(
        &mut self,
        html: &str,
        archive: Option<&mut ArchiveSet>,
        after: AfterImport,
    ) -> Result<Option<ImportOutcome>> {
        let Some(decoded) = vellum_import::import_clipboard(html)? else {
            return Ok(None);
        };
        self.import_decoded(&decoded, archive, after, None).map(Some)
    }

    /// The half of an import that must happen where the document is, with the assets
    /// already fetched.
    ///
    /// Split from [`Self::import_html_selecting`] so the expensive half — pulling assets out
    /// of a `.rtb` and into the blob store, measured at 2,012 ms of a 2,076 ms paste — can
    /// run on a worker while the window keeps painting. This part is about 50 ms.
    ///
    /// `ready` is the worker's answer; `None` makes it do the work itself, which is what the
    /// `--import` command line and every test still do.
    pub fn import_decoded(
        &mut self,
        decoded: &vellum_import::ImportedBoard,
        archive: Option<&mut ArchiveSet>,
        after: AfterImport,
        ready: Option<vellum_import::PrefetchedAssets>,
    ) -> Result<ImportOutcome> {
        let before = self.board.item_count();
        let outcome = vellum_import::import_widgets_with(
            decoded,
            archive,
            self.assets.blobs(),
            &mut self.board,
            ready,
        )?;

        log::info!(
            "imported {} items ({} -> {} on the board)",
            outcome.total(),
            before,
            self.board.item_count()
        );
        self.selection.clear();
        self.reproject();
        if after == AfterImport::SelectPasted {
            self.selection = outcome
                .items
                .iter()
                .filter_map(|doc_id| self.projection.scene_id(*doc_id))
                .collect();
        }
        self.record()?;
        Ok(outcome)
    }

    /// The assets a decoded payload will ask for, and where the backups are.
    ///
    /// Everything a worker needs to run `vellum_import::prefetch_assets`, gathered here
    /// because the blob store lives behind `Assets` and the archives behind the app.
    pub fn prefetch_plan(
        &self,
        decoded: &vellum_import::ImportedBoard,
        archive: Option<&ArchiveSet>,
    ) -> (Vec<String>, Vec<std::path::PathBuf>, vellum_store::BlobStore) {
        (
            vellum_import::requested_assets(decoded, self.assets.blobs()),
            archive.map(ArchiveSet::paths).unwrap_or_default(),
            self.assets.blobs().clone(),
        )
    }
}

/// What an import should leave selected.
///
/// The two callers want opposite things and conflating them made the app look
/// broken: opening a board with `--import` selected all 596 items before the
/// user had touched anything, so the properties panel read "596 objects", every
/// item wore a selection outline, and clicking behaved as if nothing worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterImport {
    /// Select what arrived — right for ⌘V, where the user just placed something
    /// and will usually want to move it as one unit.
    SelectPasted,
    /// Leave the selection empty — right for opening a board, where the import
    /// is how the document *loaded* rather than something the user did.
    SelectNothing,
}

/// Where Vellum keeps its boards and its shared blob store.
///
/// One directory per platform convention, chosen without a dependency: three
/// environment variables and a fallback are less code than a crate, and the fallback
/// — the current directory — means the app always starts even in a sandbox with no
/// home at all.
pub fn data_directory() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
    });

    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));

    base.unwrap_or_else(|| PathBuf::from(".")).join("Vellum")
}

/// The default board file, used when no path is given on the command line.
pub fn default_board_path() -> PathBuf {
    data_directory()
        .join("boards")
        .join(format!("board.{}", vellum_store::BOARD_EXTENSION))
}

/// The shared, content-addressed asset directory. Shared across every board, which
/// is what makes an image used on twenty of them cost one copy.
pub fn blob_directory() -> PathBuf {
    data_directory().join("blobs")
}

/// Where the user's Miro `.rtb` backups live.
///
/// A watched folder rather than a file picker, because the app has no native file
/// dialog at all and because the alternative does not scale: bringing 58 boards across
/// means 58 backups, and one paste carries one board. Drop every `.rtb` in here once and
/// every paste finds its own — resolution is by resource id across the whole set, so no
/// board matching, no naming convention, and no per-paste choice.
///
/// Beside `boards/` and `blobs/` on purpose: the three things a user accumulates, in one
/// place they can open in Finder.
pub fn archive_directory() -> PathBuf {
    data_directory().join("archives")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{ItemKind, NewItem, Placement, StyledText};

    fn editor() -> (tempfile::TempDir, Editor) {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        (home, Editor::in_memory(Board::new(), blobs))
    }

    fn sticky(x: f64, y: f64) -> NewItem {
        NewItem::new(
            ItemKind::Sticky { text: StyledText::plain("fan"), background: None },
            Placement::new(x, y, 200.0, 200.0),
        )
    }

    /// Parking releases the projection, so everything that reads through it has to
    /// come back intact. The selection is the one that fails silently: it is stored as
    /// `SceneId`s, and ids that were renumbered would resolve to the *wrong* items
    /// rather than to none.
    #[test]
    fn a_parked_board_comes_back_with_its_selection() {
        let (_home, mut editor) = editor();
        let first = editor.board.add(sticky(0.0, 0.0)).unwrap();
        let second = editor.board.add(sticky(400.0, 0.0)).unwrap();
        editor.reproject();
        editor.select([first, second]);
        assert_eq!(editor.selected_ids(), vec![first, second]);

        editor.park().unwrap();
        assert_eq!(editor.projection().len(), 0, "parking kept the projection");
        assert!(editor.projection().is_shed());

        editor.unpark();

        assert_eq!(editor.projection().len(), 2);
        assert_eq!(
            editor.selected_ids(),
            vec![first, second],
            "a parked board came back selecting different items"
        );
    }

    /// `unpark` on a board that was never parked must not throw away a projection that
    /// is already correct — every tab switch calls it, including onto a board just
    /// opened from disk.
    #[test]
    fn unparking_a_board_that_was_never_parked_is_a_no_op() {
        let (_home, mut editor) = editor();
        editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.reproject();
        let generation = editor.projection().generation();

        editor.unpark();

        assert_eq!(editor.projection().generation(), generation, "unpark rebuilt needlessly");
        assert_eq!(editor.projection().len(), 1);
    }

    #[test]
    fn a_new_board_file_is_created_loaded_and_saved() {
        let home = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        let path = home.path().join("boards").join("engine.vellum");

        let mut editor = Editor::open(&path, blobs).unwrap();
        assert_eq!(editor.board().title(), "engine");
        editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.reproject();
        editor.record().unwrap();
        editor.flush().unwrap();
        assert!(editor.is_durable());
        drop(editor);

        let blobs = BlobStore::open(home.path().join("blobs")).unwrap();
        let reopened = Editor::open(&path, blobs).unwrap();
        assert_eq!(reopened.board().item_count(), 1);
        assert_eq!(reopened.projection().len(), 1);
        assert_eq!(reopened.board().title(), "engine");
    }

    #[test]
    fn picking_selects_the_topmost_item_and_empty_canvas_clears() {
        let (_home, mut editor) = editor();
        editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.board.add(sticky(50.0, 50.0)).unwrap();
        editor.reproject();

        editor.pick(WorldPoint::new(50.0, 50.0), false);
        assert_eq!(editor.selection().len(), 1);
        let top = editor.selection()[0];

        editor.pick(WorldPoint::new(-95.0, -95.0), false);
        assert_eq!(editor.selection().len(), 1);
        assert_ne!(editor.selection()[0], top, "the lower item was not picked");

        editor.pick(WorldPoint::new(9_000.0, 9_000.0), false);
        assert!(editor.selection().is_empty(), "empty canvas did not clear");
    }

    /// Shift-click adds, and shift-clicking the same item again removes it. Anything
    /// else makes an accidental double-click destroy a careful selection.
    #[test]
    fn additive_picking_toggles() {
        let (_home, mut editor) = editor();
        editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.board.add(sticky(1_000.0, 0.0)).unwrap();
        editor.reproject();

        editor.pick(WorldPoint::new(0.0, 0.0), false);
        editor.pick(WorldPoint::new(1_000.0, 0.0), true);
        assert_eq!(editor.selection().len(), 2);

        editor.pick(WorldPoint::new(1_000.0, 0.0), true);
        assert_eq!(editor.selection().len(), 1);

        // An additive click on empty canvas must not throw the selection away.
        editor.pick(WorldPoint::new(9_000.0, 9_000.0), true);
        assert_eq!(editor.selection().len(), 1);
    }

    /// Miro selects what a marquee *touches*, not only what it encloses. Requiring
    /// containment makes a frame larger than the screen unselectable.
    #[test]
    fn a_marquee_selects_what_it_touches() {
        let (_home, mut editor) = editor();
        editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.board.add(sticky(400.0, 0.0)).unwrap();
        editor.board.add(sticky(9_000.0, 0.0)).unwrap();
        editor.reproject();

        editor.marquee(
            WorldRect::from_corners(WorldPoint::new(-50.0, -50.0), WorldPoint::new(310.0, 50.0)),
            false,
        );
        assert_eq!(editor.selection().len(), 2, "a grazed item was missed");

        editor.marquee(
            WorldRect::from_corners(
                WorldPoint::new(8_900.0, -50.0),
                WorldPoint::new(9_100.0, 50.0),
            ),
            true,
        );
        assert_eq!(editor.selection().len(), 3);
    }

    #[test]
    fn select_all_and_clear() {
        let (_home, mut editor) = editor();
        for i in 0..5 {
            editor.board.add(sticky(i as f64 * 300.0, 0.0)).unwrap();
        }
        editor.reproject();

        editor.select_all();
        assert_eq!(editor.selection().len(), 5);
        editor.clear_selection();
        assert!(editor.selection().is_empty());
    }

    #[test]
    fn deleting_the_selection_is_one_undo_step() {
        let (_home, mut editor) = editor();
        for i in 0..4 {
            editor.board.add(sticky(i as f64 * 300.0, 0.0)).unwrap();
        }
        editor.reproject();
        editor.select_all();

        assert_eq!(editor.delete_selection().unwrap(), 4);
        assert_eq!(editor.board().item_count(), 0);
        assert!(editor.selection().is_empty());

        assert!(editor.undo().unwrap());
        assert_eq!(editor.board().item_count(), 4);
        assert_eq!(editor.projection().len(), 4, "undo did not reproject");
    }

    /// An edit that fails partway has still changed the document, so both halves of
    /// `edit`'s contract have to hold on the failure path too.
    ///
    /// What this pins is not the error — it is the *next* operation. Loro answers
    /// `UndoGroupAlreadyStarted` while a group is open and nothing else ever closes one,
    /// so a leaked group broke every grouped operation for the rest of the session. The
    /// second assertion is the "draws stale" failure `edit`'s own doc comment forbids.
    #[test]
    fn a_failed_edit_closes_its_undo_group_and_still_reprojects() {
        let (_home, mut editor) = editor();
        let id = editor.board.add(sticky(0.0, 0.0)).unwrap();
        // A real id that no longer resolves — the realistic shape of a mid-edit failure.
        let ghost = editor.board.add(sticky(900.0, 0.0)).unwrap();
        editor.board.remove(ghost).unwrap();
        editor.reproject();

        let outcome = editor.edit(|board| {
            board.begin_undo_group()?;
            board.set_placement(id, Placement::new(500.0, 0.0, 200.0, 200.0))?;
            board.set_placement(ghost, Placement::new(1.0, 1.0, 10.0, 10.0))?;
            board.end_undo_group();
            Ok(())
        });
        assert!(outcome.is_err(), "the ghost id must fail the edit");

        // 1. The group did not leak, so a later grouped edit still works.
        let after = editor.edit(|board| {
            board.begin_undo_group()?;
            board.set_placement(id, Placement::new(700.0, 0.0, 200.0, 200.0))?;
            board.end_undo_group();
            Ok(())
        });
        assert!(after.is_ok(), "a leaked group breaks every later edit: {after:?}");

        // 2. The projection tracked the document across the failure rather than going
        //    stale, which is what hit-testing reads.
        let scene = editor.projection().scene_id(id).expect("still projected");
        assert_eq!(editor.projection().get(scene).unwrap().item.placement.x, 700.0);
    }

    /// `⌘A` then Delete is the accident a lock exists to survive.
    ///
    /// `Command::Delete` is only disabled when *every* selected item is locked, so a
    /// select-all over a board with one locked item leaves the command enabled — and the
    /// one item the user protected was the one that used to be lost.
    #[test]
    fn deleting_a_mixed_selection_leaves_the_locked_items_behind() {
        let (_home, mut editor) = editor();
        let mut ids = Vec::new();
        for i in 0..4 {
            ids.push(editor.board.add(sticky(i as f64 * 300.0, 0.0)).unwrap());
        }
        let safe = ids[2];
        editor
            .board
            .set_style(safe, vellum_doc::Style { locked: true, ..Default::default() })
            .unwrap();
        editor.reproject();
        editor.select_all();

        assert_eq!(editor.delete_selection().unwrap(), 3, "the three unlocked ones went");
        assert_eq!(editor.board().item_count(), 1);
        assert!(editor.board().item(safe).is_ok(), "the locked one survived");
    }

    /// The filter the arrange commands were missing.
    ///
    /// Align, distribute and the four z-order commands each build their own id list from
    /// the selection, and every one of them used to build it with `selected_ids` — so a
    /// mixed selection moved the locked member. The filter is one line; having six call
    /// sites able to skip it is the defect, which is why this pins the helper *and*
    /// `--demo locked-arrange` drives the commands.
    #[test]
    fn the_arrange_filter_drops_locked_items_and_keeps_selection_order() {
        let (_home, mut editor) = editor();
        let mut ids = Vec::new();
        for i in 0..4 {
            ids.push(editor.board.add(sticky(i as f64 * 300.0, 0.0)).unwrap());
        }
        for locked in [ids[1], ids[3]] {
            editor
                .board
                .set_style(locked, vellum_doc::Style { locked: true, ..Default::default() })
                .unwrap();
        }
        editor.reproject();
        editor.select_all();

        assert_eq!(editor.selected_ids().len(), 4, "all four are still selected");
        assert_eq!(
            editor.unlocked_selected_ids(),
            vec![ids[0], ids[2]],
            "only the movable two, in selection order — `reorder` reads that order"
        );
        assert_eq!(editor.locked_selected_count(), 2);

        // Unlocking puts one back, so the helper is not a one-way filter either.
        editor.board.set_style(ids[1], vellum_doc::Style::default()).unwrap();
        editor.reproject();
        editor.select_all();
        assert_eq!(editor.unlocked_selected_ids(), vec![ids[0], ids[1], ids[2]]);
        assert_eq!(editor.locked_selected_count(), 1);
    }

    /// Loro's undo replays an inverse diff rather than resurrecting a node, so an
    /// item id does not survive an undo. Holding a stale selection across one would
    /// point at nothing, or worse, at something else.
    #[test]
    fn undo_drops_the_selection_rather_than_keeping_stale_ids() {
        let (_home, mut editor) = editor();
        editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.reproject();
        editor.select_all();
        editor.delete_selection().unwrap();
        editor.undo().unwrap();

        assert!(editor.selection().is_empty());
        for (id, _) in editor.projection().iter() {
            assert!(editor.projection().get(*id).is_some());
        }
    }

    /// The drag path, end to end. Every frame moves the projection and nothing else;
    /// the button coming up writes the document **once**, and one press of `⌘Z` puts
    /// everything back — `docs/06-mouse-controls.md` §4.
    #[test]
    fn a_drag_previews_freely_and_commits_as_one_undo_step() {
        let (_home, mut editor) = editor();
        let a = editor.board.add(sticky(0.0, 0.0)).unwrap();
        let b = editor.board.add(sticky(400.0, 0.0)).unwrap();
        editor.reproject();
        editor.select_all();
        let scenes: Vec<SceneId> = editor.selection().to_vec();

        // A hundred frames of dragging: the document does not move.
        for step in 1..=100 {
            for (index, scene) in scenes.iter().enumerate() {
                let base = index as f64 * 400.0;
                editor.preview_placement(
                    *scene,
                    Placement::new(base + f64::from(step), f64::from(step), 200.0, 200.0),
                );
            }
        }
        assert_eq!(editor.board().item(a).unwrap().placement.x, 0.0, "the drag wrote early");
        // …but the index followed, so the item is hit-testable where it is drawn.
        assert_eq!(
            editor.projection().scene().hit_test(WorldPoint::new(100.0, 100.0)),
            Some(scenes[0])
        );

        editor
            .commit_placements(&[
                (a, Placement::new(100.0, 100.0, 200.0, 200.0)),
                (b, Placement::new(500.0, 100.0, 200.0, 200.0)),
            ])
            .unwrap();
        assert_eq!(editor.board().item(a).unwrap().placement.x, 100.0);
        assert_eq!(editor.board().item(b).unwrap().placement.x, 500.0);

        assert!(editor.undo().unwrap());
        assert_eq!(editor.board().item(a).unwrap().placement.x, 0.0, "one drag was not one step");
        assert_eq!(editor.board().item(b).unwrap().placement.x, 400.0);
    }

    #[test]
    fn committing_nothing_writes_nothing() {
        let (_home, mut editor) = editor();
        assert_eq!(editor.commit_placements(&[]).unwrap(), 0);
    }

    /// Re-importing a capture into the board it was last imported into has to replace
    /// it, or the board grows by a whole copy of the capture on every launch.
    #[test]
    fn clearing_removes_everything_in_one_undoable_step() {
        let (_home, mut editor) = editor();
        for i in 0..6 {
            editor.board.add(sticky(i as f64 * 300.0, 0.0)).unwrap();
        }
        editor.reproject();

        assert_eq!(editor.clear(), 6);
        assert_eq!(editor.board().item_count(), 0);
        assert_eq!(editor.projection().len(), 0);
        assert!(editor.selection().is_empty());
        assert_eq!(editor.clear(), 0, "clearing an empty board is not an error");

        assert!(editor.undo().unwrap());
        assert_eq!(editor.board().item_count(), 6, "clearing was not one step");
    }

    /// An additive marquee over a large selection used to be `O(k²)` — a linear scan
    /// of the selection inside the loop over the hits, which is 5×10⁷ comparisons for
    /// ten thousand items and a multi-second freeze on mouse-up, from the app's
    /// *default* gesture.
    #[test]
    fn a_large_additive_marquee_selects_each_item_once_and_quickly() {
        let (_home, mut editor) = editor();
        for i in 0..4_000 {
            editor.board.add(sticky(f64::from(i) * 10.0, 0.0)).unwrap();
        }
        editor.reproject();

        let everything = WorldRect::from_corners(
            WorldPoint::new(-1e6, -1e6),
            WorldPoint::new(1e6, 1e6),
        );
        let started = crate::time::Instant::now();
        editor.marquee(everything, false);
        editor.marquee(everything, true);
        editor.marquee(everything, true);
        let elapsed = started.elapsed();

        assert_eq!(editor.selection().len(), 4_000, "an item was selected twice");
        let mut unique = editor.selection().to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 4_000);
        // Generous by two orders of magnitude against the quadratic version, which
        // took seconds here.
        assert!(elapsed.as_millis() < 500, "three marquees took {elapsed:?}");
    }

    #[test]
    fn typing_shows_on_the_canvas_before_it_reaches_the_document() {
        let (_home, mut editor) = editor();
        let id = editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.reproject();
        let scene = editor.projection().scene_id(id).unwrap();
        let generation = editor.projection().generation();

        editor.preview_text(scene, vellum_doc::StyledText::plain("coolant"));
        let shown = editor.projection().get(scene).unwrap().item.kind.text().unwrap().to_plain();
        assert_eq!(shown, "coolant");
        assert_ne!(
            editor.projection().generation(),
            generation,
            "the laid-out glyphs would not have been re-shaped"
        );
    }

    #[test]
    fn deleting_nothing_is_not_an_error() {
        let (_home, mut editor) = editor();
        assert_eq!(editor.delete_selection().unwrap(), 0);
    }

    /// Reprojection must not leave the selection pointing at items that no longer
    /// exist, or the next frame draws a ring around empty canvas.
    #[test]
    fn reprojecting_prunes_a_stale_selection() {
        let (_home, mut editor) = editor();
        let id = editor.board.add(sticky(0.0, 0.0)).unwrap();
        editor.reproject();
        editor.select_all();
        assert_eq!(editor.selection().len(), 1);

        editor.board.remove(id).unwrap();
        editor.reproject();
        assert!(editor.selection().is_empty());
    }

    #[test]
    fn html_that_is_not_miros_imports_nothing_and_is_not_an_error() {
        let (_home, mut editor) = editor();
        assert!(editor.import_html("<p>hello</p>", None).unwrap().is_none());
        assert_eq!(editor.board().item_count(), 0);
    }

    #[test]
    fn the_data_directory_is_under_a_vellum_folder() {
        let dir = data_directory();
        assert!(dir.ends_with("Vellum"), "{}", dir.display());
        assert!(default_board_path().starts_with(&dir));
        assert!(blob_directory().starts_with(&dir));
        assert_eq!(
            default_board_path().extension().and_then(|e| e.to_str()),
            Some(vellum_store::BOARD_EXTENSION)
        );
    }
}
