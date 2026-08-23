//! Typing on the canvas, in a browser: the caret, and the one undo group a session is.
//!
//! # What this is
//!
//! Double-click an item, a caret appears in its words, and typing changes the document.
//! It is the browser half of `vellum-app`'s on-canvas caret, and it is deliberately thin:
//! **every decision about a string and a cursor already exists**, tested, in
//! [`vellum_project::edit`] — the buffer, grapheme and word motion, and `splice_styling`,
//! which is what keeps a half-bold sticky half bold across an edit.
//!
//! ⚠ **That module was `vellum-app/src/edit.rs` and has to move for this file to compile.**
//! Its only code imports are `unicode_segmentation` and two `ItemId`s; the six crates it
//! names in full — `vellum_{doc,flow,mindmap,scene,table,text}` — are all already
//! `vellum-project` dependencies. It is pure, so it moves, and its ~100 tests move with it
//! and start covering wasm-relevant code. Writing a second buffer here instead is the
//! duplication this repository has paid for three times; it is not done.
//!
//! # The division of labour, and why it is drawn here
//!
//! - **`vellum_project::edit`** owns the string. No ids, no layout, no document.
//! - **This file** owns the *session*: which item, when the undo group opens and closes,
//!   what a DOM key means, and where the caret is on screen.
//! - **[`vellum_text::Layout`]** owns everything that needs the shaped glyphs —
//!   `caret`, `byte_at`, `selection_boxes`. All three are pure and all three already exist.
//!
//! Nothing here re-derives a layout. The caret is measured against **the layout the words
//! were actually drawn from**, handed over by `crate::text::TextLayer::placed`, which is
//! the rule `draw.rs` states for the desktop: a caret computed from a second measurement
//! is a caret that disagrees with the glyphs it sits between.
//!
//! # ⚠ The undo group — trap 11, and why it is the first thing to read
//!
//! **A session holds one Loro undo group open across its whole life**, so a typed word is
//! one `⌘Z` rather than one per letter. Loro's `group_start` answers
//! `UndoGroupAlreadyStarted` when a group is already open, `group_end` merely clears the
//! slot, there is no depth count, and **nothing else ever closes one** — so a leaked group
//! breaks *every* later grouped operation on the board for the life of the tab.
//! `CLAUDE.md` records this being re-found four times.
//!
//! The group is opened by [`flush`] on the first write and closed by [`settle`], and
//! `settle` is called from **every** way a session can end:
//!
//! | ending | where |
//! |---|---|
//! | Escape, Tab, Enter in a single-line field | [`key`] |
//! | a press anywhere else on the board | [`press`], and `crate::edit::pointer_down` |
//! | starting a second session | [`begin`], first line |
//! | editing switched off | `velm_set_editing(false)` |
//! | a delete, an undo, a redo | `velm_delete_selection`, `velm_undo`, `velm_redo` |
//! | the item refusing the write | [`flush`]'s failure arm |
//!
//! Six of those are calls the integrator has to make in files this one does not own; they
//! are listed in the hand-over notes. `settle` is idempotent and free with no session, so
//! an extra call costs nothing and a missing one costs the board.
//!
//! ⚠ **`⌘Z` is deliberately not claimed by the session** — the desktop draws the line at
//! ⌘/⌃ precisely so Undo keeps working mid-word — but `velm_undo` must `settle` *before*
//! `board.undo()`, or it rewinds into a group that is still open. That is the conservative
//! reading of trap 11 and it is the one taken here.
//!
//! # ⚠ Typing must not run the shortcut table
//!
//! `CLAUDE.md` feedback 40: on the desktop, letters armed tools and **Backspace deleted the
//! frame being renamed**. The rule that fixed it is [`claims`], and it is the one
//! derivation of the question: while a session is open the session owns every key **without
//! ⌘/⌃**, and with ⌘/⌃ it owns only Select-all, Copy, Cut and Paste. Undo and Redo pass
//! through on purpose.
//!
//! `claims` is exactly the set [`key`] acts on — `key` asks it first — so the two cannot
//! drift, and a key neither of them knows (F5, the browser's own reload) is left alone
//! rather than swallowed.
//!
//! # ⚠ A caret in an *empty* item has nothing to measure from
//!
//! `TextLayer::queue` returns early for empty text and greeks anything below
//! `MIN_DEVICE_FONT_SIZE`, so in both cases there is no layout and no origin. The desktop
//! shipped that bug and the report was *"it only starts flashing after i start typing"*.
//! [`fallback`] is the answer: the caret is placed from the *slot's* own geometry and the
//! same font-size ceiling the greek test uses, so an empty sticky gets a caret on the frame
//! the session opens.
//!
//! # No blink
//!
//! The caret is drawn solid. A blink needs a timer driving redraws while nothing is
//! happening, which is what this application exists not to do — and in a tab it would also
//! keep `requestAnimationFrame` alive on a board nobody is touching.
//!
//! # ⚠ `panic = "abort"`, and strings
//!
//! Nothing here slices a string by byte. Every offset handed to a buffer comes from
//! `vellum_project::edit`, whose motions are grapheme- and word-based, or from
//! `Layout::byte_at`, which returns cluster boundaries. Two UTF-8 panics have shipped in
//! this repository on real board titles (feedback 30), and on wasm a panic is a dead tab
//! with nothing on screen.

use vellum_doc::ItemKind;
use vellum_project::edit::{EditPart, Editing, Motion, TextBuffer};
use vellum_project::project::Projection;
use vellum_render::{DrawList, QuadInstance, Rgba};
use vellum_scene::{Camera, ItemId as SceneId, ScreenPoint, WorldPoint};
use vellum_text::Layout;
use wasm_bindgen::prelude::*;

/// The device-pixel width of the caret, and of nothing else.
///
/// The desktop's own number, and its reasoning transfers exactly: one logical pixel scaled
/// by the display, **not** by the camera. A caret that fattened with the zoom would read as
/// a highlighted column rather than as a caret, and one that thinned with it would vanish on
/// a fitted board — which is the same argument `SELECTION_RING_WIDTH` records from the other
/// end.
pub const CARET_WIDTH: f32 = 2.0;

