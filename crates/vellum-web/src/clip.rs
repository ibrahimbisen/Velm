//! Cut, copy, duplicate and paste — the browser's clipboard.
//!
//! # What this is
//!
//! The four verbs `web/tools.js` already draws in its right-click menu and has been drawing
//! **disabled**, each tooltipped with the export it could not find. This is those exports.
//! Modelled on `vellum_app::actions`'s `copy`/`paste_internal`/`duplicate_selection` line for
//! line, because that is the copy that has been through the user's hands and every rule here
//! that looks arbitrary is a rule that was paid for there.
//!
//! # ⚠ The clipboard decision: in-memory, in this tab, and nothing else
//!
//! **This module never touches `navigator.clipboard`, in either direction.** A copy puts
//! `vellum_doc::Item`s into a thread-local and a paste takes them back out. You can copy on
//! this board and paste on this board. You cannot paste into another tab, another browser, or
//! another application, and you cannot paste *from* one. That is a real limitation and it is
//! stated here, in the export's own doc comment, and in the report the fixture reads — it is
//! not hidden behind a control that looks like it should work.
//!
//! Three reasons, and the first is the one that decides it:
//!
//! - **Reading needs a permission the user did not ask for.** `navigator.clipboard.readText()`
//!   requires transient activation and is permission-gated: Chrome raises a *"Allow this site
//!   to see text you copied?"* prompt, and Safari overlays its own **Paste** button that has
//!   to be pressed *again*. A menu row that raises a browser permission dialog is the shape
//!   `CLAUDE.md` names its worst failure — *"never describe a gesture the user cannot
//!   perform"* — arriving as a gesture the user did not agree to perform. A rejected promise
//!   is silent, so the honest degradation is worse than the honest refusal.
//! - **It is `async`, and every export around it is not.** `tools.js` calls
//!   `mod[fn]()` from a click handler and discards the answer; `style_selection`,
//!   `transform_selection` and `velm_delete_selection` all answer synchronously. A paste that
//!   resolved a promise would finish some frames after the click, after the menu had closed,
//!   with nothing on screen to say it was in flight.
//! - **The text flavour cannot carry a board, and that exact bug is already in this
//!   repository's history.** The system clipboard can hold a string; a board is items, with
//!   parents, connector endpoints, styles and geometry. `CLAUDE.md` feedback 17 is the desktop
//!   pasting a cross-board copy as **one grey text blob** for precisely this reason. Writing a
//!   custom MIME type would need `ClipboardItem` + `write()`, which is permission-gated again,
//!   refused for non-standard types by Firefox outright, and needs three more `web-sys`
//!   features in `Cargo.toml`.
//!
//! **Writing plain text out was considered and rejected too**, even though it needs no
//! permission in most browsers. It would let a copied sticky's words be pasted into a mail
//! client — a genuine convenience — and it buys nothing here: nothing in this tab reads it
//! back (the items are what a paste uses), a failed `writeText` is a rejected promise nobody
//! sees, and it costs a `Clipboard` feature in `Cargo.toml`. The desktop needs it because
//! `arboard` is how it talks to other applications; a tab that is only ever pasting into
//! itself does not.
//!
//! What that costs, precisely: **the clipboard dies with the tab.** A reload empties it. That
//! is the same lifetime `EditState`'s selection already has and the same one an undo history
//! has, so it is not a new class of loss — and unlike the board itself, nothing on the
//! clipboard was ever the only copy of anything.
//!
//! # ⚠ The two traps a paste has to survive
//!
//! **An `ItemId` is a Loro `TreeID` — a peer and a counter — so an id copied from one document
//! can collide with an *unrelated* item in another.** Every structural reference is therefore
//! stripped on the way in ([`cut_loose`], and `NewItem` simply has no id to carry) and put
//! back through an old-to-new map on a second pass ([`rebound`]). Nothing intermediate ever
//! names an id that is not in the document. Carried across verbatim, a pasted connector binds
//! itself to a stranger and nothing reports it.
//!
//! **A copy takes a container's descendants with it.** A frame's children are separate items
//! that merely name it as their parent, so copying a selection of *just* a frame used to
//! produce an empty box. [`Board::descendants`] answers parents-before-children, which is
//! exactly what lets [`place`] create and reparent in one forward pass with no sort.
//!
//! # ⚠ Where a paste lands
//!
//! At the pointer, never at a flat offset from the coordinates it was copied from. On one
//! board a flat `+24` reads as a nudge, which is why it survived on the desktop for so long;
//! it is only when the source coordinates are somewhere else entirely that the paste lands off
//! screen and reads as having done nothing at all. [`shift_onto`] is the desktop's, verbatim.
//!
//! Nothing in the browser tracks a hovering pointer — `input.rs`'s move handler returns early
//! while no contact is down, deliberately, so a hovering Pencil cannot invent a gesture. So
//! the aim is **told** to this module by the page, at the moment the menu opens, through
//! [`velm_note_paste_aim`]. That is strictly better than a remembered pointer: the menu is
//! opened at a known point and the paste lands under the row that was clicked. With nothing
//! noted at all it falls back to the centre of the viewport, which is on screen, visible, and
//! never the source board's coordinates.
//!
//! # ⚠ One undo group, and whose chokepoint
//!
//! A paste is one `⌘Z` — the creates, the reparents and the connector rebinds together. It
//! goes through [`crate::style::grouped`] rather than a copy of it, because trap 11 has been
//! re-found three times and a second derivation of the begin/end pairing is exactly how it
//! comes back. That function ends the group and reprojects on **every** path, including the
//! failing one, so a `?` inside the closure cannot leak a group.
//!
//! ⚠ One difference between this crate's two chokepoints is worth knowing before debugging a
//! *"paste does nothing"* report: `style::grouped` returns an error **without closing** a group
//! it found already open — deliberately, since closing somebody else's group commits their
//! unfinished gesture — while `edit::grouped` closes it. Nothing in the browser holds a group
//! open across calls today (there are no text sessions here), so no path can reach that state.
//! If one is ever added, paste would fail persistently where delete self-heals.
//!
//! # Nothing here can be tested
//!
//! `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so **no test in this crate is ever
//! compiled**. `cargo test --workspace` will report this file as covered by saying nothing
//! about it. [`velm_clip_report`] is the honest check that is available, for the same reason
//! `velm_edit_report` and `camera_report` exist: a fixture drives the real listeners and then
//! asks what happened. The three pure decisions — [`shift_onto`], [`cut_loose`] and
//! [`rebound`] — are free functions over plain data precisely so they can be moved to
//! `vellum-project`, where the tests run, exactly as `edit.rs`'s header recommends for its own.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use wasm_bindgen::prelude::*;

