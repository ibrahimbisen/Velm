//! Selecting, moving, deleting and undoing — the browser's edit loop.
//!
//! # What this is
//!
//! The core gesture machine for a board in a tab: a press picks or sweeps, a drag previews,
//! a release commits **one** undo step, and `Backspace`, `⌘Z` and `⌘⇧Z` do what they do
//! everywhere else. It is modelled on `vellum_app::editor::Editor` line for line — that
//! module is the one that has been through the user's hands, and every rule here that looks
//! arbitrary is a rule that was paid for there.
//!
//! # ⚠ This makes the client a writer, and the header of `lib.rs` argues it should not be
//!
//! *"a client that could edit would, at that moment, be holding the only recent copy of a
//! board that cannot be re-imported."* That argument is not wrong and it is not answered
//! here — **it is answered by the push half, which another agent owns.** Until that ships,
//! an edit made in a tab lives only in this tab's `Board` and is lost when the tab is
//! closed, reloaded, or has its snapshot replaced by the poll loop.
//!
//! That is why [`EditState::enabled`] is **`false` by default** and why every entry point in
//! this module answers "not mine" while it is off. With editing off, this file changes no
//! behaviour at all: the pointer still pans, the wheel still zooms, and nothing can write to
//! the document. Turning it on is one explicit call, [`velm_set_editing`], which is the
//! decision a person makes rather than one a build flag makes for them.
//!
//! # Nothing here can be tested, and what to do about it
//!
//! `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so **no test in this crate is ever
//! compiled, let alone run.** `cargo test --workspace` will report this file as fully
//! covered by saying nothing about it. So the pure decisions are written as free functions
//! that take plain slices and return plain data — no `Board`, no `Projection`, no `web_sys`
//! — precisely so they can be **moved to `vellum-project`, where the tests run**:
//!
//! - [`widen_to_contents`] — a move carries what is parented to it.
//! - [`marquee_selection`] — the additive algebra.
//! - [`reframe`] — which frame each moved item belongs to now. **This one most of all.**
//!   Its four rules were each earned by a case that went wrong on the desktop, and an
//!   untested reimplementation of it is this repository's signature failure.
//!
//! `look.rs`, `runs.rs`, `frame.rs` and `card.rs` all made that move already and the
//! recommendation is to do the same with these three: `crates/vellum-project/src/edit.rs`,
//! `pub mod edit;` in that crate's `lib.rs`, and this file keeps only the wiring.
//!
//! Meanwhile [`velm_edit_report`] is the honest check that is available: a string a fixture
//! can read after dispatching real pointer events at the real listeners, exactly as
//! `camera_report` exists for the camera. Trap 9's lesson — a fixture that calls the handler
//! directly starts downstream of everything that can go wrong.
//!
//! # ⚠ The invariant that makes a live sync survive a drag
//!
//! **A drag is recomputed absolutely from the placements captured at the press — never
//! incrementally from the last frame.** `live.rs` can rebuild the projection under a
//! gesture at any moment (its timer is not the frame loop), which throws the preview away.
//! Because every [`EditState::drag_to`] rewrites `original + total offset`, the very next
//! pointer move repaints the whole preview correctly and the stomp is invisible. Make it
//! incremental and a poll landing mid-drag silently halves the distance travelled.
//!
//! # ⚠ The undo group, and the trap this file exists downstream of
//!
//! `CLAUDE.md` trap 11: Loro's `group_start` answers `UndoGroupAlreadyStarted` when a group
//! is already open, `group_end` merely clears the slot, there is no depth count, and
//! **nothing else ever closes one** — so a single `?` escaping between begin and end breaks
//! every grouped operation on the board for the rest of the session. It has been re-found
//! three times.
//!
//! `vellum-app` fixed it once at a chokepoint (`Editor::edit`). There is no `Editor` here,
//! so `grouped` is this crate's chokepoint, and it is stronger than a rescue: **its
//! closure returns `T`, not `Result<T>`, so a `?` cannot escape it because there is nothing
//! to `?` on.** Per-item failures are logged and folded into a count, which is also the
//! honest answer — a frame's children go with the frame, so a later id in the same batch
//! being gone is success, not failure.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use wasm_bindgen::prelude::*;

use vellum_doc::{Board, ItemId as DocId, ItemKind, Placement};
use vellum_project::frame::clipped_by_frame;
use vellum_project::project::Projection;
use vellum_scene::{ItemId as SceneId, WorldPoint, WorldRect};

/// How deep a parent chain is walked before giving up.
///
/// The same bound and the same reason as `vellum_project::frame`'s: Miro's own nesting is a
/// frame containing a group containing items, 64 is far past anything real, and a render
/// loop is the worst place to discover a cycle. A corrupt document cannot hang the tab.
const MAX_NESTING: usize = 64;

/// Below this, a released move is treated as no move at all, in **world** units.
///
/// ⚠ **This is not the tap-versus-drag threshold and must not be mistaken for one.** That
/// decision belongs to `input.rs`, where it is `TAP_SLOP` in **CSS pixels** — the only place
/// device pixels exist. A slop expressed in world units is a different distance at every
/// zoom: six units is most of a sticky at 800% and a fifth of a device pixel on a board
/// fitted at 4%. This value exists for one much narrower job — refusing to write a document
/// change, and an undo step, for a gesture that moved nothing measurable.
const MOVE_FLOOR: f64 = 1e-6;

// ---------------------------------------------------------------------------------------
// Pure decisions. No `Board`, no `Projection`, no `web_sys` — ready to move to
// `vellum-project`, where they can be tested. See the module header.
// ---------------------------------------------------------------------------------------

/// `roots`, plus everything parented to them however deeply, in that order.
///
/// **A move carries what is on it.** Nothing in `vellum-doc` composes a parent's transform
/// into a child's — `Placement` is absolute and `parent` is structural — so a frame dragged
/// without this slides out from under every sticky on it. That was feedback 31 on the
/// desktop, and the browser inherits the same document layer and therefore the same bug.
///
/// **A locked child comes too**, which is deliberately *not* what the top-level filter does.
/// A lock means "I did not mean to drag this"; it cannot mean "leave this behind when the
/// surface under it moves", because the result is an item that is no longer on the frame it
/// was pinned to. `Board::remove` already follows this rule for a deleted frame.
///
/// Deduplicated, so selecting a frame *and* a sticky on it moves the sticky once rather than
/// twice as far. Breadth-first over an explicit stack rather than recursion: `panic =
/// "abort"` means a blown stack is a dead tab with no message, and a bounded loop cannot
/// blow one.
///
/// `parent_of` is every item's `(id, parent)` pair — a `Projection` iteration on the calling
/// side, a fixture's literal on the testing side.
pub fn widen_to_contents(
    roots: &[SceneId],
    parent_of: &[(SceneId, Option<SceneId>)],
) -> Vec<SceneId> {
    let mut children: HashMap<SceneId, Vec<SceneId>> = HashMap::new();
    for (id, parent) in parent_of {
        if let Some(parent) = parent {
            children.entry(*parent).or_default().push(*id);
        }
    }

    let mut seen: HashSet<SceneId> = HashSet::with_capacity(roots.len());
    let mut out: Vec<SceneId> = Vec::with_capacity(roots.len());
    let mut frontier: Vec<(SceneId, usize)> = Vec::with_capacity(roots.len());
    for root in roots {
        if seen.insert(*root) {
            out.push(*root);
            frontier.push((*root, 0));
        }
    }
    while let Some((id, depth)) = frontier.pop() {
        // Bounded rather than trusted: a cycle in the parent chain would otherwise be an
        // infinite loop, and `seen` already makes it terminate — this is the second belt,
        // for a document deep enough to be pathological without being cyclic.
        if depth >= MAX_NESTING {
            continue;
        }
        let Some(kids) = children.get(&id) else { continue };
        for kid in kids {
            if seen.insert(*kid) {
                out.push(*kid);
                frontier.push((*kid, depth + 1));
            }
        }
    }
    out
}