/// How visible the selection wash is over the glyphs it covers.
///
/// ⚠ **Drawn *under* the glyphs**, which is what makes this legal at all: the desktop's
/// `push_selection_boxes` runs before the text and so does [`push_selection`], between
/// `list.use_view(screen)` and the text flush. A wash painted afterwards at any alpha high
/// enough to see is a wash you have to read the selected words through.
const SELECTION_ALPHA: f32 = 0.22;

/// Which text slot a session addresses.
///
/// ⚠ **Always zero on the web, where the desktop uses `BlockKey::SECONDARY` for a frame's
/// title.** This is a real divergence and it is deliberate: `lib.rs`'s item loop queues
/// *every* plain kind at slot `0`, frames included, so a session opened at the desktop's
/// slot would ask `TextLayer::placed` for a block that was never queued, get `None`, and
/// silently fall back to an unshaped caret on every frame title on the board. That is this
/// repository's signature failure — a fix applied at the sibling site and not at this one —
/// arriving before the fact rather than after it.
///
/// If `lib.rs` ever grows a second block for a plain kind, this becomes a function of the
/// kind and the two must move together.
const SLOT: u16 = 0;

/// The modifier state of one key press, as the DOM reports it.
///
/// Plain `bool`s rather than a `web_sys::KeyboardEvent`, and that is not laziness:
/// `KeyboardEvent` is **not** in this crate's `web-sys` feature list, so taking one would
/// mean a `Cargo.toml` change in a file this module does not own. Taking four bools means
/// the whole of this file compiles against nothing but the workspace's own crates, and the
/// page reads `event.key` and the four flags itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    /// ⌘ on macOS, the Windows key elsewhere.
    pub meta: bool,
    pub ctrl: bool,
    pub shift: bool,
    /// ⌥ on macOS. A *character* modifier there, which is why it does not count as a
    /// command below.
    pub alt: bool,
}

impl Mods {
    /// Whether this is a command chord rather than typing.
    ///
    /// ⚠ **⌥ is not in it.** On macOS `⌥e` is a character, so treating Alt as a command
    /// would make every accented letter unreachable inside a sticky while still swallowing
    /// the key. The desktop draws the same line and states the same reason.
    const fn command(self) -> bool {
        self.meta || self.ctrl
    }

    /// Whether this chord means "by word".
    ///
    /// ⌥ on macOS, ⌃ on Windows and Linux. Both are accepted rather than sniffing the
    /// platform: a browser has no honest way to ask, `navigator.platform` is deprecated and
    /// lies on an iPad, and the two chords do not collide with anything else here.
    const fn by_word(self) -> bool {
        self.alt || self.ctrl
    }
}

/// What [`key`] did with a press, and what the page should do about it.
///
/// Three answers rather than a `bool`, because the clipboard is genuinely a third case:
/// the browser has to be allowed to *finish* ⌘C, ⌘X and ⌘V — its own `copy`, `cut` and
/// `paste` events are how the data crosses, synchronously and with no permission prompt —
/// while the board's own bindings must still not see the chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// Not the session's. The page may run the board's own bindings.
    No,
    /// Acted on. The page should `preventDefault()`.
    Yes,
    /// The session's, but the **browser** completes it. Do *not* `preventDefault()`, and do
    /// not run the board's bindings either. See [`selection`], [`cut`] and [`insert`].
    Clipboard,
}

impl Handled {
    /// `0`/`1`/`2`, for the wasm boundary — one small integer beats three exports.
    pub const fn code(self) -> u32 {
        match self {
            Self::No => 0,
            Self::Yes => 1,
            Self::Clipboard => 2,
        }
    }
}

/// Where the caret and its selection were drawn, in **device pixels**.
///
/// Rebuilt every frame by [`measure`] and read by [`push_selection`], [`push_caret`] and
/// [`press`]. Keeping it rather than recomputing on demand is what lets a click be resolved
/// against the layout the words were actually drawn from: `TextLayer`'s per-frame `pending`
/// list is gone by the time a `pointerdown` arrives, and a second shaping would be a second
/// derivation of where the characters are.
#[derive(Debug, Clone)]
struct Drawn {
    /// Top-left of the caret quad.
    origin: [f32; 2],
    height: f32,
    /// One quad per visual line of the selection, `[x, y, w, h]`.
    selection: Vec<[f32; 4]>,
}

/// Work that needs the shaped layout, recorded now and resolved on the next [`measure`].
///
/// ⚠ **Deferring is the design, not a compromise.** A click and a vertical arrow both need
/// `Layout`, which lives inside `crate::text` and is only reachable while the frame loop
/// holds it — a `pointerdown` and a `keydown` arrive nowhere near that. Resolving them one
/// frame later costs 16ms and is invisible; shaping a second copy here to answer them
/// immediately would put two layouts of one block in a tab's memory and let them disagree
/// about where the characters are.
#[derive(Debug, Clone, Default)]
struct Pending {
    /// A click inside the edited item, in device pixels, and whether it extends.
    click: Option<([f32; 2], bool)>,
    /// Net visual-line motion asked for by Up/Down. Accumulated rather than queued, so
    /// holding a key down cannot grow an unbounded list between two frames.
    lines: i32,
    lines_extend: bool,
}

/// The session, if there is one, and where it was last drawn.
///
/// ⚠ **A field on `Viewer`, never a `thread_local`.** `edit.rs`'s header states the
/// principle for the layer next door — the edit state "holds no globals and reaches for
/// nothing, which is what keeps the whole of this module a value the viewer owns rather
/// than a second piece of page state that could disagree with it" — and a caret is the
/// piece of state that would be worst to have two of.
#[derive(Debug, Default)]
pub struct CaretState {
    session: Option<Editing>,
    drawn: Option<Drawn>,
    pending: Pending,
}