use vellum_doc::{Board, Item, ItemId as DocId, ItemKind, NewItem, Placement};
use vellum_project::project::Projection;
use vellum_scene::{Camera, ItemId as SceneId, ScreenPoint, WorldPoint};

/// How far a duplicate lands from its original, in **world** units.
///
/// `vellum_app::actions::OFFSET`'s value, so a board duplicated in a tab and the same board
/// duplicated on the desktop put the copy in the same place.
///
/// ⚠ At a fitted 4% this is one device pixel, so the copy is drawn almost exactly on top of
/// the original and the offset is *not* what tells you the gesture worked — **the selection
/// is**. `duplicate_selection` leaves the new items selected, so the rings move to the copy.
/// That is why [`crate::edit::EditState::select`] is a hard requirement of this module rather
/// than a nicety: without it a duplicate and a no-op look identical.
const DUPLICATE_OFFSET: f64 = 24.0;

/// The most items one copy will hold.
///
/// ⚠ **A judgement, not a measurement, and it refuses rather than truncating.** Two costs sit
/// behind it and both are worse in a tab than on a desktop: wasm linear memory **never returns
/// to the operating system**, so the peak of one enormous copy becomes this tab's footprint
/// for the rest of its life (feedback 45 found two leaks of exactly that shape in this crate);
/// and `Board::add` is O(n²) in `vellum-doc`'s Loro insert, so the *paste* is where a huge
/// clipboard is actually paid for.
///
/// The reference board is **1,306 items**, so this is about four times the largest board this
/// port has ever been pointed at. A copy over the cap is **refused whole**, with the reason
/// logged: a truncated copy that pastes back a fraction of what was selected is the shape
/// `CLAUDE.md` records as the worst kind of failure — a partial result reported as a complete
/// one.
const MAX_CLIPBOARD_ITEMS: usize = 5_000;