/// The selection a marquee produces, given the selection the gesture started with.
///
/// Split out because it is the one piece of selection algebra with an ordering rule worth
/// asserting: an additive sweep keeps `base` **first**, in its existing order, and appends
/// only what is new. A `HashSet` round trip would be one line shorter and would renumber the
/// selection on every pointer move, which is a different z-order for every operation that
/// walks it.
pub fn marquee_selection(base: &[SceneId], hits: &[SceneId], additive: bool) -> Vec<SceneId> {
    if !additive {
        return hits.to_vec();
    }
    let mut seen: HashSet<SceneId> = base.iter().copied().collect();
    let mut out = base.to_vec();
    out.reserve(hits.len());
    for id in hits {
        if seen.insert(*id) {
            out.push(*id);
        }
    }
    out
}

/// One item, as [`reframe`] needs to see it. Placement is where it is **after** the move.
#[derive(Debug, Clone, Copy)]
pub struct Filing {
    pub id: DocId,
    pub parent: Option<DocId>,
    /// Destination, not origin. The caller resolves the move before calling.
    pub placement: Placement,
    pub is_frame: bool,
}

/// Which frame each moved item belongs to now.
///
/// **A frame clips its contents, so an item dragged off one and left parented to it simply
/// stops being drawn.** `vellum_project::frame::clipped_by_frame` is applied by this
/// client's painter and by its tap path already, so without this an item dragged off a frame
/// in a browser vanishes exactly as it did on the desktop: still there, still selectable,
/// drawing a selection ring around nothing. That was feedback 39, and the tell — recorded
/// there because it is the transferable part — is that **the rings survive and the content
/// does not**, which no missing texture can do.
///
/// Four rules, each of which is a case that went wrong when it was written the other way:
///
/// - **The item's centre decides**, not its overlap. Overlap makes an item straddling two
///   frames belong to both, and one hanging off an edge belong to a frame it is mostly
///   outside of; requiring containment means a sticky wider than its frame can never be on
///   one.
/// - **The innermost frame wins**, by area, so nesting resolves the way the eye reads it.
/// - **A frame is never adopted.** Frames are backdrops; a frame dragged over another would
///   otherwise become its child, inherit its clipping, and take everything on it out of the
///   drawing at once.
/// - **Only items already loose or already on a frame are considered.** An item inside a
///   *group* keeps its group — re-filing it under a frame would silently dismantle the
///   group, and grouping is the user's arrangement rather than a consequence of geometry.
///
/// Returns only the items whose parent actually changes, so an unchanged filing costs no
/// CRDT write and no undo content.
pub fn reframe(items: &[Filing], moved: &HashSet<DocId>) -> Vec<(DocId, Option<DocId>)> {
    let frames: Vec<&Filing> = items.iter().filter(|item| item.is_frame).collect();
    if frames.is_empty() {
        return Vec::new();
    }
    let is_frame: HashSet<DocId> = frames.iter().map(|frame| frame.id).collect();

    let mut out = Vec::new();
    for item in items {
        if !moved.contains(&item.id) || item.is_frame {
            continue;
        }
        // Loose, or already on a frame. Anything else — a group's member, a widget's part —
        // is somebody else's child and stays one.
        if let Some(parent) = item.parent
            && !is_frame.contains(&parent)
        {
            continue;
        }

        let (cx, cy) = (item.placement.x, item.placement.y);
        let home = frames
            .iter()
            .copied()
            .filter(|frame| {
                frame.id != item.id && {
                    let (w, h) = frame.placement.scaled_size();
                    cx >= frame.placement.x - w / 2.0
                        && cx <= frame.placement.x + w / 2.0
                        && cy >= frame.placement.y - h / 2.0
                        && cy <= frame.placement.y + h / 2.0
                }
            })
            // `total_cmp`, not `partial_cmp().unwrap()` — a NaN extent from a corrupt
            // document is a dead tab under `panic = "abort"`, and it orders rather than
            // refuses.
            .min_by(|a, b| {
                let area = |f: &&Filing| {
                    let (w, h) = f.placement.scaled_size();
                    w * h
                };
                area(a).total_cmp(&area(b))
            })
            .map(|frame| frame.id);

        if home != item.parent {
            out.push((item.id, home));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// The gesture machine.
// ---------------------------------------------------------------------------------------

/// What a released gesture asks the document to do.
///
/// Deliberately a value rather than a call: a gesture ends in `input.rs`, where the borrows
/// are a pointer handler's, and the write needs `&mut Board`. Handing back a description
/// keeps the decision and the write in two places that can each be read on their own — and
/// makes `Commit::None` a real answer rather than an early return nobody can see.
#[derive(Debug, Clone, PartialEq)]
pub enum Commit {
    Move { items: Vec<SceneId>, by: (f64, f64) },
    None,
}

/// The gesture in flight, if there is one.
#[derive(Debug, Clone)]
enum Gesture {
    Move {
        /// Every item the gesture carries — the picked ones plus their descendants — each
        /// with the placement it had **when the press landed**.
        ///
        /// ⚠ The originals are kept rather than the last frame's positions, and that is
        /// what makes the drag idempotent: see the module header on `live.rs` stomping a
        /// preview mid-gesture.
        items: Vec<(SceneId, Placement)>,
        from: WorldPoint,
        offset: (f64, f64),
    },
    Marquee {
        from: WorldPoint,
        to: WorldPoint,
        additive: bool,
        /// The selection at the press, so an additive sweep can be recomputed from scratch
        /// on every pointer move rather than accumulated — a sweep that shrinks back must
        /// un-select what it has left behind.
        base: Vec<SceneId>,
    },
}

/// Selection, the gesture in flight, and the one switch that turns writing on.
#[derive(Debug, Default)]
pub struct EditState {
    selection: Vec<SceneId>,
    gesture: Option<Gesture>,
    /// ⚠ **Off by default.** See the module header: until the push half ships, an edit in a
    /// tab is volatile. It also means this whole file is behaviourally inert until somebody
    /// asks for it, so wiring it cannot regress the viewer.
    enabled: bool,
}

impl EditState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The selection, in the order it was built — which for a marquee is paint order, so an
    /// operation over several items reads the same way twice.
    #[must_use]
    pub fn selection(&self) -> &[SceneId] {
        &self.selection
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns writing on or off. Turning it **off** drops the selection and any gesture, so
    /// there is no way to leave a half-finished drag behind a switch nobody can see.
    ///
    /// The preview is deliberately *not* restored here, because that would need the
    /// projection; the caller ([`velm_set_editing`]) does it, which is why that is the door.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.gesture = None;
            self.selection.clear();
        }
    }

    /// A press at a world point. `additive` for a shift-click. Answers whether anything is
    /// selected afterwards.
    ///
    /// ⚠ **Takes `&mut Projection`, where the sketch for this module said `&Projection`.**
    /// Two reasons, and both are the source's rather than a preference. A press arriving
    /// while a gesture is still live — a `pointerup` the browser swallowed, a system gesture
    /// — has to put the previous preview back, and a preview lives in the projection
    /// ([`Projection::moved`], which is `&mut self`). Without it a lost release leaves items
    /// drawn where they are not, with the document disagreeing, until something else happens
    /// to reproject. On a touchscreen that is not a corner case.
    ///
    /// # What a press decides, in order
    ///
    /// 1. **What is under it**, through `Scene::hit_test_where` and never `hit_test` then a
    ///    filter. `scene.rs` states the rule verbatim: the predicate is applied *before*
    ///    "topmost", so a rejected item is genuinely transparent. Filtered afterwards, a
    ///    locked sticky lying over a frame turns into a **hole** — the frame under it stops
    ///    being clickable through it. Two things are rejected here: **locked** items
    ///    (`Style::locked`, the lock the desktop enforces at the pointer rather than in the
    ///    document) and items an ancestor frame has **clipped** away, which is the same
    ///    predicate the badge-opening tap path already applies four lines from here.
    /// 2. **Whether the selection changes.** A non-additive press *inside* an existing
    ///    selection leaves it alone, so dragging one of five stickies moves all five.
    ///
    ///    ⚠ **Miro additionally collapses to the one item you tapped when the tap does not
    ///    travel, and this deliberately does not.** The desktop app does not either — its
    ///    press arm is `Some(id) if !additive && contains => {}` and nothing at its release
    ///    narrows the selection — and the two front ends drawing and behaving as one board
    ///    is worth more here than matching Miro on a point you can already reach by
    ///    clicking bare board first. Recorded rather than left implicit, because it is a
    ///    real difference from Miro and it is one line to change: remember `hit` on the
    ///    gesture and, in `release`, replace the selection with it when the offset is below
    ///    `MOVE_FLOOR`.
    /// 3. **⚠ Whether this is a backdrop.** A press on a frame that is *not already
    ///    selected* sweeps a marquee instead of picking the frame up. *"when i try to drag
    ///    select multiple things, if i start dragging while being on top of a frame it
    ///    starts moving the frame instead of drag selecting."* A frame is usually the
    ///    largest thing on the board and everything sits on top of it, so grabbing on press
    ///    makes marquee selection impossible over most of the board. The press still
    ///    *selects* it — so a tap leaves it selected and the **next** drag moves it. One
    ///    click to pick it up, then drag.
    ///
    ///    ⚠ `was_selected` is read **before** step 2 mutates the selection. Asked
    ///    afterwards it is always true, the backdrop rule never fires, and the drag moves
    ///    the frame as before — code that looks right. The desktop's fixture caught exactly
    ///    that, which is the only reason this line is in the right order.
    ///
    ///    Scoped to frames rather than to "any container", deliberately: a table, a kanban
    ///    and a mind map are all things you drag as a unit, and none of them is what other
    ///    items sit on top of.
    pub fn press(&mut self, at: WorldPoint, additive: bool, projection: &mut Projection) -> bool {
        if !self.enabled {
            return false;
        }
        // A gesture still live means a release was lost. Put its preview back before
        // starting another, or the second drag layers on top of the first.
        self.cancel(projection);

        let view: &Projection = projection;
        let hit = view.scene().hit_test_where(at, |id| pickable(id, view));
        // ⚠ Before the pick below, not after. See the doc comment.
        let was_selected = hit.is_some_and(|id| self.selection.contains(&id));

        match hit {
            // Inside an existing selection: leave it alone so a drag moves the whole thing.
            Some(_) if !additive && was_selected => {}
            Some(id) => {
                if additive {
                    match self.selection.iter().position(|held| *held == id) {
                        Some(index) => {
                            self.selection.remove(index);
                        }
                        None => self.selection.push(id),
                    }
                } else {
                    self.selection.clear();
                    self.selection.push(id);
                }
            }
            // A shift-click on bare board adds nothing and must not take anything away.
            None if !additive => self.selection.clear(),
            None => {}
        }

        let backdrop = !was_selected
            && hit.is_some_and(|id| {
                projection
                    .get(id)
                    .is_some_and(|projected| matches!(projected.item.kind, ItemKind::Frame { .. }))
            });

        let gesture = match hit.filter(|_| !backdrop) {
            Some(_) => self.begin_move(at, projection),
            None => Some(Gesture::Marquee {
                from: at,
                to: at,
                additive,
                base: self.selection.clone(),
            }),
        };
        self.gesture = gesture;
        !self.selection.is_empty()
    }

    /// The move gesture a press starts, or `None` when there is nothing movable.
    ///
    /// The carried set is the **selection**, not the item that was hit: a press inside a
    /// five-item selection moves five. Locked items are dropped at the top level and picked
    /// back up as children by [`widen_to_contents`] — see its note on why those are two
    /// different answers to what looks like one question.
    fn begin_move(&self, at: WorldPoint, projection: &Projection) -> Option<Gesture> {
        let roots: Vec<SceneId> = self
            .selection
            .iter()
            .copied()
            .filter(|id| {
                projection
                    .get(*id)
                    .is_some_and(|projected| !projected.item.style.locked)
            })
            .collect();
        if roots.is_empty() {
            return None;
        }
        let parent_of: Vec<(SceneId, Option<SceneId>)> = projection
            .iter()
            .map(|(id, projected)| (*id, projected.parent))
            .collect();
        let items: Vec<(SceneId, Placement)> = widen_to_contents(&roots, &parent_of)
            .into_iter()
            .filter_map(|id| projection.get(id).map(|p| (id, p.item.placement)))
            .collect();
        if items.is_empty() {
            return None;
        }
        Some(Gesture::Move { items, from: at, offset: (0.0, 0.0) })
    }

    /// The pointer moved with the button down — preview a move, or extend a marquee.
    ///
    /// ⚠ **`&mut Projection`, and it cannot be otherwise.** A drag must write nothing to the
    /// document until the button comes up: every frame of a drag would otherwise be a Loro
    /// transaction plus a full reprojection, measured at 57 ms per frame on a sixteen-
    /// thousand-item board, for a position the user has not committed to. `Projection::moved`
    /// is the mechanism — an `O(log n)` remove-and-reinsert in the R-tree that keeps the
    /// index, the hit-test and the painter in step — and it takes `&mut self`.
    ///
    /// ⚠ **Only call this once the pointer has cleared `input.rs`'s own slop.** That
    /// threshold is in CSS pixels because that is where device pixels live; see
    /// `MOVE_FLOOR` for why it does not and cannot live here.
    ///
    /// The offset is recomputed from the press point every time. See the module header.
    pub fn drag_to(&mut self, at: WorldPoint, projection: &mut Projection) {
        // Resolved inside the match and written after it, because the selection and the
        // gesture are two fields of the same `self` and the marquee arm needs both.
        let swept = match self.gesture.as_mut() {
            Some(Gesture::Move { items, from, offset }) => {
                let by = (at.x - from.x, at.y - from.y);
                *offset = by;
                for (id, original) in items.iter() {
                    let mut placement = *original;
                    placement.x = original.x + by.0;
                    placement.y = original.y + by.1;
                    projection.moved(*id, placement);
                }
                None
            }
            Some(Gesture::Marquee { from, to, additive, base }) => {
                *to = at;
                let rect = WorldRect::from_corners(*from, at);
                Some((rect, *additive, base.clone()))
            }
            None => None,
        };
        let Some((rect, additive, base)) = swept else { return };
        let hits = marquee_hits_visible(projection, rect);
        self.selection = marquee_selection(&base, &hits, additive);
    }

    /// The button came up. Answers what to commit, if anything.
    ///
    /// A marquee commits nothing — a selection is not a document change — and a move that
    /// went nowhere commits nothing either, so a click cannot leave an undo step behind it.
    pub fn release(&mut self) -> Option<Commit> {
        let gesture = self.gesture.take()?;
        match gesture {
            Gesture::Move { items, offset, .. } => {
                if offset.0.abs() < MOVE_FLOOR && offset.1.abs() < MOVE_FLOOR {
                    return Some(Commit::None);
                }
                Some(Commit::Move {
                    items: items.into_iter().map(|(id, _)| id).collect(),
                    by: offset,
                })
            }
            Gesture::Marquee { .. } => Some(Commit::None),
        }
    }

    /// The gesture ended without a release — Escape, `pointercancel`, a second finger
    /// landing, the tab being hidden, editing being switched off.
    ///
    /// ⚠ **`&mut Projection`, where the sketch said no argument at all, and this is the
    /// change with the sharpest consequence.** `drag_to` writes its preview *into the
    /// projection*; a cancel that cannot reach the projection leaves every dragged item
    /// drawn at the dragged position with the document unchanged, and nothing later puts it
    /// back except an unrelated reprojection. `pointercancel` is routine on a touchscreen —
    /// a system edge gesture, a notification, four fingers — so this is the ordinary path,
    /// not the exotic one.
    ///
    /// `CLAUDE.md` feedback 27's rule, which this repository has now paid for on four
    /// separate gestures: **give every way a gesture can end without a release a call to the
    /// function that closes it.** Idempotent, so calling it defensively costs nothing.
    pub fn cancel(&mut self, projection: &mut Projection) {
        let Some(gesture) = self.gesture.take() else { return };
        match gesture {
            Gesture::Move { items, .. } => {
                for (id, original) in items {
                    projection.moved(id, original);
                }
            }
            // A cancelled sweep puts the selection back to what the press found, rather than
            // leaving it wherever the rectangle happened to be when the browser interrupted.
            Gesture::Marquee { base, .. } => self.selection = base,
        }
    }

    /// Selects everything on the board, in a deterministic order.
    ///
    /// Locked and clipped items are **included**, matching the desktop: a lock is enforced
    /// where an operation writes, not where a selection is built, so that `⌘A` then Delete
    /// removes everything except the locked items rather than refusing outright.
    pub fn select_all(&mut self, projection: &Projection) {
        if !self.enabled {
            return;
        }
        let mut ids: Vec<SceneId> = projection.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        self.selection = ids;
    }

    /// Replaces the selection with `ids`, keeping only the ones the projection still holds.
    ///
    /// The door a **creating** operation needs. A paste and a duplicate both answer with
    /// document ids, and an item created a moment ago has no scene id until
    /// `Projection::rebuild` has interned one — so this is called after the rebuild, never
    /// before, or it selects nothing at all.
    ///
    /// ⚠ Selecting the result is the only feedback either gesture gives. A duplicate lands
    /// 24 world units from its original, which at a fitted 4% is one device pixel: without
    /// this, a duplicate and a no-op are indistinguishable on screen.
    ///
    /// Filtered rather than trusted, for [`EditState::sync`]'s reason — an id the projection
    /// does not hold draws a ring around nothing.
    pub fn select(&mut self, ids: Vec<SceneId>, projection: &Projection) {
        if !self.enabled {
            return;
        }
        self.selection = ids;
        self.sync(projection);
    }

    /// Selects exactly `id`, dropping whatever was held.
    ///
    /// The one thing a create gesture needs and `press` cannot give it: a newly added item is
    /// topmost among its siblings, but a **frame** takes a negative z band, so pressing at its
    /// own centre would pick up whatever is drawn on top of it. `crate::tools` calls this
    /// after the projection has been rebuilt, so the scene id exists.
    pub fn select_only(&mut self, id: SceneId) {
        self.selection.clear();
        self.selection.push(id);
    }

    pub fn clear(&mut self) {
        self.selection.clear();
    }

    /// How far the preview has moved, or `None` when no move is in flight.
    ///
    /// ⚠ **The painter does not need this to place a dragged item.** The preview goes
    /// through `Projection::moved`, so the items' bounds, their quads, their text and the
    /// R-tree behind the hit-test are *already* at the dragged position — a painter that
    /// also applied this offset would move everything twice. It is the answer to "is a move
    /// in flight, and how far", which the status line and [`velm_edit_report`] want and
    /// which a future snap indicator would measure against. Stated plainly because a
    /// function called `moved_offset` on an edit module reads like the other thing.
    #[must_use]
    pub fn moved_offset(&self) -> Option<(f64, f64)> {
        match &self.gesture {
            Some(Gesture::Move { offset, .. }) => Some(*offset),
            _ => None,
        }
    }

    /// The marquee rectangle, in **world** units, for the painter. `None` when not sweeping.
    #[must_use]
    pub fn marquee(&self) -> Option<WorldRect> {
        match &self.gesture {
            Some(Gesture::Marquee { from, to, .. }) => Some(WorldRect::from_corners(*from, *to)),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_dragging(&self) -> bool {
        self.gesture.is_some()
    }

    /// Drops any id the projection no longer holds.
    ///
    /// ⚠ **The one repair that keeps a selection honest across every kind of rebuild**, and
    /// there are four: a delete here, an undo, a redo, and `live.rs` applying an update from
    /// the server on its own timer — which this module cannot wire and must not depend on.
    /// So rather than asking every one of those to remember, [`EditState::rings`] calls this
    /// on the way to the painter: a stale id cannot survive a single frame, whatever created
    /// it. Public because calling it earlier is free and never wrong.
    ///
    /// # Why this retains rather than clearing, where the desktop clears
    ///
    /// `Editor::undo` drops the selection outright, on the correct observation that Loro
    /// re-creates a deleted node under a **new** `ItemId` so ids cannot be held across an
    /// undo. Retaining is safe *here* for a reason worth stating so nobody restores the
    /// clear: `Projection::intern` counts `next_id` upwards and **never reuses a
    /// `SceneId`**, so a re-created item gets a fresh one and the old one is simply absent
    /// — pruned by the line below. Nothing can silently come to name a different item. The
    /// payoff is that undoing a *move* keeps what you had selected, which is what an undo of
    /// a move should feel like.
    pub fn sync(&mut self, projection: &Projection) {
        self.selection.retain(|id| projection.get(*id).is_some());
        if let Some(Gesture::Move { items, .. }) = self.gesture.as_mut() {
            items.retain(|(id, _)| projection.get(*id).is_some());
        }
    }
}

/// Whether a press may pick this item: not locked, and not clipped away by a frame.
///
/// Handed to `Scene::hit_test_where` rather than applied to `hit_test`'s answer. See
/// [`EditState::press`] for the whole argument; the short version is that a filter applied
/// afterwards turns a rejected item into a hole in the board.
pub(crate) fn pickable(id: SceneId, projection: &Projection) -> bool {
    projection.get(id).is_some_and(|projected| {
        !projected.item.style.locked && !clipped_by_frame(projected, projection)
    })
}

/// What a marquee catches, minus anything a frame has clipped out of the drawing.
///
/// `Projection::marquee_hits` owns the two rules that matter — intersection rather than
/// containment, so a sweep that clips a sticky's corner takes it, **except** for a frame,
/// which is only caught when the sweep contains the whole of it. Both are Miro's, and using
/// that function rather than a local query is what stops a live preview promising a
/// selection the release does not deliver.
///
/// The clipping filter is this module's addition, and it is the same predicate the tap path
/// and both paint passes already apply: an item that is not drawn must not be selectable, or
/// a sweep over apparently empty board silently picks up items nobody can see.
fn marquee_hits_visible(projection: &Projection, rect: WorldRect) -> Vec<SceneId> {
    let view: &Projection = projection;
    view.marquee_hits(rect)
        .into_iter()
        .filter(|id| {
            view.get(*id)
                .is_some_and(|projected| !clipped_by_frame(projected, view))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Document operations.
// ---------------------------------------------------------------------------------------

/// Runs `f` inside one undo group, and closes the group on **every** path.
///
/// This crate's answer to `Editor::edit`, and it is deliberately stronger than that one.
/// `Editor::edit` takes a fallible closure and rescues the group when it fails; this takes
/// an **infallible** one, so there is nothing to `?` on and trap 11's failure — a `?`
/// escaping between begin and end — is unrepresentable rather than recovered. Per-item
/// errors are logged inside and folded into whatever `T` the caller counts, which is also
/// the truthful shape: a frame takes its subtree, so a later id in the same batch already
/// being gone is success.
///
/// # The begin-failure path
///
/// `begin_undo_group` has exactly one failure, *"there is already an active group"*, which
/// means a group has leaked from somewhere. Closing it and refusing this operation is the
/// rescue `Editor::edit` performs for the same reason: the alternative is a board on which
/// no grouped operation works again until the tab is reloaded. The next operation succeeds.
pub(crate) fn grouped<T>(board: &mut Board, f: impl FnOnce(&mut Board) -> T) -> Result<T, String> {
    if let Err(error) = board.begin_undo_group() {
        // Not `?` — closing the leaked group is the whole point of being here.
        board.end_undo_group();
        return Err(format!("could not start an undo step: {error}"));
    }
    let value = f(&mut *board);
    board.end_undo_group();
    Ok(value)
}

/// Rebuilds the projection from the document, logging rather than failing.
///
/// ⚠ **Called on every path out of a mutation, including the failure one.** A closure that
/// failed part way has usually already changed the document, and a projection left stale
/// does not merely draw the old board — it *hit-tests* the old board, so clicks land where
/// items used to be. That is `Editor::edit`'s hardest-won line and it is the same here.
pub(crate) fn resettle(board: &Board, projection: &mut Projection) {
    if let Err(error) = projection.rebuild(board) {
        log::error!("velm edit: laying the board out again: {error}");
    }
}

/// Applies a released gesture to the document and re-derives everything.
///
/// Answers whether the document changed, so a caller can tell a real edit from a no-op
/// without counting anything.
///
/// # Three things about how the move is written
///
/// **`Board::translate`, not `set_placement`.** Its own doc comment names this exact caller:
/// *"Dragging N items is N of these inside one `begin_undo_group`."* It writes two keys
/// instead of six, and — the reason that matters more — it **composes** with a concurrent
/// remote edit. This client polls for updates and applies them with `Board::apply`; writing
/// a whole `Placement` captured at press time would clobber a resize or a rotation that
/// arrived from another machine during the drag, where adding a delta does not.
///
/// **The destination is read from the *document*, never from the projection.** By the time a
/// gesture releases, the projection holds the **preview** — that is what `drag_to` put
/// there — so `projected.item.placement` is already the answer and adding `by` to it would
/// move everything twice. `board.item(doc)` is the only honest source.
///
/// **The reparenting is written before the placements**, and both are inside one group. A
/// move that changes what an item belongs to is still one thing the user did, so `⌘Z` has to
/// put both halves back together — and a second `begin_undo_group` would answer
/// `UndoGroupAlreadyStarted` and break every grouped operation after it (trap 11).
pub fn commit(
    board: &mut Board,
    projection: &mut Projection,
    commit: Commit,
) -> Result<bool, String> {
    let (items, by) = match commit {
        Commit::Move { items, by } => (items, by),
        Commit::None => return Ok(false),
    };
    if items.is_empty() || (by.0.abs() < MOVE_FLOOR && by.1.abs() < MOVE_FLOOR) {
        return Ok(false);
    }

    // Scene ids to document ids, with where each one lands. `order` keeps the write
    // deterministic; `destinations` is what the filing decision reads.
    let mut destinations: HashMap<DocId, Placement> = HashMap::with_capacity(items.len());
    let mut order: Vec<DocId> = Vec::with_capacity(items.len());
    for scene in &items {
        // Silently skipped rather than refused: an item can be deleted from under a drag by
        // an undo or by a frame that took its children, and the rest of the move still
        // stands — which is the half the user watched happen.
        let Some(projected) = projection.get(*scene) else { continue };
        let doc = projected.doc_id;
        let Ok(item) = board.item(doc) else { continue };
        let mut destination = item.placement;
        destination.x += by.0;
        destination.y += by.1;
        if destinations.insert(doc, destination).is_none() {
            order.push(doc);
        }
    }
    if order.is_empty() {
        resettle(board, projection);
        return Ok(false);
    }

    let reparents = reframe_on(board, &destinations);
    let written = grouped(&mut *board, |board| {
        for (doc, parent) in &reparents {
            if let Err(error) = board.reparent(*doc, *parent) {
                // Refused rather than fatal: a cycle, or an item deleted from under the
                // drag. The move itself still stands.
                log::debug!("velm edit: refiling {doc}: {error}");
            }
        }
        let mut written = 0usize;
        for doc in &order {
            match board.translate(*doc, by.0, by.1) {
                Ok(()) => written += 1,
                Err(error) => log::debug!("velm edit: moving {doc}: {error}"),
            }
        }
        written
    });

    resettle(board, projection);
    written.map(|count| count > 0)
}

/// [`reframe`] over a live board. Reads the document once and decides purely.
fn reframe_on(board: &Board, destinations: &HashMap<DocId, Placement>) -> Vec<(DocId, Option<DocId>)> {
    let Ok(items) = board.items() else {
        log::debug!("velm edit: could not read the board to refile a move");
        return Vec::new();
    };
    let filings: Vec<Filing> = items
        .iter()
        .map(|item| Filing {
            id: item.id,
            parent: item.parent,
            // Where everything is *after* the drag: a dragged frame has moved too, and
            // deciding against its old box would file items into where it used to be.
            placement: destinations.get(&item.id).copied().unwrap_or(item.placement),
            is_frame: matches!(item.kind, ItemKind::Frame { .. }),
        })
        .collect();
    let moved: HashSet<DocId> = destinations.keys().copied().collect();
    reframe(&filings, &moved)
}

/// Deletes `items` in one undoable step. Answers how many items left the board.
///
/// # Three rules, all of them the desktop's
///
/// **Locked items are left behind.** `Style::locked` is enforced where the write happens
/// rather than at the gate, because the gate only ever disables a command when *everything*
/// selected is locked — which leaves the case a lock exists to survive, `⌘A` over a board
/// with one locked item, wide open.
///
/// **Roots only.** `Board::remove` takes the whole subtree and refuses an id its own
/// previous call already removed, so asking for a frame and then for a sticky on it is an
/// error report for something that worked. Anything with a doomed ancestor is dropped from
/// the list before the group opens.
///
/// **The count is what left the board**, taken from the item total rather than from the
/// loop. A loop counts the removals it asked for; a frame carrying nine children is one
/// removal and ten items, and the user watched ten disappear.
///
/// A locked item *inside* a deleted frame still goes, because the subtree cannot be asked
/// for in part. That is the narrower hole the desktop records rather than papers over.
pub fn delete(
    board: &mut Board,
    projection: &mut Projection,
    items: &[SceneId],
) -> Result<usize, String> {
    let doomed: Vec<DocId> = items
        .iter()
        .filter_map(|id| projection.get(*id))
        .filter(|projected| !projected.item.style.locked)
        .map(|projected| projected.doc_id)
        .collect();
    if doomed.is_empty() {
        return Ok(0);
    }
    let set: HashSet<DocId> = doomed.iter().copied().collect();
    let roots: Vec<DocId> = {
        let view: &Board = board;
        doomed
            .iter()
            .copied()
            .filter(|id| !has_doomed_ancestor(view, *id, &set))
            .collect()
    };

    let before = board.item_count();
    let removed = grouped(&mut *board, |board| {
        for id in &roots {
            if let Err(error) = board.remove(*id) {
                log::debug!("velm edit: removing {id}: {error}");
            }
        }
        before.saturating_sub(board.item_count())
    });

    resettle(board, projection);
    removed
}

/// Whether any ancestor of `id` is also being deleted. Bounded, for the reason
/// `MAX_NESTING` gives.
fn has_doomed_ancestor(board: &Board, id: DocId, doomed: &HashSet<DocId>) -> bool {
    let mut parent = board.parent_of(id);
    for _ in 0..MAX_NESTING {
        let Some(current) = parent else { return false };
        if doomed.contains(&current) {
            return true;
        }
        parent = board.parent_of(current);
    }
    false
}

/// Reverses the last local edit. Answers whether anything was undone.
///
/// No undo group is opened here, so there is nothing to leak — but a gesture in flight must
/// be cancelled *before* this runs, or a live preview sits on top of a rewound document.
/// [`velm_undo`] and [`velm_redo`] are the doors that do it.
pub fn undo(board: &mut Board, projection: &mut Projection) -> Result<bool, String> {
    let changed = board.undo().map_err(|error| format!("undo: {error}"))?;
    if changed {
        resettle(board, projection);
    }
    Ok(changed)
}

/// Reapplies the last undone edit. Answers whether anything was redone.
pub fn redo(board: &mut Board, projection: &mut Projection) -> Result<bool, String> {
    let changed = board.redo().map_err(|error| format!("redo: {error}"))?;
    if changed {
        resettle(board, projection);
    }
    Ok(changed)
}

// ---------------------------------------------------------------------------------------
// What the painter needs.
//
// A second `impl` block rather than more methods above, so the painter-facing API sits with
// the type it hands back. [`EditState`] itself is a field on `Viewer` — it holds no globals
// and reaches for nothing — which is what keeps the whole of this module a value the viewer
// owns rather than a second piece of page state that could disagree with it.
// ---------------------------------------------------------------------------------------

/// One selection ring, in **world** units.
///
/// The item's own unrotated box plus its rotation, rather than the axis-aligned bounds,
/// because a ring drawn on the AABB of a rotated sticky is a box *around* the sticky rather
/// than an outline *on* it. `rotation` is radians clockwise, which is what every renderer
/// instance field already takes.
///
/// ⚠ **The stroke must be sized in screen pixels, not world units.** The desktop's
/// `SELECTION_RING_WIDTH` is **one device pixel** and it is its own constant precisely
/// because that number was tuned by hand (feedback 32): a ring holds its device width while
/// the item under it shrinks with the zoom, so anything thicker reads as a band around a
/// small item on a fitted board. A world-unit stroke is invisible at 4% and a slab at 8×.
#[derive(Debug, Clone, Copy)]
pub struct Ring {
    pub centre: WorldPoint,
    pub size: (f64, f64),
    pub rotation: f32,
}

impl EditState {
    /// The rings the painter should draw — and the one place the selection is pruned.
    ///
    /// ⚠ **The pruning is here on purpose.** Four different things rebuild the projection
    /// and one of them is `live.rs`'s poll, which runs on its own timer and which this
    /// module does not wire. Asking each of them to remember [`EditState::sync`] is asking
    /// somebody to remember; doing it on the way to the painter means a stale id cannot
    /// survive a single frame, whatever created it. It costs one `retain` over a list that
    /// is almost always short, and [`EditState::sync`] stays public for a caller that wants
    /// it sooner.
    pub fn rings(&mut self, projection: &Projection) -> Vec<Ring> {
        self.sync(projection);
        self.selection
            .iter()
            .filter_map(|id| projection.get(*id))
            .map(|projected| Ring {
                centre: WorldPoint::new(projected.item.placement.x, projected.item.placement.y),
                size: projected.item.placement.scaled_size(),
                rotation: projected.rotation(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// The hops `input.rs` calls — one line each, so the file this module does not own stays
// almost unchanged, and so the decisions stay here where they can be read together.
// ---------------------------------------------------------------------------------------

/// A pointer went down on the canvas. Answers whether the edit layer took the gesture.
///
/// `false` means "not mine" — editing is off, or the press found nothing to drag or sweep —
/// and the caller should carry on panning exactly as it does today. That is what makes
/// wiring this **additive**: with editing off every answer is `false`, and the viewer
/// behaves as it did before this file existed.
///
/// # ⚠ One interaction the caller must decide, because this cannot see it
///
/// `input.rs` already opens a link card's page from `pointerup`, gated on
/// `badges::pressed` — so with editing on, a tap on a card's ↗ both **selects the card**
/// and **opens its page**. Neither half is wrong on its own and this module has no way to
/// tell: a badge is geometry the badge layer owns.
///
/// The two answers, and the second is the recommendation: accept it, since selecting the
/// thing you just opened is harmless; or ask `badges::pressed` **before** this call in the
/// `pointerdown` handler and skip the press when it hits, which keeps the badge a pure
/// button. The desktop app chose the second (feedback 25 guarded the badge against
/// double-firing and against swallowing a ⇧-click), so the second is the parity answer.
pub fn pointer_down(viewer: &mut crate::Viewer, at: WorldPoint, additive: bool) -> bool {
    let crate::Viewer { edit, projection, .. } = viewer;
    edit.press(at, additive, projection);
    // A marquee counts as taken too: a one-finger drag cannot both sweep a selection and
    // pan the board, and answering `false` here would do both at once.
    edit.is_dragging()
}

/// The pointer moved with the button down, **after** `input.rs`'s own slop.
///
/// Answers whether the edit layer consumed the motion, so the caller can skip the camera
/// pan.
pub fn pointer_move(viewer: &mut crate::Viewer, at: WorldPoint) -> bool {
    let crate::Viewer { edit, projection, .. } = viewer;
    if !edit.is_dragging() {
        return false;
    }
    edit.drag_to(at, projection);
    true
}

/// The pointer came up. Commits whatever the gesture asks for, tells the push half, and
/// answers whether the document changed.
pub fn pointer_up(viewer: &mut crate::Viewer) -> bool {
    let crate::Viewer { edit, board, projection, push, .. } = viewer;
    let Some(what) = edit.release() else { return false };
    match commit(board, projection, what) {
        Ok(changed) => {
            if changed {
                announce(push, board);
            }
            changed
        }
        Err(error) => {
            log::error!("velm edit: {error}");
            false
        }
    }
}

/// The gesture ended without a release — `pointercancel`, a second finger landing, Escape,
/// the page being hidden. Idempotent, and free when nothing is in flight.
///
/// `CLAUDE.md` feedback 27's rule, which this repository has paid for on four separate
/// gestures: **give every way a gesture can end without a release a call to the function
/// that closes it.**
pub fn pointer_cancel(viewer: &mut crate::Viewer) {
    let crate::Viewer { edit, projection, .. } = viewer;
    edit.cancel(projection);
}

/// Tells the push half there is something to send, and sends it this frame.
///
/// ⚠ **After the mutation and after the reprojection, never before** — `Pusher::note_edit`'s
/// own documentation states it: `tick` exports the board as it stands, so a `note_edit` that
/// ran first and a `tick` that ran before the write would send an empty delta and mark the
/// real edit acknowledged. Every path in this file that changes the document ends here, and
/// only when it actually changed one: a spurious call costs a full `export_since` on a board
/// that had nothing to say.
///
/// Silently nothing when there is no server behind this board — a static `board.bin` has
/// nowhere to push to, and that is the ordinary development case rather than an error.
pub(crate) fn announce(push: &mut Option<crate::push::Pusher>, board: &Board) {
    if let Some(pusher) = push.as_mut() {
        pusher.note_edit();
        pusher.tick(board);
    }
}

// ---------------------------------------------------------------------------------------
// The exports the page calls. Prefixed `velm_` — unlike `zoom_by` and `fit_board`, which
// are not — because three of them would otherwise collide with this module's own `undo`,
// `redo` and `delete`, and because `delete` is a reserved word in JavaScript.
// ---------------------------------------------------------------------------------------

/// The viewer, if the page has one and nothing further up the stack is holding it.
///
/// Every export below goes through this rather than repeating the `VIEWER` dance, and every
/// borrow in it is a **`try_`** borrow. `panic = "abort"` is set for every release profile
/// including `web`, so a `RefCell` collision here is not a caught panic and a line in the
/// console — it is a dead tab with nothing on screen and nothing in the log. `None` means
/// "not now", which for a key press is exactly the right answer.
pub(crate) fn viewer() -> Option<Rc<RefCell<crate::Viewer>>> {
    crate::VIEWER.with(|slot| slot.try_borrow().ok().and_then(|held| held.clone()))
}

/// Turns editing on or off for this page.
///
/// ⚠ **This changes what a one-finger drag does.** With editing on, a drag that starts on an
/// item moves it and a drag on bare board sweeps a marquee — so on a mouse there is no pan
/// left except the wheel, and on a touchscreen the pan is the two-finger one. That is Miro's
/// arrangement and the desktop app's, and it is why this is a switch a person throws rather
/// than a default. It is also why the whole module is inert until it is thrown: see the
/// module header on edits being volatile until the push half is trusted.
///
/// Turning it off puts any preview back before dropping the gesture, so an item cannot be
/// left drawn where it is not.
#[wasm_bindgen]
pub fn velm_set_editing(on: bool) {
    let Some(held) = viewer() else { return };
    let Ok(mut viewer) = held.try_borrow_mut() else { return };
    let crate::Viewer { edit, projection, .. } = &mut *viewer;
    edit.cancel(projection);
    edit.set_enabled(on);
}

#[wasm_bindgen]
pub fn velm_editing() -> bool {
    viewer()
        .and_then(|held| held.try_borrow().ok().map(|viewer| viewer.edit.enabled()))
        .unwrap_or(false)
}

/// `⌘Z`. Answers whether anything was undone, so the page can say so.
#[wasm_bindgen]
pub fn velm_undo() -> bool {
    rewind(true)
}

/// `⌘⇧Z` / `⌘Y`.
#[wasm_bindgen]
pub fn velm_redo() -> bool {
    rewind(false)
}

/// The two are one function because the only difference is which `Board` method runs, and
/// the four things around it — refusing while editing is off, **cancelling a gesture
/// first**, rebuilding, and telling the push half — are identical and must not drift apart.
/// Feedback 35's rule applied before the fact: the sibling of a fix is where the next defect
/// is, so the two siblings are one body.
fn rewind(backwards: bool) -> bool {
    let Some(held) = viewer() else { return false };
    let Ok(mut viewer) = held.try_borrow_mut() else { return false };
    let crate::Viewer { edit, board, projection, push, .. } = &mut *viewer;
    if !edit.enabled() {
        return false;
    }
    // ⚠ Before the document is rewound. A live preview on top of an undone move draws the
    // item at a position neither the document nor the user asked for, and the release
    // afterwards would then write it there.
    edit.cancel(projection);
    let outcome = if backwards { undo(board, projection) } else { redo(board, projection) };
    match outcome {
        Ok(changed) => {
            if changed {
                announce(push, board);
            }
            changed
        }
        Err(error) => {
            log::error!("velm edit: {error}");
            false
        }
    }
}

/// `Backspace` / `Delete`. Answers how many items left the board.
#[wasm_bindgen]
pub fn velm_delete_selection() -> u32 {
    let Some(held) = viewer() else { return 0 };
    let Ok(mut viewer) = held.try_borrow_mut() else { return 0 };
    let crate::Viewer { edit, board, projection, push, .. } = &mut *viewer;
    if !edit.enabled() {
        return 0;
    }
    edit.cancel(projection);
    let doomed = edit.selection().to_vec();
    if doomed.is_empty() {
        return 0;
    }
    match delete(board, projection, &doomed) {
        Ok(count) => {
            // The ids are gone from the projection, so `rings` would prune them on the next
            // paint anyway. Cleared here as well because a selection that outlives its items
            // for even one frame draws rings around nothing — which is the exact symptom
            // feedback 39 spent two rounds chasing on the desktop.
            edit.clear();
            if count > 0 {
                announce(push, board);
            }
            u32::try_from(count).unwrap_or(u32::MAX)
        }
        Err(error) => {
            log::error!("velm edit: {error}");
            0
        }
    }
}

/// `⌘A`. Answers how many items are selected.
#[wasm_bindgen]
pub fn velm_select_all() -> u32 {
    let Some(held) = viewer() else { return 0 };
    let Ok(mut viewer) = held.try_borrow_mut() else { return 0 };
    let crate::Viewer { edit, projection, .. } = &mut *viewer;
    edit.select_all(projection);
    u32::try_from(edit.selection().len()).unwrap_or(u32::MAX)
}

/// What a right-click at a screen point should offer, having first made the selection right.
///
/// Answers `"selection"` or `"canvas"` — which of `tools.js`'s two row lists to draw — and
/// `""` when the viewer cannot be reached, where the page falls back to asking how many items
/// are selected. That fallback is honest and it is worse: it cannot select the item under the
/// pointer, so a right-click on an unselected sticky offers the canvas rows.
///
/// **This is a separate verb from `press` rather than a flag on it, because a menu is not a
/// click**, and the three rules that make it one are all about what it must *not* do:
///
/// - A right-click on an **unselected** item selects it first, so the rows act on the thing
///   under the pointer rather than on whatever was picked a minute ago.
/// - One **inside** an existing selection leaves it alone. Right-clicking one of five picked
///   items still offers all five — replacing the selection there is the single most annoying
///   thing a canvas application can do.
/// - One on **bare board** does not clear the selection. A request for a menu is not a
///   dismissal, and the desktop's `--demo context-menu` measures exactly this.
///
/// The first two are `EditState::press`'s own rules, reached by handing it `additive = false`
/// only when the hit is new; the third is why bare board is answered without touching the
/// selection at all. All three live here rather than in the page because only this side has
/// the scene — a hit test in JavaScript would be a second derivation of what is under the
/// pointer, and it would be the one that decides what a menu row acts on.
#[wasm_bindgen]
pub fn context_target(x: f64, y: f64) -> String {
    let Some(held) = viewer() else { return String::new() };
    let Ok(mut viewer) = held.try_borrow_mut() else { return String::new() };
    let ratio = crate::input::ratio();
    let at = viewer.camera.screen_to_world(vellum_scene::ScreenPoint::new(x * ratio, y * ratio));
    let crate::Viewer { edit, projection, .. } = &mut *viewer;
    let view: &Projection = projection;
    match view.scene().hit_test_where(at, |id| pickable(id, view)) {
        // Already picked, alone or among others: the rows act on everything held.
        Some(id) if edit.selection().contains(&id) => "selection".to_owned(),
        Some(id) => {
            edit.select_only(id);
            "selection".to_owned()
        }
        // ⚠ Deliberately no `edit.clear()` here. Bare board keeps whatever is selected; the
        // canvas rows — paste, select all — do not act on a selection, so leaving one held
        // costs nothing and clearing it would throw away a selection the user is looking at.
        None => "canvas".to_owned(),
    }
}

/// Escape, and the page's own "deselect" control.
#[wasm_bindgen]
pub fn velm_clear_selection() {
    let Some(held) = viewer() else { return };
    let Ok(mut viewer) = held.try_borrow_mut() else { return };
    let crate::Viewer { edit, projection, .. } = &mut *viewer;
    edit.cancel(projection);
    edit.clear();
}

#[wasm_bindgen]
pub fn velm_selection_count() -> u32 {
    let count = viewer()
        .and_then(|held| held.try_borrow().ok().map(|viewer| viewer.edit.selection().len()))
        .unwrap_or(0);
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Whether `⌘Z` and `⌘⇧Z` would do anything, for a page that greys its own buttons.
///
/// One string rather than two exports because they are read together, and a page that asks
/// for one and forgets the other draws a control that lies.
#[wasm_bindgen]
pub fn velm_history_state() -> String {
    let Some(held) = viewer() else { return "0 0".to_owned() };
    let Ok(viewer) = held.try_borrow() else { return "0 0".to_owned() };
    format!(
        "{} {}",
        u8::from(viewer.board.can_undo()),
        u8::from(viewer.board.can_redo())
    )
}

/// The edit layer, as a string, for a fixture to read.
///
/// The only honest check available in a crate where **no test is ever compiled**. It exists
/// for the reason `camera_report` exists: `input.rs` is driven by DOM events, so the only
/// way to verify a gesture is to dispatch real events at the real listeners and then ask
/// what happened — which needs a reader. Trap 9's lesson in a third place: a fixture that
/// calls the handler directly starts downstream of everything that can go wrong between the
/// browser and the handler, and stays green on a build where the listener was never
/// attached.
///
/// `editing selection dragging marquee items`, where `items` is the projection's item count
/// so a delete and an undo can each be measured against it. Space-separated, because a
/// fixture that splits on whitespace cannot be broken by a change of punctuation.
#[wasm_bindgen]
pub fn velm_edit_report() -> String {
    let Some(held) = viewer() else { return "0 0 0 0 0".to_owned() };
    let Ok(viewer) = held.try_borrow() else { return "0 0 0 0 0".to_owned() };
    format!(
        "{} {} {} {} {}",
        u8::from(viewer.edit.enabled()),
        viewer.edit.selection().len(),
        u8::from(viewer.edit.moved_offset().is_some()),
        u8::from(viewer.edit.marquee().is_some()),
        viewer.projection.len()
    )
}