impl CaretState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a caret is on the board.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// The item being edited, for a caller that needs to leave it alone.
    #[must_use]
    pub fn scene(&self) -> Option<SceneId> {
        self.session.as_ref().map(|session| session.scene)
    }

    /// Forgets everything drawn. Called wherever a session ends, so a stale caret cannot
    /// outlive the words it was in for even one frame — the symptom feedback 39 spent two
    /// rounds chasing was a ring drawn around nothing.
    fn forget(&mut self) {
        self.drawn = None;
        self.pending = Pending::default();
    }
}

// ---------------------------------------------------------------------------------------
// Pure decisions. No `Viewer`, no `Board`, no DOM — the part that could be tested if this
// crate compiled a test, and the part to move to `vellum-project` if it ever needs to be.
// ---------------------------------------------------------------------------------------

/// Whether a session owns this key, so the board's own bindings must not see it.
///
/// **The one derivation of feedback 40's rule.** On the desktop, typing on a board ran the
/// shortcut table: `n` armed the sticky tool mid-word and Backspace deleted the frame being
/// renamed. The fix was to draw the line at ⌘/⌃ — everything without one is something
/// typing emits by definition, everything with one is deliberate — with four exceptions
/// the session claims back.
///
/// - **Select-all, Copy, Cut and Paste are the exceptions to the exception.** Inside a
///   caret they mean the selected *characters*, not the selected *items*. Without them
///   ⌘V pastes a whole new item on top of the one being typed into, which is a different
///   verb wearing the same chord.
/// - **Undo and Redo deliberately are not.** No session implements them, and taking `⌘Z`
///   away from someone mid-word is exactly the moment they most want it. This is the whole
///   reason the line is at ⌘ rather than at the table.
///
/// ⚠ **And the set is closed at both ends.** A key that is neither one printable character
/// nor one of the named keys below is *not* claimed — so `F5` still reloads the page and
/// `F12` still opens the inspector while a caret is up. An earlier shape of this returned
/// `true` for everything without ⌘, which would have swallowed the browser's own keys and
/// been reported as "the page is frozen".
///
/// [`key`] asks this first, so the two cannot come to disagree about what a session owns.
#[must_use]
pub fn claims(key: &str, mods: Mods) -> bool {
    if mods.command() {
        return matches!(key, "a" | "A" | "c" | "C" | "x" | "X" | "v" | "V")
            // ⚠ **Motion and deletion are claimed with a command modifier too, and leaving
            // them out was a bug in the first draft of this function.** The two platforms
            // spell the same two gestures with these very keys — `⌘←` is the start of the
            // line on macOS and `⌃←` is a word on Windows — so refusing them leaves line
            // motion unreachable on one and word motion unreachable on the other, while the
            // word-motion branch in [`key`] sits below as dead code that reads as working.
            //
            // `⌘⌫` is the one that matters most, and it is feedback 40 exactly: unclaimed, it
            // falls through to the board's own `Delete` and **removes the item being typed
            // into**. None of these is a board command, so claiming them costs nothing.
            || matches!(
                key,
                "ArrowLeft"
                    | "ArrowRight"
                    | "ArrowUp"
                    | "ArrowDown"
                    | "Home"
                    | "End"
                    | "Backspace"
                    | "Delete"
            );
    }
    is_typed(key)
        || matches!(
            key,
            "Escape"
                | "Enter"
                | "Tab"
                | "Backspace"
                | "Delete"
                | "ArrowLeft"
                | "ArrowRight"
                | "ArrowUp"
                | "ArrowDown"
                | "Home"
                | "End"
        )
}

/// Whether a DOM `KeyboardEvent.key` is one typed character.
///
/// The DOM's own convention: a printable key reports the character it produces and every
/// other key reports a name — `"Enter"`, `"ArrowLeft"`, `"Dead"`, `"Unidentified"`. So "one
/// `char` and not a control character" is exact rather than a heuristic, and it is why the
/// space bar (`" "`) types a space while `"Dead"` does not type four letters.
///
/// `chars()`, never bytes: `"é"` is one character and two bytes, and `"👍"` is one character
/// and four. Counting bytes here is the UTF-8 mistake this file's header refuses to make.
fn is_typed(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| !c.is_control()) && chars.next().is_none()
}

/// Where a caret goes when its block was never shaped, in device pixels.
///
/// ⚠ **This is the "caret in an empty sticky" fix, and it covers two cases, not one.**
/// `TextLayer::queue` answers `false` and queues nothing when the text is empty *and* when
/// the block is below `MIN_DEVICE_FONT_SIZE` and is greeked instead. Both leave
/// `TextLayer::placed` with nothing to hand over, and a caret with no origin is the bug the
/// desktop shipped: *"it only starts flashing after i start typing"*.
///
/// The size is the **same ceiling the greek test uses** — the style's own, or the box's
/// height over the line height — rather than a number invented here. An auto-fitted block
/// can never resolve to a size whose line height does not fit its box, so the ceiling is an
/// upper bound rather than a guess, and using the greek test's own expression is what stops
/// the caret being a different height from the bars beside it.
///
/// `origin` is the top-left of the *slot* in device pixels; `size` is the slot in **world**
/// units, which is the pair `TextLayer::queue` is given.
fn fallback(
    origin: [f32; 2],
    size: [f32; 2],
    font_size: Option<f32>,
    zoom: f32,
    anchor: crate::layout::Anchor,
) -> Drawn {
    let ceiling = font_size.unwrap_or((size[1] / crate::text::LINE_HEIGHT).max(1.0));
    let height = (ceiling * crate::text::LINE_HEIGHT * zoom).max(CARET_WIDTH);
    // An empty block lays out as one empty line, so its extent is zero wide and one line
    // tall. Centring against that is what puts the caret in the middle of an empty sticky
    // rather than in its top-left corner, which is where the words will appear.
    let at = match anchor {
        crate::layout::Anchor::TopLeft => origin,
        crate::layout::Anchor::Centred => [
            origin[0] + size[0] * zoom * 0.5,
            origin[1] + (size[1] * zoom - height).max(0.0) * 0.5,
        ],
    };
    Drawn { origin: at, height, selection: Vec::new() }
}