thread_local! {
    /// The clipboard. See the module header for why it is here and not on the pasteboard.
    ///
    /// A thread-local rather than a field on `Viewer`, and that is deliberate rather than
    /// lazy: the clipboard must survive things the viewer's own state does not. `live.rs`
    /// rebuilds the projection on its own timer and `EditState::set_enabled(false)` drops the
    /// selection — neither should empty what you copied a moment ago. wasm is single-threaded
    /// and `VIEWER` is a thread-local too, so this is the same storage class the module it
    /// sits beside already uses.
    static CLIPBOARD: RefCell<Vec<Item>> = const { RefCell::new(Vec::new()) };

    /// Where the next paste aims, in **client CSS pixels**, or `None` for the viewport centre.
    ///
    /// ⚠ **Screen, not world, and it must stay screen.** "At the pointer" is a statement about
    /// the window, not about the board: a world point noted before a pan would paste where the
    /// board *used* to be under the pointer, which is the off-screen paste this module exists
    /// to prevent, arriving by the opposite route.
    ///
    /// ⚠ **CSS pixels, converted at use.** Trap 4 — every `clientX` off a DOM event is a CSS
    /// pixel and every coordinate the camera takes is a physical one. Converting here and
    /// again at the camera is a drift of exactly the device ratio, and *exactly correct* on a
    /// 1x monitor, which is the machine most likely to be testing it.
    static AIM: Cell<Option<(f64, f64)>> = const { Cell::new(None) };
}

// ---------------------------------------------------------------------------------------
// Pure decisions. No `Board`, no `Projection`, no `web_sys` — ready to move to
// `vellum-project`, where they can be tested. See the module header.
// ---------------------------------------------------------------------------------------

/// How far a copied group must move so that its centre lands on `at`.
///
/// `vellum_app::actions::shift_onto`, verbatim, including the part that looks like an
/// oversight and is not.
///
/// **The whole group moves by one offset** rather than each item to the pointer, or a copied
/// diagram would collapse into a stack at a single point.
///
/// ⚠ **Sizes are deliberately not considered.** `Placement`'s `x`/`y` are the item's *centre*,
/// so the midpoint of the centres is the group's centre for this purpose. Folding in widths
/// and heights sounds more correct and is worse: one 38,000-unit frame in the selection drags
/// the computed centre far away from where the items actually are, and the paste lands off
/// screen — the exact symptom this function exists to prevent.
fn shift_onto(items: &[Item], at: WorldPoint) -> (f64, f64) {
    let Some(first) = items.first() else { return (0.0, 0.0) };
    let (mut min_x, mut min_y) = (first.placement.x, first.placement.y);
    let (mut max_x, mut max_y) = (min_x, min_y);
    for item in items {
        min_x = min_x.min(item.placement.x);
        min_y = min_y.min(item.placement.y);
        max_x = max_x.max(item.placement.x);
        max_y = max_y.max(item.placement.y);
    }
    (at.x - (min_x + max_x) / 2.0, at.y - (min_y + max_y) / 2.0)
}

/// Strips a connector's endpoint targets, leaving its anchors and arrowheads alone.
///
/// Pass one of a paste writes this, so that no item ever enters the document naming an id that
/// does not exist in it. [`rebound`] puts the surviving targets back.
///
/// Matched with `..` rather than by listing the variant's fields, so a connector growing
/// another one does not silently drop it here.
fn cut_loose(mut kind: ItemKind) -> ItemKind {
    if let ItemKind::Connector { start, end, .. } = &mut kind {
        start.target = None;
        end.target = None;
    }
    kind
}

/// Re-points a connector's endpoints at the copies of what they were attached to.
///
/// ⚠ An endpoint whose target was **not** part of the copy is left free rather than bound back
/// to the original, and that is wrong in both directions if you do it: across documents the id
/// belongs to a different board — and because an `ItemId` is a Loro `TreeID`, it can happily
/// match an unrelated item there — while on the same board it ties the copy to the original's
/// geometry, so moving one drags the other's connector with it.
///
/// This also covers an item whose *creation failed*: [`place`] records only what the document
/// actually accepted, so a failed add is absent from `remap` and its dependants degrade to
/// exactly the "was not part of the copy" case. That is the reason a failed add does not have
/// to abort the paste.
fn rebound(mut kind: ItemKind, remap: &HashMap<DocId, DocId>) -> ItemKind {
    if let ItemKind::Connector { start, end, .. } = &mut kind {
        for endpoint in [start, end] {
            endpoint.target = endpoint.target.and_then(|old| remap.get(&old).copied());
        }
    }
    kind
}

// ---------------------------------------------------------------------------------------
// Document operations.
// ---------------------------------------------------------------------------------------

/// The selected items, plus everything parented to them however deeply, read out of the board.
///
/// ⚠ **The descendants are the point.** A frame's children are separate items that merely name
/// it as their parent, so gathering only what was selected copies a frame as an empty box —
/// the defect `CLAUDE.md` feedback 17 fixed on the desktop, and one this crate would otherwise
/// have inherited whole.
///
/// **Locked items are gathered**, unlike a delete. A lock means *"I did not mean to drag
/// this"*; it cannot mean *"and it may not be copied"*, because a copy writes nothing to the
/// item it reads. The copy carries the lock with it, since [`place`] clones the style verbatim
/// — so pasting a locked item gives a locked copy, which is the desktop's behaviour and is
/// faithful rather than convenient.
///
/// Read out of the **document** rather than the projection, because these are about to be
/// written back and the document is the authority at write time — `style::targets`' rule. It
/// also means a live drag preview cannot be copied: `Projection::moved` puts the preview in
/// the projection and never in the board.
///
/// Order is the selection's, each root immediately followed by its own subtree, deduplicated —
/// so selecting a frame *and* a sticky on it copies the sticky once.
fn gather(board: &Board, projection: &Projection, selection: &[SceneId]) -> Vec<Item> {
    let mut wanted: Vec<DocId> = Vec::with_capacity(selection.len());
    let mut seen: HashSet<DocId> = HashSet::with_capacity(selection.len());

    // Two passes over the selection: every selected id is claimed first, so that a child
    // selected alongside its own parent is not pulled forward out of the parent's subtree.
    // `Board::descendants` answers parents-before-children and `place` relies on that order.
    let roots: Vec<DocId> = selection
        .iter()
        .filter_map(|id| projection.get(*id))
        .map(|projected| projected.doc_id)
        .collect();
    seen.extend(roots.iter().copied());

    for root in &roots {
        wanted.push(*root);
        for child in board.descendants(*root) {
            if seen.insert(child) {
                wanted.push(child);
            }
        }
    }

    wanted.iter().filter_map(|id| board.item(*id).ok()).collect()
}