/// The offset one visual line up or down from `cursor`, against a shaped block.
///
/// Lifted from the desktop's `move_caret_by_line` rather than re-invented, including the
/// part that looks like an edge case and is not: **already on the first or last line, Up
/// goes to the very start and Down to the very end**, which is what a text field does. Doing
/// nothing there makes a one-line sticky feel as though the arrow keys are broken.
///
/// The target offset is taken by asking [`Layout::byte_at`] for the point at the *current*
/// caret's `x` on the target line's middle — so the column is preserved through lines of
/// different lengths, which is the whole reason this needs the layout at all.
fn line_target(layout: &Layout, text: &str, cursor: usize, delta: i32) -> usize {
    let caret = layout.caret(text, cursor);
    let last = layout.lines.len().saturating_sub(1);
    let target = caret
        .line
        .saturating_add_signed(delta as isize)
        .min(last);
    if target == caret.line {
        return if delta < 0 { 0 } else { text.len() };
    }
    let line = &layout.lines[target];
    layout.byte_at(text, caret.x, line.top + line.height * 0.5)
}

// ---------------------------------------------------------------------------------------
// The session's life. Every function here is a hop the integrator calls.
// ---------------------------------------------------------------------------------------

/// Starts a session on `scene`, ending whatever was running first.
///
/// `replacing` selects everything, which is what a freshly placed item wants; a
/// double-clicked one gets the cursor at the end, because "let me add to this" is far more
/// often what a double click means and `⌘A` is one chord away.
///
/// Answers whether a session started. `false` for an item with no words of its own, for a
/// **locked** one, and for one an ancestor frame has clipped out of the drawing — the same
/// three refusals `crate::edit::pickable` applies to a press, asked through that very
/// function so a caret cannot open on something a click cannot reach.
///
/// ⚠ **Refused outright while editing is off, and that is a posture rule rather than a
/// convenience.** `crate::edit`'s header states the contract: an edit made in a tab lives
/// only in that tab until the push half is trusted, so *"with editing off, this file changes
/// no behaviour at all … nothing can write to the document"*. A caret is a document write —
/// `flush` calls `Board::set_text` — so a session that could open behind the switch would be
/// the one hole in it. Gating `begin` is enough to close it: [`key`], [`insert`], [`press`]
/// and [`cut`] all require a session, and `velm_set_editing(false)` settles the one that
/// might exist.
///
/// ⚠ **[`settle`] on the first line, and it is not tidiness.** The assignment below
/// *overwrites* whatever session was there, and a session that has taken one keystroke owns
/// an open Loro undo group. Dropping it that way leaks the group, and trap 11 is what that
/// costs: every later grouped operation on the board fails for the life of the tab.
/// Committing first is also the right behaviour on its own — typing in one sticky and then
/// clicking into another is two undo steps, and the first one's words have to be written
/// before the second's session begins.
pub fn begin(viewer: &mut crate::Viewer, scene: SceneId, replacing: bool) -> bool {
    if !viewer.edit.enabled() {
        return false;
    }
    settle(viewer);

    let crate::Viewer { caret, projection, .. } = viewer;
    let view: &Projection = projection;
    if !crate::edit::pickable(scene, view) {
        return false;
    }
    let Some(projected) = view.get(scene) else { return false };
    // ⚠ Exactly the kinds `Board::set_text` accepts — `ItemKind::text()` answers `Some` for
    // Sticky, Text, Shape, Frame, Agent and AgentNote, and `set_text` matches the same six.
    // Checking here rather than discovering it at the first keystroke is what stops a
    // session opening on a table and failing with `WrongKind` at the end of a gesture the
    // user had already completed.
    let Some(styled) = projected.item.kind.text() else { return false };

    let doc = projected.doc_id;
    let plain = styled.to_plain();
    // The item's **styled** value, not just its words. This is what `splice_styling` splices
    // each keystroke into, and seeding it from the document is the whole of "formatting
    // survives an edit": `StyledText::plain(text)` here is what turned a half-bold sticky
    // into an all-regular one on the desktop, on the first keystroke.
    let styled = styled.clone();
    let session = if replacing {
        Editing::replacing(scene, doc, SLOT, plain)
    } else {
        Editing::new(scene, doc, SLOT, plain)
    };
    caret.session = Some(session.with_styling(styled));
    caret.forget();
    true
}

/// Ends the session and **closes its undo group**.
///
/// Nothing is written here: every keystroke already wrote. What this closes is the group,
/// which is the only thing that made the session one undo step rather than one per letter.
///
/// Idempotent, and free with no session — which is what makes it safe to call from all six
/// endings in the header's table. A missing call costs the board; a spare one costs a
/// branch.
pub fn settle(viewer: &mut crate::Viewer) {
    let crate::Viewer { caret, board, .. } = viewer;
    let Some(session) = caret.session.take() else {
        caret.forget();
        return;
    };
    if session.opened() {
        // `end_undo_group` clears the slot and cannot fail. It is also idempotent, which is
        // what makes every rescue in this file free.
        board.end_undo_group();
    }
    caret.forget();
}