/// Creates `items` on the board, offset by `(dx, dy)`, in **two passes**.
///
/// Answers `(index into items, the new id)` for everything the document accepted, which is
/// what lets a caller select the result without assuming the two lists line up.
///
/// # ⚠ Why two passes, and why nothing is written twice
///
/// An item's `parent` and a connector's endpoints are `ItemId`s, and the ids of the items being
/// created **do not exist until they are created**. Pass one strips both, so no intermediate
/// state ever names an id that is not in the document; pass two puts them back through the
/// old-to-new map.
///
/// Stripping is not belt-and-braces. An `ItemId` is a Loro `TreeID` — a peer and a counter — so
/// an id copied from one board can *collide with an unrelated item* on another. Carried across
/// verbatim, a pasted connector silently binds itself to a stranger, and `NewItem` has no
/// parent field set by `new` anyway, which is what used to make a pasted group fall apart.
///
/// # ⚠ Why a failure never aborts
///
/// Every write here logs and continues rather than escaping with `?`, and the result is not
/// merely tolerable but semantically clean: an item the document refused is absent from
/// `remap`, so its children lose their parent and connectors bound to it come loose — which is
/// [`rebound`]'s documented "was not part of the copy" behaviour, reached by a second route.
/// Aborting would leave the same items on the board with nobody holding their ids, so nothing
/// could select them and nothing could say which ones arrived.
///
/// The caller supplies the undo group; this function opens none, which is what keeps trap 11's
/// begin/end pairing in exactly one place.
fn place(board: &mut Board, items: &[Item], dx: f64, dy: f64) -> Vec<(usize, DocId)> {
    let mut created: Vec<(usize, DocId)> = Vec::with_capacity(items.len());
    let mut remap: HashMap<DocId, DocId> = HashMap::with_capacity(items.len());

    for (index, item) in items.iter().enumerate() {
        let placement = Placement { x: item.placement.x + dx, y: item.placement.y + dy, ..item.placement };
        let fresh = NewItem::new(cut_loose(item.kind.clone()), placement).with_style(item.style.clone());
        match board.add(fresh) {
            Ok(id) => {
                remap.insert(item.id, id);
                created.push((index, id));
            }
            Err(error) => log::warn!("velm clip: creating a copy of {}: {error}", item.id),
        }
    }

    for (index, new_id) in &created {
        let Some(item) = items.get(*index) else { continue };
        // A parent that was not itself copied stays dropped: on another board it would not
        // exist, and on this one re-parenting into the original's frame puts the copy
        // somewhere the user cannot see it moved to.
        if let Some(parent) = item.parent.and_then(|old| remap.get(&old)) {
            if let Err(error) = board.reparent(*new_id, Some(*parent)) {
                log::debug!("velm clip: refiling {new_id}: {error}");
            }
        }
        if matches!(item.kind, ItemKind::Connector { .. }) {
            if let Err(error) = board.set_kind(*new_id, rebound(item.kind.clone(), &remap)) {
                log::debug!("velm clip: rebinding {new_id}: {error}");
            }
        }
    }

    created
}

/// Where a paste lands, in world units.
///
/// The noted aim if there is one, otherwise the centre of the viewport.
///
/// ⚠ **A non-finite aim is refused here as well as at the door.** `finite` in `style.rs`
/// records what one costs: a `NaN` reaching a `Placement` poisons the R-tree's bounds,
/// `Projection::content_bounds` and therefore every later fit — a board that cannot be looked
/// at again, from one bad number. [`velm_note_paste_aim`] guards its input and this guards its
/// output, because the two are separated by an arbitrary amount of time and by a JS caller.
///
/// The viewport is already in **physical** pixels — it is set from the canvas — so the centre
/// takes no ratio conversion, while a noted aim is CSS pixels and takes one.
fn aim_world(camera: &Camera) -> WorldPoint {
    let viewport = camera.viewport();
    let centre = ScreenPoint::new(viewport.width / 2.0, viewport.height / 2.0);
    let screen = match AIM.with(|aim| aim.get()) {
        Some((x, y)) if x.is_finite() && y.is_finite() => {
            let ratio = crate::input::ratio();
            ScreenPoint::new(x * ratio, y * ratio)
        }
        _ => centre,
    };
    let world = camera.screen_to_world(screen);
    if world.x.is_finite() && world.y.is_finite() {
        world
    } else {
        // A camera in an impossible state is not this module's to repair, and a paste at the
        // origin is recoverable where a poisoned R-tree is not.
        camera.screen_to_world(centre)
    }
}

/// The clipboard's contents, cloned out.
///
/// ⚠ Cloned, and the borrow dropped, **before** anything touches the board. Nothing in a paste
/// re-enters this module today, but `panic = "abort"` means a `RefCell` collision here is not
/// a caught panic and a line in the console — it is a dead tab with the board still on screen
/// and nothing in the log.
fn clipboard_items() -> Vec<Item> {
    CLIPBOARD.with(|held| held.try_borrow().map(|items| items.clone()).unwrap_or_default())
}