/// Writes the live buffer to the document, opening the session's undo group on the first
/// write.
///
/// Every keystroke, for `vellum_project::edit`'s own reason: auto-fit derives a sticky's
/// font size from its text, and the search index, the projection and the painter all read
/// the document — so a buffer the document has not seen would be a second source of truth
/// for the item's words, and the board on screen would not be the board being typed into.
///
/// Answers whether the session survived. `false` means the item refused the write and the
/// session has been ended, group and all.
fn flush(viewer: &mut crate::Viewer) -> bool {
    let crate::Viewer { caret, board, projection, push, .. } = viewer;
    let Some(session) = caret.session.as_mut() else { return false };
    // Defensive, and it should be unreachable: `begin` only ever creates `EditPart::Item`.
    // A structured widget's fields live inside an opaque JSON token and `set_text` has
    // nowhere to put them, so a session on one would write to the wrong place rather than
    // fail — which is the failure worth a branch.
    if session.part != EditPart::Item {
        let opened = session.opened();
        caret.session = None;
        caret.forget();
        if opened {
            board.end_undo_group();
        }
        return false;
    }
    let doc = session.doc;

    if !session.opened() {
        if let Err(error) = board.begin_undo_group() {
            // ⚠ The one failure `begin_undo_group` has is *"there is already an active
            // group"*, which means one has leaked from somewhere. Closing it and taking the
            // next one is the rescue `crate::edit::grouped` performs for the same reason:
            // refusing here would leave a board on which nothing can be grouped again until
            // the tab is reloaded.
            log::warn!("velm caret: an undo group had leaked ({error}); closing it");
            board.end_undo_group();
            if let Err(error) = board.begin_undo_group() {
                log::error!("velm caret: this edit gets no undo step: {error}");
            }
        }
        // Marked whether or not the group opened, because this flag is also "have we
        // written yet" — and `settle` reads it to decide whether to close. Marking it only
        // on success would leak the group the *second* attempt opened.
        session.mark_opened();
    }

    // The keystroke spliced into the item's own runs rather than a flat replacement.
    let styled = session.styled_now();
    if let Err(error) = board.set_text(doc, styled) {
        // The item is gone — deleted from under the caret by an undo, or by a frame that
        // took its children. End the session rather than typing into nothing, and close the
        // group with it.
        log::warn!("velm caret: {doc} would not take the edit: {error}");
        let opened = session.opened();
        caret.session = None;
        caret.forget();
        if opened {
            board.end_undo_group();
        }
        crate::edit::resettle(board, projection);
        return false;
    }

    // ⚠ **`refresh_item`, not `rebuild`.** A keystroke changes one item's words and cannot
    // move anything, so the projection is patched and one item's generation is bumped —
    // which is what keeps the text cache from reshaping *every visible block* on the board
    // on every letter. That is the exact cost `Projected::generation`'s own doc records as
    // having made typing feel like it was "struggling so much to show me what i am typing".
    // The cheap path refuses when the bounds moved, and then the full rebuild is correct.
    match projection.refresh_item(board, doc) {
        Ok(true) => {}
        Ok(false) => crate::edit::resettle(board, projection),
        Err(error) => {
            log::debug!("velm caret: patching {doc}: {error}");
            crate::edit::resettle(board, projection);
        }
    }
    // ⚠ After the mutation and after the reprojection, never before — `Pusher::note_edit`'s
    // own documentation states it: `tick` exports the board as it stands, so announcing
    // first would send an empty delta and mark the real edit acknowledged.
    crate::edit::announce(push, board);
    true
}

/// One key press, as the DOM reports it.
///
/// `key` is `KeyboardEvent.key` verbatim. See [`Handled`] for what the page does with the
/// answer, and [`claims`] for the rule that decides whether the board's own bindings may
/// see the chord at all.
///
/// # The order of the arms is the behaviour
///
/// ⌘A is select-all rather than an `a`; ⌥← is a word rather than a character; ⌘← is the
/// start of the line rather than a word. Each is resolved by being tested before the arm it
/// would otherwise fall into, which is the same ordering the desktop's `type_key` uses.
pub fn key(viewer: &mut crate::Viewer, key: &str, mods: Mods) -> Handled {
    if viewer.caret.session.is_none() || !claims(key, mods) {
        return Handled::No;
    }

    // Whether Enter commits rather than inserting a break. A **frame's title** is drawn on
    // one line above the frame, so a break in it would be laid out and never seen; every
    // structured-widget field is one line by definition. Read before the buffer is borrowed
    // mutably, because it needs the projection.
    let single_line = viewer.caret.session.as_ref().is_some_and(|session| {
        session.part.is_single_line()
            || viewer
                .projection
                .get(session.scene)
                .is_some_and(|projected| matches!(projected.item.kind, ItemKind::Frame { .. }))
    });

    // Escape and Tab end the session, so they cannot be folded into the buffer arms below:
    // both need `settle`, which needs the whole viewer.
    match key {
        "Escape" | "Tab" => {
            settle(viewer);
            return Handled::Yes;
        }
        "Enter" if single_line => {
            settle(viewer);
            return Handled::Yes;
        }
        // ⚠ The browser finishes these three. Its `copy`, `cut` and `paste` events fire on
        // the focused element synchronously and with no permission prompt, which is the only
        // clipboard a wasm module can reach without `navigator.clipboard`'s async dance — and
        // `Clipboard` is not in this crate's `web-sys` feature list on purpose. The page
        // wires those events to `selection`, `cut` and `insert`. Claimed rather than passed
        // through, so the board's own ⌘V does not also paste an item on top of the one being
        // typed into.
        "c" | "C" | "x" | "X" | "v" | "V" if mods.command() => return Handled::Clipboard,
        // Up and Down need to know where the *visual* lines fall, which is a property of the
        // layout and not of the string -- `Motion` deliberately has no variant for them, and
        // says so. Recorded here and resolved on the next `measure`.
        //
        // Handled in *this* match, before the buffer is borrowed, because it writes to
        // `pending` rather than to the buffer: two mutable paths into one `CaretState` in one
        // statement is a thing that happens to compile rather than a thing that is clearly
        // right, and this feature is not worth spending that on.
        // `⌘↑`/`⌘↓` mean the start and the end of the whole block, which is a motion over
        // the *string* and is answered by the buffer below. Only the bare and shifted forms
        // are visual-line motion, so only they are deferred.
        "ArrowUp" | "ArrowDown" if !mods.meta => {
            let delta = if key == "ArrowUp" { -1 } else { 1 };
            let pending = &mut viewer.caret.pending;
            pending.lines = pending.lines.saturating_add(delta);
            pending.lines_extend = mods.shift;
            return Handled::Yes;
        }
        _ => {}
    }

    let extend = mods.shift;
    let mut changed = false;
    {
        let Some(session) = viewer.caret.session.as_mut() else { return Handled::No };
        let buffer: &mut TextBuffer = &mut session.buffer;
        match key {
            "Enter" => changed = buffer.insert("\n"),
            "Backspace" => changed = buffer.backspace(),
            "Delete" => changed = buffer.delete(),
            "ArrowLeft" => {
                // ⌘← is the start of the line on macOS and harmless elsewhere; ⌥←/⌃← is a
                // word. Tested in that order because ⌃ satisfies both.
                let motion = if mods.meta {
                    Motion::ParagraphStart
                } else if mods.by_word() {
                    Motion::WordLeft
                } else {
                    Motion::Left
                };
                buffer.move_cursor(motion, extend);
            }
            "ArrowRight" => {
                let motion = if mods.meta {
                    Motion::ParagraphEnd
                } else if mods.by_word() {
                    Motion::WordRight
                } else {
                    Motion::Right
                };
                buffer.move_cursor(motion, extend);
            }
            // Only reachable with ⌘ — the arm above took the rest.
            "ArrowUp" => buffer.move_cursor(Motion::DocumentStart, extend),
            "ArrowDown" => buffer.move_cursor(Motion::DocumentEnd, extend),
            "Home" => buffer.move_cursor(Motion::ParagraphStart, extend),
            "End" => buffer.move_cursor(Motion::ParagraphEnd, extend),
            "a" | "A" if mods.command() => buffer.select_all(),
            // Anything the browser turned into one character is text. Guarded on `command`,
            // so `⌘s` is still the page's Save rather than an `s` typed into the item, and on
            // `is_typed`, so a named key cannot arrive as a glyph.
            typed if is_typed(typed) && !mods.command() => changed = buffer.insert(typed),
            // Unreachable while `claims` and this match agree, which they are written to.
            _ => return Handled::No,
        }
    }

    if changed && !flush(viewer) {
        // The session ended inside the flush — the item refused the write. Still ours: the
        // key was consumed, and letting it fall through to the board's bindings now would
        // run a command with the same chord.
        return Handled::Yes;
    }
    Handled::Yes
}

/// Inserts text at the caret — a paste, or anything the page produced that is not a key.
///
/// Replaces the selection if there is one, exactly as typing does. Answers whether anything
/// was inserted.
///
/// ⚠ **Control characters are stripped except `\n`.** A paste carries whatever was on the
/// clipboard, and a `\r` or a `\t` shaped into a board's words is a glyph nobody asked for —
/// `\r\n` in particular would otherwise double every line break in text copied from a
/// Windows editor.
pub fn insert(viewer: &mut crate::Viewer, text: &str) -> bool {
    if viewer.caret.session.is_none() || text.is_empty() {
        return false;
    }
    let cleaned: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    if cleaned.is_empty() {
        return false;
    }
    let changed = viewer
        .caret
        .session
        .as_mut()
        .is_some_and(|session| session.buffer.insert(&cleaned));
    if changed {
        flush(viewer);
    }
    changed
}

/// The selected characters, for the page to put on the clipboard.
///
/// Empty with no selection, and **deliberately not the whole item**: ⌘C with nothing
/// selected copying every word in the sticky would be a surprise, and the ⌘X built on the
/// same rule would be a destructive one. The desktop refuses both for the same reason.
#[must_use]
pub fn selection(viewer: &crate::Viewer) -> String {
    let Some(session) = viewer.caret.session.as_ref() else { return String::new() };
    let range = session.buffer.selection();
    if range.is_empty() {
        return String::new();
    }
    // The range comes from the buffer, whose every motion is grapheme- or word-aligned, so
    // it is a character boundary by construction. `get` rather than an index anyway: under
    // `panic = "abort"` a slicing mistake here is a dead tab, and this is the one place a
    // range crosses a module boundary.
    session.buffer.text().get(range).unwrap_or_default().to_owned()
}

/// The selected characters, removed. `⌘X`.
///
/// Reads the selection *before* deleting it, so the page gets what it is about to lose.
pub fn cut(viewer: &mut crate::Viewer) -> String {
    let taken = selection(viewer);
    if taken.is_empty() {
        return taken;
    }
    let changed = viewer
        .caret
        .session
        .as_mut()
        .is_some_and(|session| session.buffer.backspace());
    if changed {
        flush(viewer);
    }
    taken
}

/// A press on the board while a caret is up.
///
/// Answers whether the press belonged to the session. `false` means it landed somewhere
/// else — **the session has been settled** and the caller must go on to handle the press
/// normally, because clicking away from a caret is how editing ends and it must not also be
/// swallowed.
///
/// ⚠ **The *item* is hit-tested, not the text block**, which is the desktop's rule and is
/// deliberately looser: the words rarely fill their box, and a click on the empty half of a
/// sticky should put the caret at the nearest character rather than end the session.
/// `Layout::byte_at` clamps in both axes, so "nearest" falls out for free.
///
/// The click is recorded in device pixels and resolved on the next [`measure`], because the
/// layout it has to be resolved against is not reachable from here. One frame, invisible.
pub fn press(viewer: &mut crate::Viewer, at: WorldPoint) -> bool {
    let Some(scene) = viewer.caret.scene() else { return false };
    if viewer.projection.scene().hit_test(at) != Some(scene) {
        settle(viewer);
        return false;
    }
    let screen = viewer.camera.world_to_screen(at);
    viewer.caret.pending.click = Some(([screen.x as f32, screen.y as f32], false));
    true
}

/// Whether a caret is on the board, for a caller that must not interrupt one.
///
/// ⚠ **`crate::live::merge` is the caller this exists for.** A poll landing mid-edit calls
/// `Projection::rebuild`, and whether `Board::apply` is safe *inside* an open local undo
/// group is not established anywhere in this repository — treat it as unknown. Feedback 34's
/// rule decides the shape regardless: *a command is something the user just asked for and
/// closing their edit to serve it is reasonable; a background fetch must not end an edit in
/// progress.* So the merge **waits**, and nothing is lost — `live` polls with the version it
/// already has, so the same updates arrive again the moment the caret closes.
#[must_use]
pub const fn busy(viewer: &crate::Viewer) -> bool {
    viewer.caret.is_open()
}

// ---------------------------------------------------------------------------------------
// What the painter needs. Three calls, and their order in `frame()` is load-bearing.
// ---------------------------------------------------------------------------------------