// ---------------------------------------------------------------------------------------
// The exports the page calls.
//
// ⚠ **Named without the `velm_` prefix, unlike `edit.rs`'s.** That module prefixes because
// three of its exports would otherwise collide with its own `undo`, `redo` and `delete`, and
// because `delete` is a reserved word in JavaScript. None of these four collides with
// anything, and `web/tools.js` names each of them **first** in its `EXPORTS` table — which is
// the name a tooltip prints when nothing matches, so it is the name to write.
//
// ⚠ **Every one takes the viewer through `with_viewer` and answers rather than panicking.**
// These are called from DOM listeners.
//
// ⚠ **Every one that changes the document ends with `crate::after_edit`** — the one place an
// edit is finished. It re-syncs the selection against the new projection and pushes to the
// server on the same frame rather than leaving it to the 250ms timer, which is the difference
// between "every edit pushes immediately" being a property of the design and of the build.
// ---------------------------------------------------------------------------------------

/// Copies the selection, and everything parented to it, into this tab's clipboard.
///
/// Answers how many items were taken — the *whole* count, so copying a frame with nine
/// stickies on it answers 10 and the number matches what a paste will produce.
///
/// # Not gated on editing being on, and it needs no gate
///
/// A copy writes nothing. It also cannot fire when editing is off, because
/// `EditState::set_enabled(false)` clears the selection and `press` refuses to build a new
/// one — so it answers 0 by arithmetic rather than by a check that would have to be kept in
/// step with one. The three verbs below that *do* write are gated explicitly.
#[wasm_bindgen]
pub fn copy_selection() -> u32 {
    crate::with_viewer(0, |viewer| {
        let selection = viewer.edit.selection().to_vec();
        if selection.is_empty() {
            return 0;
        }
        let items = gather(&viewer.board, &viewer.projection, &selection);
        if items.is_empty() {
            log::info!("velm clip: nothing in the selection could be read out of the board");
            return 0;
        }
        // Refused whole rather than truncated. See `MAX_CLIPBOARD_ITEMS`: half a copy that
        // reports success is worse than a copy that says it will not run.
        if items.len() > MAX_CLIPBOARD_ITEMS {
            log::warn!(
                "velm clip: refusing to copy {} items — this build holds at most {MAX_CLIPBOARD_ITEMS}, \
                 and a partial copy that reported success would be worse",
                items.len()
            );
            return 0;
        }
        let count = items.len();
        CLIPBOARD.with(|held| {
            if let Ok(mut slot) = held.try_borrow_mut() {
                *slot = items;
            }
        });
        u32::try_from(count).unwrap_or(u32::MAX)
    })
}

/// Copies the selection and then removes it. Answers how many items **left the board**.
///
/// ⚠ **The two halves can disagree, and the answer is the delete's.** A locked item is copied
/// and is *not* deleted — `edit::delete` enforces the lock where the write happens, which is
/// the only place that covers `⌘A` over a board with one locked item on it. So cutting a
/// locked item puts it on the clipboard and leaves it on the board, which is a **copy**, and
/// answering with the copied count would call that a cut. The desktop arrived at the same
/// split for the same reason and states it in as many words.
///
/// The copy runs either way, which is right: the items are on the clipboard and the board is
/// untouched, so nothing has been lost — only the verb was not the one that was asked for.
#[wasm_bindgen]
pub fn cut_selection() -> u32 {
    let copied = copy_selection();
    if copied == 0 {
        return 0;
    }
    crate::with_viewer(0, |viewer| {
        if !viewer.edit.enabled() {
            log::info!("velm clip: this board is not editable, so the copy stands and nothing was cut");
            return 0;
        }
        let crate::Viewer { edit, board, projection, .. } = &mut *viewer;
        // A gesture still in flight has a preview in the projection. Cancelling puts it back
        // before the document is rewritten under it — `velm_delete_selection`'s rule, and
        // feedback 27's: give every way a gesture can end without a release a call to the
        // function that closes it.
        edit.cancel(projection);
        let doomed = edit.selection().to_vec();
        let removed = match crate::edit::delete(board, projection, &doomed) {
            Ok(count) => count,
            Err(error) => {
                log::error!("velm clip: cutting: {error}");
                return 0;
            }
        };
        // Cleared even though `after_edit` would prune it: a selection that outlives its items
        // for even one frame draws rings around nothing, which is the symptom feedback 39 spent
        // two rounds chasing on the desktop.
        edit.clear();
        if removed > 0 {
            crate::after_edit(viewer);
        }
        u32::try_from(removed).unwrap_or(u32::MAX)
    })
}