/// Works out where the caret and its selection go this frame, and resolves anything that
/// was waiting for a layout.
///
/// ⚠ **Call this after the item loop and *before* `TextLayer::flush`.** The anchor-adjusted
/// origin a block was drawn at lives only in that layer's per-frame `pending` list, and
/// `flush` takes it. Called afterwards, `placed` answers `None` for every block on the
/// board and every caret in the application falls back to the unshaped path — which looks
/// almost right, which is what makes it worth a warning.
pub fn measure(
    caret: &mut CaretState,
    text: &crate::text::TextLayer,
    projection: &Projection,
    camera: &Camera,
) {
    let Some(scene) = caret.scene() else {
        caret.drawn = None;
        return;
    };
    // An item deleted from under the caret. Nothing is drawn this frame; the next keystroke
    // fails its write and `flush` ends the session properly, group and all. Ending it here
    // would need the board, which the painter does not hand over.
    let Some(projected) = projection.get(scene) else {
        caret.drawn = None;
        return;
    };
    let theme = &vellum_project::theme::Theme::LIGHT;
    // The same function the item loop asked, so the caret is measured against the box the
    // words were set in rather than against the item's own rectangle. A sticky's words are
    // inset by Miro's 8%; a frame's name sits *above* the frame.
    let Some(slot) = crate::layout::text_slot(projected, theme.text, theme.text_muted) else {
        caret.drawn = None;
        return;
    };

    let zoom = camera.zoom() as f32;
    let corner = camera.world_to_screen(slot.rect.min);
    let origin = [corner.x as f32, corner.y as f32];
    let size = [slot.rect.width() as f32, slot.rect.height() as f32];

    let Some((layout, placed, drawn_zoom)) = text.placed(scene, SLOT) else {
        // Empty, or greeked. A pending click cannot be resolved against a block that was
        // never shaped, so it is dropped rather than guessed at — a caret that jumped to
        // offset zero on every click at a fitted zoom would be worse than one that did not
        // move.
        caret.pending = Pending::default();
        caret.drawn = Some(fallback(origin, size, slot.font_size, zoom, slot.anchor));
        return;
    };

    // Taken out of `pending` before the session is borrowed, so the resolution below reads
    // plain locals. `pending` is emptied whether or not it can be honoured -- a request that
    // survived the frame it was made for would be applied against a different layout.
    let click = caret.pending.click.take();
    let lines = std::mem::replace(&mut caret.pending.lines, 0);
    let lines_extend = caret.pending.lines_extend;

    // One scope, one mutable borrow of the session, and everything it produces is a value.
    let (spot, selection) = {
        let Some(session) = caret.session.as_mut() else {
            caret.drawn = None;
            return;
        };
        // Resolve the deferred work **before** the caret is measured, so this frame draws the
        // answer rather than the frame after it.
        if click.is_some() || lines != 0 {
            let text_now = session.buffer.text().to_owned();
            if let Some((point, extend)) = click {
                // Block-relative and unscaled: `push_layout` places a layout at `placed` and
                // magnifies it by `drawn_zoom`, so undoing exactly those two is what turns a
                // device-pixel point back into the coordinates the layout is expressed in.
                let local = [
                    (point[0] - placed[0]) / drawn_zoom,
                    (point[1] - placed[1]) / drawn_zoom,
                ];
                let byte = layout.byte_at(&text_now, local[0], local[1]);
                session.buffer.place(byte, extend);
            }
            if lines != 0 {
                let byte = line_target(layout, &text_now, session.buffer.cursor(), lines);
                session.buffer.place(byte, lines_extend);
            }
        }

        let text_now = session.buffer.text();
        let spot = layout.caret(text_now, session.buffer.cursor());
        let range = session.buffer.selection();
        let selection: Vec<[f32; 4]> = if range.is_empty() {
            Vec::new()
        } else {
            layout
                .selection_boxes(text_now, range)
                .into_iter()
                .map(|sel| {
                    [
                        placed[0] + sel.x * drawn_zoom,
                        placed[1] + sel.top * drawn_zoom,
                        (sel.width * drawn_zoom).max(1.0),
                        (sel.height * drawn_zoom).max(1.0),
                    ]
                })
                .collect()
        };
        (spot, selection)
    };

    caret.drawn = Some(Drawn {
        origin: [placed[0] + spot.x * drawn_zoom, placed[1] + spot.top * drawn_zoom],
        height: (spot.height * drawn_zoom).max(CARET_WIDTH),
        selection,
    });
}

/// The selection highlight, one quad per visual line.
///
/// ⚠ **Under the glyphs**, so it must be pushed *before* `TextLayer::flush` — push order is
/// paint order. A wash over the words at any alpha you can see is a wash you then have to
/// read the selected text through, and the desktop draws it underneath for the same reason.
///
/// In the **screen** view, where the glyphs are.
pub fn push_selection(caret: &CaretState, list: &mut DrawList, accent: Rgba) {
    let Some(drawn) = caret.drawn.as_ref() else { return };
    let wash = accent.with_alpha(SELECTION_ALPHA);
    for quad in &drawn.selection {
        list.push_quad(QuadInstance::solid([quad[0], quad[1]], [quad[2], quad[3]], wash));
    }
}

/// The caret itself.
///
/// After the glyphs, so a caret between two characters is not hidden by the one to its
/// right. In the **screen** view, and sized in device pixels — see [`CARET_WIDTH`].
pub fn push_caret(caret: &CaretState, list: &mut DrawList, accent: Rgba) {
    let Some(drawn) = caret.drawn.as_ref() else { return };
    list.push_quad(QuadInstance::solid(
        drawn.origin,
        [CARET_WIDTH, drawn.height.max(CARET_WIDTH)],
        accent,
    ));
}

// ---------------------------------------------------------------------------------------
// The exports the page calls.
//
// Prefixed `velm_caret_`, and every one of them takes and returns a primitive: this file
// must not need a `web-sys` feature that `Cargo.toml` does not already have, because
// `Cargo.toml` belongs to somebody else this round.
// ---------------------------------------------------------------------------------------