/// Copies the selection in place, offset by [`DUPLICATE_OFFSET`], and selects the copies.
///
/// Answers how many items were created.
///
/// ⚠ **The clipboard is deliberately not touched.** Duplicating is not copying: someone who
/// copied a diagram, duplicated a sticky and then pasted would otherwise get the sticky.
///
/// # ⚠ This is deliberately not what `vellum_app::actions::duplicate_selection` does
///
/// The desktop's takes neither the descendants nor the remap — so duplicating a frame there
/// yields an empty box, and duplicating two connected shapes yields two shapes whose new
/// connector still points at the originals. Both are the defects `CLAUDE.md` feedback 17 fixed
/// for *copy* and never applied to the sibling beside it, which is feedback 35's rule exactly:
/// when a fix lands, check its siblings. Reproducing it here to match would be copying a bug on
/// purpose; sharing [`gather`] and [`place`] with the paste path means there is one derivation
/// and the question cannot arise again.
#[wasm_bindgen]
pub fn duplicate_selection() -> u32 {
    crate::with_viewer(0, |viewer| {
        if !viewer.edit.enabled() {
            log::info!("velm clip: this board is not editable, so nothing was duplicated");
            return 0;
        }
        let selection = viewer.edit.selection().to_vec();
        if selection.is_empty() {
            return 0;
        }
        {
            let crate::Viewer { edit, projection, .. } = &mut *viewer;
            edit.cancel(projection);
        }
        let items = gather(&viewer.board, &viewer.projection, &selection);
        if items.is_empty() {
            return 0;
        }
        write_copies(viewer, &items, DUPLICATE_OFFSET, DUPLICATE_OFFSET)
    })
}

/// Pastes this tab's clipboard, centred on the noted aim, and selects what arrives.
///
/// Answers how many items were created.
///
/// **Takes no arguments**, which is what `web/tools.js` calls it with today: its `ROWS` table
/// runs a verb as `mod[fn]()` or `mod[fn](arg)` with exactly one argument, so a point cannot
/// come through that door. It comes through [`velm_note_paste_aim`] instead, which the page
/// calls when it opens the menu.
///
/// ⚠ **This is the tab's own clipboard and not the system one** — see the module header. A
/// paste with nothing copied *in this tab* answers 0 and says so in the log; it does not reach
/// for `navigator.clipboard`, and it does not raise a permission prompt from a menu row.
#[wasm_bindgen]
pub fn paste() -> u32 {
    let items = clipboard_items();
    if items.is_empty() {
        log::info!(
            "velm clip: nothing to paste — this tab's clipboard is empty, and it is the tab's own \
             rather than the system's, so a copy made elsewhere is not visible here"
        );
        return 0;
    }
    crate::with_viewer(0, |viewer| {
        if !viewer.edit.enabled() {
            log::info!("velm clip: this board is not editable, so nothing was pasted");
            return 0;
        }
        {
            let crate::Viewer { edit, projection, .. } = &mut *viewer;
            edit.cancel(projection);
        }
        let at = aim_world(&viewer.camera);
        let (dx, dy) = shift_onto(&items, at);
        write_copies(viewer, &items, dx, dy)
    })
}

/// The body [`paste`] and [`duplicate_selection`] share: one undo group, then the selection.
///
/// One function rather than two bodies because the four things around the write — the group,
/// the reprojection, selecting the result and pushing — are identical and must not drift
/// apart. Feedback 35's rule applied before the fact.
fn write_copies(viewer: &mut crate::Viewer, items: &[Item], dx: f64, dy: f64) -> u32 {
    let created = {
        let crate::Viewer { board, projection, .. } = &mut *viewer;
        // ⚠ `crate::style::grouped`, not a fourth copy of the begin/end pairing. It closes the
        // group and reprojects on every path including the failing one, so the `?`-shaped leak
        // of trap 11 is unrepresentable here rather than recovered from.
        match crate::style::grouped(board, projection, |board| Ok(place(board, items, dx, dy))) {
            Ok(created) => created,
            Err(why) => {
                log::error!("velm clip: {why}");
                return 0;
            }
        }
    };
    if created.is_empty() {
        return 0;
    }

    // ⚠ Doc ids to scene ids, and only **after** the reprojection inside `grouped` — an item
    // created a moment ago has no scene id until the projection has interned one, so asking
    // any earlier answers `None` for every single copy and the paste appears to select nothing.
    let count = created.len();
    let picked: Vec<SceneId> = {
        let crate::Viewer { projection, .. } = &*viewer;
        created.iter().filter_map(|(_, doc)| projection.scene_id(*doc)).collect()
    };
    {
        let crate::Viewer { edit, projection, .. } = &mut *viewer;
        edit.select(picked, projection);
    }

    crate::after_edit(viewer);
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Tells this module where the next paste should land, in **client CSS pixels**.
///
/// The page calls it as it opens the context menu, so a paste lands under the row that was
/// clicked. That is where `web/tools.js` already knows the point: `openMenu(x, y)` is reached
/// from the `contextmenu` event, from the touchscreen long press, and from the `⋮` on the
/// selection bar — and in all three the menu is where the user is looking.
///
/// ⚠ **Non-finite values are refused rather than stored.** A bare `f64` parameter receives
/// JavaScript's `undefined` as `NaN`, so `velm_note_paste_aim()` called with no arguments —
/// which is one typo away from every correct call — would otherwise store a `NaN` that reaches
/// a `Placement` and poisons the R-tree's bounds and every later fit. `style.rs`'s `finite`
/// records what that costs: a board that cannot be looked at again, from one empty field.
///
/// Refusing **leaves the previous aim in place** rather than clearing it. A bad call should not
/// be able to move a paste that was already aimed correctly.
#[wasm_bindgen]
pub fn velm_note_paste_aim(x: f64, y: f64) {
    if !x.is_finite() || !y.is_finite() {
        log::warn!("velm clip: ignoring a paste aim that is not a pair of numbers");
        return;
    }
    AIM.with(|aim| aim.set(Some((x, y))));
}

/// The clipboard, as a string, for a fixture to read.
///
/// The only honest check available in a crate where **no test is ever compiled**, and it exists
/// for the reason `velm_edit_report` and `camera_report` do: a fixture dispatches real events at
/// the real listeners and then asks what happened. Trap 9's lesson — a fixture that calls the
/// handler directly starts downstream of everything that can go wrong between the browser and
/// the handler, and stays green on a build where nothing was ever wired.
///
/// `held aimed x y`, where `held` is how many items are on the clipboard, `aimed` is whether
/// the page has told this module where to paste, and `x y` is the **world point the next paste
/// would land on**.
///
/// ⚠ That last pair is the number worth having and it is otherwise invisible. A paste that
/// lands off screen looks exactly like a paste that did nothing, which is how the desktop's
/// went unnoticed through three reports; a fixture that reads this can assert the landing point
/// is inside the board before a single item is created. Space-separated, because a fixture that
/// splits on whitespace cannot be broken by a change of punctuation.
#[wasm_bindgen]
pub fn velm_clip_report() -> String {
    let held = CLIPBOARD.with(|slot| slot.try_borrow().map_or(0, |items| items.len()));
    let aimed = u8::from(AIM.with(|aim| aim.get()).is_some());
    crate::with_viewer(format!("{held} {aimed} 0 0"), |viewer| {
        let at = aim_world(&viewer.camera);
        format!("{held} {aimed} {:.3} {:.3}", at.x, at.y)
    })
}