/// The viewer, if the page has one and nothing further up the stack is holding it.
///
/// `crate::edit::viewer`'s twin argument applies unchanged: `panic = "abort"` is set for
/// every release profile, so a `RefCell` collision is not a caught panic and a line in the
/// console — it is a dead tab. Every borrow below is a `try_` borrow, and `None` means "not
/// now", which for a key press is exactly the right answer.
fn with<T>(default: T, f: impl FnOnce(&mut crate::Viewer) -> T) -> T {
    let Some(held) = crate::edit::viewer() else { return default };
    let Ok(mut viewer) = held.try_borrow_mut() else { return default };
    f(&mut viewer)
}

/// Opens a caret on the item under a point, in **CSS** pixels.
///
/// The page's `dblclick` handler. CSS rather than device pixels because that is what a DOM
/// event carries, and the conversion is the same `client_x * devicePixelRatio` every handler
/// in `input.rs` performs — kept identical on purpose, since a caret that opened on the item
/// beside the one you double-clicked would be indistinguishable from the feature not working.
///
/// ⚠ **Only a mouse produces `dblclick`.** A double *tap* on a touchscreen does not, so on an
/// iPad there is no gesture that reaches this today. That is a real gap and it is named in
/// the hand-over notes rather than papered over.
#[wasm_bindgen]
pub fn velm_caret_open_at(x: f64, y: f64) -> bool {
    let ratio = web_sys::window()
        .map_or(1.0, |window| window.device_pixel_ratio())
        .max(1.0);
    with(false, |viewer| {
        let at = viewer
            .camera
            .screen_to_world(ScreenPoint::new(x * ratio, y * ratio));
        // ⚠ `hit_test_where`, never `hit_test` then a filter. `scene.rs` states the rule and
        // `crate::edit::press` repeats it: the predicate is applied *before* "topmost", so a
        // locked sticky lying over a frame stays transparent instead of becoming a hole that
        // no caret can open through. Filtered afterwards, double-clicking one would answer
        // "nothing to edit" rather than opening the frame's title beneath it.
        // Scoped, so the shared borrow of the projection is provably over before `settle`
        // and `begin` take the viewer mutably below.
        let hit = {
            let view: &Projection = &viewer.projection;
            view.scene()
                .hit_test_where(at, |id| crate::edit::pickable(id, view))
        };
        let Some(scene) = hit else {
            // A double click on bare board still ends whatever was being typed.
            settle(viewer);
            return false;
        };
        if !begin(viewer, scene, false) {
            return false;
        }
        // Aim the caret at the character that was double-clicked rather than dropping it at
        // the end of the words. Resolved on the next `measure`, like every other click.
        let screen = viewer.camera.world_to_screen(at);
        viewer.caret.pending.click = Some(([screen.x as f32, screen.y as f32], false));
        true
    })
}

/// Whether a caret is on the board.
#[wasm_bindgen]
pub fn velm_caret_open() -> bool {
    with(false, |viewer| viewer.caret.is_open())
}

/// One key press. Answers `0` (not ours), `1` (handled — `preventDefault`) or `2` (the
/// browser must finish it — do **not** `preventDefault`). See [`Handled`].
#[wasm_bindgen]
pub fn velm_caret_key(key: &str, meta: bool, ctrl: bool, shift: bool, alt: bool) -> u32 {
    let mods = Mods { meta, ctrl, shift, alt };
    with(0, |viewer| self::key(viewer, key, mods).code())
}

/// Whether a session would own this chord, without a session having to exist.
///
/// For a page that guards a **window-level capture** handler: it has to decide whether to
/// let a key through before anything else sees it, and asking two questions — is a caret up,
/// and does it own this key — is what stops the board's own bindings firing on the letters
/// of a word. See [`claims`] for the rule and for what it deliberately does not claim.
#[wasm_bindgen]
pub fn velm_caret_claims(key: &str, meta: bool, ctrl: bool) -> bool {
    claims(key, Mods { meta, ctrl, shift: false, alt: false })
}

/// Inserts text at the caret. The page's `paste` handler.
#[wasm_bindgen]
pub fn velm_caret_insert(text: &str) -> bool {
    with(false, |viewer| insert(viewer, text))
}

/// The selected characters. The page's `copy` handler.
#[wasm_bindgen]
pub fn velm_caret_selection() -> String {
    with(String::new(), |viewer| selection(viewer))
}

/// The selected characters, removed. The page's `cut` handler.
#[wasm_bindgen]
pub fn velm_caret_cut() -> String {
    with(String::new(), |viewer| cut(viewer))
}

/// Ends the session and closes its undo group.
///
/// The page calls this when the canvas loses focus, when a tool is armed, and when the tab
/// is hidden — the three ways a gesture ends without an Escape. Feedback 27's rule: *give
/// every way a gesture can end without a release a call to the function that closes it.*
#[wasm_bindgen]
pub fn velm_caret_commit() {
    with((), settle);
}

/// The caret, as a string, for a fixture to read.
///
/// **The only honest check available in a crate where no test is ever compiled.** It exists
/// for the reason `velm_edit_report` and `camera_report` exist: the whole feature is driven
/// by DOM events, so the only way to verify it is to dispatch real events at the real
/// listeners and then ask what happened — and trap 9's lesson is that a fixture calling the
/// handler directly starts downstream of everything that can go wrong between the browser
/// and the handler.
///
/// `open item cursor anchor chars group drawn`, space-separated so a fixture that splits on
/// whitespace cannot be broken by a change of punctuation. `group` is whether the session's
/// undo group is open, which is the one piece of state trap 11 is about and the one a
/// screenshot can never show.
#[wasm_bindgen]
pub fn velm_caret_report() -> String {
    with("0 - 0 0 0 0 0".to_owned(), |viewer| {
        let caret = &viewer.caret;
        let Some(session) = caret.session.as_ref() else {
            return format!("0 - 0 0 0 0 {}", u8::from(caret.drawn.is_some()));
        };
        format!(
            "1 {} {} {} {} {} {}",
            session.doc,
            session.buffer.cursor(),
            session.buffer.anchor(),
            session.buffer.text().chars().count(),
            u8::from(session.opened()),
            u8::from(caret.drawn.is_some()),
        )
    })
}
