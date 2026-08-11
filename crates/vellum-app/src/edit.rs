//! On-canvas text editing: the buffer, the cursor, and what a keystroke does to them.
//!
//! Until now an item's words were edited in the properties panel — double-clicking put
//! the cursor in a plain `TextEdit` off to the side, which is a text field that happens
//! to be near a board rather than editing on a board. `CLAUDE.md`'s known-defects list
//! has carried that entry since the panel was built.
//!
//! This module is the half of the fix that can be tested: a buffer, a cursor, a selection
//! anchor, and the pure transitions between them. Everything it needs to know about
//! *shape* — which visual line an offset is on, where a click lands — is a question for
//! [`vellum_text::Layout`], which answers it from the same layout the glyphs were drawn
//! from. Nothing here measures or draws.
//!
//! # Why the document is written on every keystroke
//!
//! It would be cheaper to hold the string here and write once at the end. It would also
//! mean the canvas draws from a buffer the document does not have, which breaks auto-fit
//! (a sticky's font size is derived from its text), the search index, the properties
//! panel, and the thumbnail — each of which reads the document. So a keystroke writes
//! through, and the *whole session* is one undo group, exactly as the eraser's sweep is
//! one group across many dabs. `⌘Z` after typing a word puts back the word.
//!
//! # Honest limits
//!
//! - **Formatting survives an edit; it cannot be *changed* by one.** The buffer is plain and
//!   the styling is re-applied around each keystroke by [`splice_styling`], so a half-bold
//!   sticky stays half bold and typing at the end of a bold word continues in bold. What is
//!   still missing is the other half of rich-text editing: there is no way to select a range
//!   and *make* it bold from the canvas. The panel's weight control is still reported as
//!   unsupported — `vellum_doc::Style` has no weight field, because Miro carries weight in
//!   the spans, which is exactly where this module now writes it. That is the next step and
//!   it is a different one: this preserves runs, it does not create them.
//!   (A **structured widget's** fields are plain strings in their own models, so their
//!   sessions have no runs to preserve — see [`EditPart`].)
//! - **IME is wired but unverified.** `winit`'s `Ime` events are consumed now —
//!   [`Editing::set_composition`] writes the composing text through to the document as it
//!   changes, and `crate::actions::ime_allowed` turns the window's input method on exactly
//!   while a caret is on the board. What is *measured* is the half that could regress: with
//!   IME allowed, real keystrokes driven through the window still arrive as
//!   `KeyEvent::text` and insert **once** — `key Character("Z") text Some("Z") … ime true`,
//!   and the board reads `CaZQ` rather than `CaZZQQ`. What is **not** measured is composition
//!   itself: a US layout on macOS produces no `Ime` events at all, so the path is exercised
//!   only by an input method this project has no way to drive unattended. Treat a report of
//!   "composing does not work" as untested rather than as working.
//!   The composing text is also drawn like ordinary text, without the underline a native
//!   field puts under it — see [`Editing::preedit`].
//! - **No blink.** The caret is drawn solid. A blink needs a timer driving redraws while
//!   nothing is happening, which is the opposite of what this app is for.
//! - **LTR only**, inherited from [`vellum_text::Layout::caret`] and stated there.

use unicode_segmentation::UnicodeSegmentation;
use vellum_doc::ItemId as DocId;
use vellum_scene::ItemId as SceneId;

/// Which way a cursor motion goes.
///
/// `Up` and `Down` are deliberately absent: they depend on where the *visual* lines fall,
/// which is a property of the layout and not of the string. The caller resolves them
/// against [`vellum_text::Layout`] and applies the result with [`Editing::place`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// One grapheme cluster left. Grapheme, not `char`: a flag or a family emoji is one
    /// key press to the user, and `char`-wise movement would take four.
    Left,
    /// One grapheme cluster right.
    Right,
    /// One word left, by Unicode word boundaries.
    WordLeft,
    WordRight,
    /// Start of the paragraph the cursor is in — `Home`. Not the start of the visual
    /// line, which would need the layout; see the note on this enum.
    ParagraphStart,
    ParagraphEnd,
    DocumentStart,
    DocumentEnd,
}

/// Carries a styled value across one plain-text edit.
///
/// This is what stops a session from flattening formatting. The buffer is a plain `String`
/// and always will be — a caret's arithmetic is about graphemes and words, not about runs —
/// so the styling has to be re-applied to the new string afterwards. Writing
/// `StyledText::plain(text)` instead is what used to happen, and it turned a half-bold
/// sticky into an all-regular one the first time it was typed into.
///
/// # Why a splice and not a diff
///
/// `next` differs from `current`'s plain text by exactly **one contiguous change**, and that
/// is a property of the caller rather than an assumption: every mutation `TextBuffer` offers
/// is one splice at the cursor — insert (which replaces the selection, if any), backspace,
/// delete — and `crate::actions` flushes after each one. So the change is found by taking the
/// longest common prefix and the longest common suffix, and everything between them is what
/// moved. A general diff would be slower, harder to reason about, and no more correct here.
///
/// Two edits arriving between flushes still produce a *legal* answer — the minimal splice
/// covering both — rather than a wrong one; the middle would take one style where it should
/// have taken two.
///
/// # Which style new text takes
///
/// The run holding the character **before** the insertion point, which is what every text
/// editor does: typing at the end of a bold word continues in bold. At the very start there
/// is no preceding character, so the first run's style is used — the alternative, plain,
/// would make typing at the head of a bold paragraph produce one stray regular word.
///
/// Block structure is carried over and re-truncated by `with_blocks`, so an edit that removes
/// a line drops the block style that belonged to it rather than shifting every later one up.
pub fn splice_styling(current: &vellum_doc::StyledText, next: &str) -> vellum_doc::StyledText {
    use vellum_doc::{StyledText, TextSpan};

    let previous = current.to_plain();
    if previous == next {
        return current.clone();
    }

    // In **characters**, because a span's length is measured in characters everywhere in
    // `vellum-doc` — `char_len` is what Loro indexes text by — and because slicing a string
    // by a byte offset found this way could split a grapheme.
    let prev: Vec<char> = previous.chars().collect();
    let now: Vec<char> = next.chars().collect();
    let head = prev.iter().zip(&now).take_while(|(a, b)| a == b).count();
    // The suffix cannot reach back past the prefix, or a repeated character would be counted
    // twice and the splice would be shorter than the change: "aa" -> "a" has a common prefix
    // of one and a common suffix of one, and the two overlap.
    let tail = prev
        .iter()
        .rev()
        .zip(now.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(prev.len() - head)
        .min(now.len() - head);

    let inserted: String = now[head..now.len() - tail].iter().collect();
    let removed = head..prev.len() - tail;

    // The style the inserted text inherits, resolved before the spans are rebuilt.
    let style_at = |index: usize| {
        let mut seen = 0usize;
        for span in current.spans() {
            let len = span.text.chars().count();
            if index < seen + len {
                return span.style.clone();
            }
            seen += len;
        }
        current.spans().last().map_or_else(Default::default, |span| span.style.clone())
    };
    let inherited = if head == 0 { style_at(0) } else { style_at(head - 1) };

    // Rebuild through a flat (character, style) list rather than by walking runs and
    // splitting them: a splice can start inside one run and end inside another, and the
    // arithmetic for that written per-run is where this went wrong the first time.
    // `from_spans` folds the neighbours that agree back into single runs, so this costs one
    // pass and no accuracy.
    let mut chars: Vec<(char, vellum_doc::SpanStyle)> =
        Vec::with_capacity(now.len().max(prev.len()));
    for span in current.spans() {
        for c in span.text.chars() {
            chars.push((c, span.style.clone()));
        }
    }
    chars.drain(removed);
    for (offset, c) in inserted.chars().enumerate() {
        chars.insert(head + offset, (c, inherited.clone()));
    }

    let mut spans: Vec<TextSpan> = Vec::new();
    for (c, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push(c),
            _ => spans.push(TextSpan::new(c.to_string(), style)),
        }
    }
    StyledText::from_spans(spans).with_blocks(current.blocks().to_vec())
}

/// Which piece of an item a session is typing into.
///
/// Most items have one place for words and this is [`EditPart::Item`], written through
/// `vellum_doc::Board::set_text`. The four structured widgets have many, and none of them
/// is reachable that way: their whole model — every cell, every card, every node — is one
/// opaque JSON token on the item, so "type into this cell" means *decode the token, change
/// one field, re-encode*. Naming the piece is what lets one caret serve both.
///
/// The widget ids are carried as their own types rather than as raw numbers because
/// `vellum-flow` and `vellum-mindmap` deliberately do not expose constructors for them —
/// an id is a handle its container hands out and never reuses, which is exactly the
/// property that makes a stale one safe. A session that outlives the card it was editing
/// therefore fails to find it and stops, rather than silently editing whichever card took
/// its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPart {
    /// The item's own words: a sticky, a text item, a shape's label, a frame's title.
    Item,
    /// One cell of a table, by its anchor — the top-left of a merge, which is the
    /// coordinate every `vellum-table` API expects back.
    TableCell(vellum_table::CellRef),
    /// A kanban board's own title, in the strip across its top.
    KanbanTitle,
    /// A kanban column's header.
    KanbanColumn(vellum_flow::ColumnId),
    /// A kanban card's label.
    KanbanCard(vellum_flow::CardId),
    /// A mind-map node's text.
    MindMapNode(vellum_mindmap::NodeId),
}

impl EditPart {
    /// Whether this part is one line by definition, so `Enter` commits rather than
    /// inserting a break.
    ///
    /// Every widget part is: a table cell wraps but a hard break in one is not something a
    /// grid has a use for, and a column header or a mind-map node with two paragraphs in it
    /// is a layout nobody asked for. The item's own words are the case that varies, and the
    /// caller decides that from the kind — a frame's title is one line, a sticky's note is
    /// not.
    pub const fn is_single_line(self) -> bool {
        !matches!(self, Self::Item)
    }
}

/// An editing session: which item's which slot, and the buffer.
///
/// The ids are separated from the buffer on purpose. Everything interesting about editing
/// text is a property of the string and the cursor, and none of it needs to know what item
/// it belongs to — so [`TextBuffer`] holds the logic and the tests, and this holds the two
/// identities and the one bit of session bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editing {
    /// The item being edited, in both namespaces: the scene id addresses the projection
    /// and the painter's caches, the document id addresses the board.
    pub scene: SceneId,
    pub doc: DocId,
    /// Which text slot — `BlockKey::PRIMARY` for a sticky, a text item or a shape's
    /// label; `BlockKey::SECONDARY` for a frame's title; `CELL_SLOT_BASE + index` for one
    /// piece of a structured widget, where the index is the painter's own.
    ///
    /// **The slot is the painter's coordinate and [`Editing::part`] is the document's.**
    /// They are two answers to different questions and both are needed: the caret is drawn
    /// against the block the slot names, and the keystroke is written into the field the
    /// part names. Deriving either from the other would mean re-implementing one of the two
    /// mappings a second time.
    pub slot: u16,
    /// Which piece of the item the words belong to.
    pub part: EditPart,
    pub buffer: TextBuffer,
    /// The item's styled value, kept in step with the buffer.
    ///
    /// This is what preserves formatting across an edit. The buffer is plain; each write
    /// splices the keystroke into *this* — see [`splice_styling`] — and this is what goes to
    /// the document, so a half-bold sticky stays half bold. Writing `StyledText::plain`
    /// instead is what used to happen, and it flattened the item on the first keystroke.
    ///
    /// Meaningful only for [`EditPart::Item`]. A structured widget's fields are plain strings
    /// in their own model, so their sessions leave this at its default and never read it.
    pub styled: vellum_doc::StyledText,
    /// Whether the document has been written to yet, which is also whether an undo group
    /// has been opened. The two are the same question and must not become two.
    opened: bool,
    /// The byte range of the buffer currently holding **uncommitted IME composition**.
    ///
    /// `None` when nothing is being composed, which is every keystroke of a Latin layout.
    ///
    /// The composing text is written *through* to the document like any other edit, rather
    /// than held aside and overlaid: the canvas shapes text from the document, so composition
    /// held anywhere else would be invisible until it was committed — and typing Japanese
    /// into a sticky that shows nothing until you press Enter is not editing on the board.
    /// The cost is that the composing text is drawn like ordinary text, without the underline
    /// a native field puts under it. That is stated in the module header as the honest limit;
    /// drawing the underline needs the preedit's *range* to reach the painter, which is the
    /// next step rather than a missing part of this one.
    preedit: Option<std::ops::Range<usize>>,
}

impl Editing {
    /// Begins a session with the cursor at the end and nothing selected.
    ///
    /// At the end rather than selecting everything: a double click on a sticky that
    /// already says something means "let me add to this" far more often than "let me
    /// replace it", and replacing is one `⌘A` away.
    pub fn new(scene: SceneId, doc: DocId, slot: u16, text: String) -> Self {
        Self {
            scene,
            doc,
            slot,
            part: EditPart::Item,
            styled: vellum_doc::StyledText::plain(text.clone()),
            buffer: TextBuffer::at_end(text),
            opened: false,
            preedit: None,
        }
    }

    /// Begins a session with everything selected, so the first keystroke replaces it.
    ///
    /// What a freshly *placed* item wants: it says "Frame" or nothing at all, and the
    /// user is about to say what it really is.
    pub fn replacing(scene: SceneId, doc: DocId, slot: u16, text: String) -> Self {
        Self {
            scene,
            doc,
            slot,
            part: EditPart::Item,
            styled: vellum_doc::StyledText::plain(text.clone()),
            buffer: TextBuffer::all_selected(text),
            opened: false,
            preedit: None,
        }
    }

    /// Aims the session at one piece of a structured widget instead of the item's words.
    ///
    /// A builder rather than a fourth constructor: the two above already differ only in
    /// where the cursor starts, and a part is orthogonal to that.
    pub fn in_part(mut self, part: EditPart) -> Self {
        self.part = part;
        self
    }

    /// Seeds the session with the item's real styled value, rather than the plain one the
    /// constructors derive from the buffer.
    ///
    /// The caller has the `StyledText` in hand — it read the plain text out of it — and this
    /// is where the runs it is about to preserve come from. Without it the first splice would
    /// be against a value that had already been flattened, which is the bug this whole field
    /// exists to fix, one step earlier.
    ///
    /// Debug-asserted to agree with the buffer: a styled value whose plain form is not what
    /// the caret is indexing would put the runs one offset out.
    pub fn with_styling(mut self, styled: vellum_doc::StyledText) -> Self {
        debug_assert_eq!(
            styled.to_plain(),
            self.buffer.text(),
            "the session's styled value must be the same words the buffer holds"
        );
        self.styled = styled;
        self
    }

    /// Folds the buffer's current text into the styled value and returns what to write.
    ///
    /// Called once per keystroke by `crate::actions`, which is what makes the splice exact:
    /// one call, one contiguous change. See [`splice_styling`].
    pub fn styled_now(&mut self) -> vellum_doc::StyledText {
        self.styled = splice_styling(&self.styled, self.buffer.text());
        self.styled.clone()
    }

    /// Whether an undo group has been opened for this session.
    pub fn opened(&self) -> bool {
        self.opened
    }

    /// Records that the document has been written to, so the group is opened once.
    pub fn mark_opened(&mut self) {
        self.opened = true;
    }

    /// Whether an input method is composing right now.
    ///
    /// Read by `crate::actions::type_key` to stand aside: while this is true the characters
    /// belong to the input method, and inserting them from `KeyEvent::text` as well would
    /// type everything twice.
    pub const fn composing(&self) -> bool {
        self.preedit.is_some()
    }

    /// Replaces the composing text with `text`, returning whether the buffer changed.
    ///
    /// One function for both halves of the IME protocol, because they are the same operation
    /// on different arguments: a `Preedit` replaces the composition and stays composing, a
    /// `Commit` replaces it and stops. Splitting them would mean two copies of the
    /// range arithmetic, and the two going out of step leaves stray composition in the text.
    ///
    /// The first call inserts at the cursor, replacing any selection — exactly what typing a
    /// character does — so beginning a composition over selected text replaces it, as it
    /// should.
    pub fn set_composition(&mut self, text: &str, still_composing: bool) -> bool {
        let changed = match self.preedit.clone() {
            Some(range) => self.buffer.replace_range(range, text),
            None => self.buffer.insert(text),
        };
        // The range of what was just written, measured from where it ended: the cursor is
        // always left at the end of an insertion, which is the one invariant `TextBuffer`
        // guarantees for every mutation.
        self.preedit = still_composing
            .then(|| self.buffer.cursor().saturating_sub(text.len())..self.buffer.cursor());
        changed
    }

    /// Abandons any composition without touching the text.
    ///
    /// For `Ime::Disabled`, and for the end of a session: whatever was composed has already
    /// been written through, so the only thing to forget is that it was provisional. Leaving
    /// a stale range behind would make the *next* composition replace text the user typed.
    pub fn end_composition(&mut self) {
        self.preedit = None;
    }
}

/// A string, a cursor and a selection anchor — and every transition between them.
///
/// Pure: no ids, no layout, no document. That is what lets grapheme movement, word
/// motion and the boundary snapping below be tested as arithmetic over a `&str`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBuffer {
    /// The live string. Always equal to what was last written to the document, so the
    /// canvas and this cannot disagree — see the module docs.
    text: String,
    /// Byte offset of the cursor. Always on a `char` boundary.
    cursor: usize,
    /// The other end of the selection. Equal to `cursor` when nothing is selected.
    anchor: usize,
}

impl TextBuffer {
    /// Cursor at the end, nothing selected.
    pub fn at_end(text: String) -> Self {
        let end = text.len();
        Self { text, cursor: end, anchor: end }
    }

    /// Everything selected, so the first keystroke replaces it.
    pub fn all_selected(text: String) -> Self {
        let end = text.len();
        Self { text, cursor: end, anchor: 0 }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// The selected range, low end first. Empty when there is no selection.
    pub fn selection(&self) -> std::ops::Range<usize> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }

    pub fn has_selection(&self) -> bool {
        self.cursor != self.anchor
    }

    /// Inserts text at the cursor, replacing any selection. Returns whether anything
    /// changed.
    ///
    /// Newlines arrive through here too, from `Enter`: there is nothing special about one
    /// in a plain buffer, and the layout splits paragraphs on it.
    pub fn insert(&mut self, what: &str) -> bool {
        if what.is_empty() {
            return false;
        }
        let range = self.selection();
        self.text.replace_range(range.clone(), what);
        self.cursor = range.start + what.len();
        self.anchor = self.cursor;
        true
    }

    /// Replaces an exact byte range with `what`, leaving the cursor at the end of it.
    ///
    /// For **IME composition**, which is the one edit that is not "at the cursor": each
    /// `Preedit` replaces the previous one, and the range it replaces is remembered rather
    /// than re-derived. Ordinary keystrokes go through [`TextBuffer::insert`].
    ///
    /// The range is clamped and snapped to character boundaries rather than trusted. It comes
    /// from a previous call's arithmetic, and an input method that reports a composition the
    /// buffer has since been changed under — a click that moved the caret mid-composition —
    /// must not be able to panic `replace_range`.
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, what: &str) -> bool {
        let start = self.snap(range.start.min(self.text.len()));
        let end = self.snap(range.end.clamp(start, self.text.len()));
        if start == end && what.is_empty() {
            return false;
        }
        self.text.replace_range(start..end, what);
        self.cursor = start + what.len();
        self.anchor = self.cursor;
        true
    }

    /// Deletes the selection, or the grapheme before the cursor. Returns whether anything
    /// changed.
    pub fn backspace(&mut self) -> bool {
        if self.has_selection() {
            return self.delete_selection();
        }
        let Some(from) = self.grapheme_before(self.cursor) else { return false };
        self.text.replace_range(from..self.cursor, "");
        self.cursor = from;
        self.anchor = from;
        true
    }

    /// Deletes the selection, or the grapheme after the cursor.
    pub fn delete(&mut self) -> bool {
        if self.has_selection() {
            return self.delete_selection();
        }
        let Some(to) = self.grapheme_after(self.cursor) else { return false };
        self.text.replace_range(self.cursor..to, "");
        true
    }

    fn delete_selection(&mut self) -> bool {
        let range = self.selection();
        if range.is_empty() {
            return false;
        }
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = range.start;
        true
    }

    /// Moves the cursor. `extend` leaves the anchor where it is, which is what Shift does.
    pub fn move_cursor(&mut self, motion: Motion, extend: bool) {
        let to = match motion {
            Motion::Left => self.collapse_or(extend, false, |s| s.grapheme_before(s.cursor)),
            Motion::Right => self.collapse_or(extend, true, |s| s.grapheme_after(s.cursor)),
            Motion::WordLeft => self.word_before(self.cursor),
            Motion::WordRight => self.word_after(self.cursor),
            Motion::ParagraphStart => self.paragraph_bounds().0,
            Motion::ParagraphEnd => self.paragraph_bounds().1,
            Motion::DocumentStart => 0,
            Motion::DocumentEnd => self.text.len(),
        };
        self.place(to, extend);
    }

    /// An unextended Left/Right with a selection collapses to its near end rather than
    /// stepping from the cursor — the behaviour every editor has, and the one people
    /// notice the absence of immediately.
    fn collapse_or(
        &self,
        extend: bool,
        forward: bool,
        step: impl Fn(&Self) -> Option<usize>,
    ) -> usize {
        if !extend && self.has_selection() {
            let range = self.selection();
            return if forward { range.end } else { range.start };
        }
        step(self).unwrap_or(self.cursor)
    }

    /// Puts the cursor at a byte offset, snapped to a `char` boundary.
    ///
    /// The one entry point for a motion the string cannot compute itself — Up, Down, and
    /// a click — so the boundary snapping and the anchor rule are written once.
    pub fn place(&mut self, byte: usize, extend: bool) {
        let byte = self.snap(byte);
        self.cursor = byte;
        if !extend {
            self.anchor = byte;
        }
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    /// Selects the word around a byte offset — a **double click**.
    ///
    /// *"when i am in the text typing mode in the notepad, if i double click it should select
    /// the word, and if i click one more time it should select everything."* Every text field
    /// in every application does this, which is why its absence reads as the field being
    /// broken rather than as a feature being missing.
    ///
    /// Word here is `unicode_word_indices`' word, the same one `⌥←` and `⌥→` land on, so a
    /// double click and a word motion cannot disagree about where a word begins.
    ///
    /// A click in the run of spaces *between* two words selects that run rather than jumping
    /// to a neighbour: it is what the pointer was actually on, and selecting a word the user
    /// did not point at is worse than selecting the gap they did.
    pub fn select_word_at(&mut self, byte: usize) {
        let byte = self.snap(byte);
        let word = self
            .text
            .unicode_word_indices()
            .map(|(at, word)| at..at + word.len())
            .find(|range| range.contains(&byte) || range.start == byte);
        let range = word.unwrap_or_else(|| {
            // Not inside a word: take the run of non-word characters the click landed in, so
            // a double click on a gap still selects *something* the user can see.
            let start = self.text[..byte]
                .char_indices()
                .rev()
                .take_while(|(_, c)| !c.is_alphanumeric())
                .last()
                .map_or(byte, |(at, _)| at);
            let end = self.text[byte..]
                .char_indices()
                .take_while(|(_, c)| !c.is_alphanumeric())
                .last()
                .map_or(byte, |(at, c)| byte + at + c.len_utf8());
            start..end
        });
        self.anchor = range.start;
        self.cursor = range.end;
    }

    /// The nearest `char` boundary at or before `byte`, clamped to the string.
    ///
    /// A layout resolves a click to a cluster boundary, so this should never have to do
    /// anything — but "should never" and "cannot" are different, and a byte offset that
    /// splits a character panics `String::replace_range` rather than misbehaving quietly.
    fn snap(&self, byte: usize) -> usize {
        let mut byte = byte.min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        byte
    }

    fn grapheme_before(&self, byte: usize) -> Option<usize> {
        let byte = self.snap(byte);
        self.text[..byte].grapheme_indices(true).next_back().map(|(at, _)| at)
    }

    fn grapheme_after(&self, byte: usize) -> Option<usize> {
        let byte = self.snap(byte);
        self.text[byte..].graphemes(true).next().map(|g| byte + g.len())
    }

    /// The start of the word before the cursor, or the start of the text.
    ///
    /// `unicode_word_indices` skips punctuation and whitespace runs, which is what makes
    /// `⌥←` land on words rather than on every gap between them.
    fn word_before(&self, byte: usize) -> usize {
        let byte = self.snap(byte);
        self.text[..byte]
            .unicode_word_indices()
            .next_back()
            .map_or(0, |(at, _)| at)
    }

    fn word_after(&self, byte: usize) -> usize {
        let byte = self.snap(byte);
        self.text[byte..]
            .unicode_word_indices()
            // The word the cursor is *inside* starts at 0 and is not a destination, so
            // the end of the first word is what `⌥→` means.
            .map(|(at, word)| byte + at + word.len())
            .find(|end| *end > byte)
            .unwrap_or(self.text.len())
    }

    /// The paragraph the cursor is in, as byte offsets. The separator belongs to neither
    /// side, so `End` on a wrapped line stops before the `\n`.
    fn paragraph_bounds(&self) -> (usize, usize) {
        let cursor = self.snap(self.cursor);
        let start = self.text[..cursor].rfind('\n').map_or(0, |at| at + 1);
        let end = self.text[cursor..].find('\n').map_or(self.text.len(), |at| cursor + at);
        (start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(text: &str) -> TextBuffer {
        TextBuffer::at_end(text.to_owned())
    }

    #[test]
    fn a_new_session_puts_the_cursor_at_the_end_and_selects_nothing() {
        let editing = session("hello");
        assert_eq!(editing.cursor(), 5);
        assert!(!editing.has_selection());
        // A freshly placed item selects everything instead, so the first keystroke
        // replaces the placeholder.
        let placed = TextBuffer::all_selected("Frame".to_owned());
        assert!(placed.has_selection());
        assert_eq!(placed.selection(), 0..5);
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut editing = session("hello");
        editing.select_all();
        assert!(editing.insert("bye"));
        assert_eq!(editing.text(), "bye");
        assert_eq!(editing.cursor(), 3);
        assert!(!editing.has_selection(), "the selection outlived what it replaced");
    }

    #[test]
    fn inserting_nothing_changes_nothing() {
        let mut editing = session("hi");
        assert!(!editing.insert(""));
        assert_eq!(editing.text(), "hi");
    }

    /// Grapheme movement, not `char` movement. A family emoji is four code points and one
    /// key press; `char`-wise backspace would take four presses and leave three broken
    /// intermediate states on screen.
    #[test]
    fn backspace_removes_one_grapheme_not_one_char() {
        let family = "👨‍👩‍👦";
        assert!(family.chars().count() > 1, "the fixture is not a multi-char cluster");
        let mut editing = session(&format!("a{family}"));
        assert!(editing.backspace());
        assert_eq!(editing.text(), "a", "backspace split a grapheme cluster");
        assert!(editing.backspace());
        assert_eq!(editing.text(), "");
        // And at the start there is nothing to take.
        assert!(!editing.backspace());
    }

    #[test]
    fn delete_takes_the_grapheme_after_the_cursor() {
        let mut editing = session("café!");
        editing.place(3, false); // before the `é`, which is two bytes
        assert!(editing.delete());
        assert_eq!(editing.text(), "caf!");
        editing.place(editing.text().len(), false);
        assert!(!editing.delete(), "there is nothing after the end");
    }

    #[test]
    fn arrows_step_by_grapheme_and_shift_extends() {
        let mut editing = session("abc");
        editing.move_cursor(Motion::Left, false);
        assert_eq!(editing.cursor(), 2);
        assert!(!editing.has_selection());

        editing.move_cursor(Motion::Left, true);
        assert_eq!(editing.cursor(), 1);
        assert_eq!(editing.selection(), 1..2, "shift-left did not extend");

        // An unextended arrow with a selection collapses to its near end rather than
        // stepping from the cursor.
        editing.move_cursor(Motion::Right, false);
        assert_eq!(editing.cursor(), 2);
        assert!(!editing.has_selection());
    }

    #[test]
    fn word_motion_lands_on_words_rather_than_on_every_gap() {
        let mut editing = session("one two  three");
        editing.move_cursor(Motion::WordLeft, false);
        assert_eq!(&editing.text()[editing.cursor()..], "three");
        editing.move_cursor(Motion::WordLeft, false);
        assert_eq!(&editing.text()[editing.cursor()..], "two  three");
        editing.move_cursor(Motion::WordLeft, false);
        assert_eq!(editing.cursor(), 0);
        // And back out again, stopping at the end of each word.
        editing.move_cursor(Motion::WordRight, false);
        assert_eq!(&editing.text()[..editing.cursor()], "one");
        editing.move_cursor(Motion::WordRight, false);
        assert_eq!(&editing.text()[..editing.cursor()], "one two");
    }

    /// Home and End are the *paragraph's* ends, and the separator belongs to neither
    /// side — otherwise End on the first line puts the cursor on the second.
    #[test]
    fn home_and_end_stay_inside_their_paragraph() {
        let mut editing = session("one\ntwo\nthree");
        editing.place(5, false); // inside "two"
        editing.move_cursor(Motion::ParagraphStart, false);
        assert_eq!(editing.cursor(), 4);
        editing.move_cursor(Motion::ParagraphEnd, false);
        assert_eq!(editing.cursor(), 7, "End crossed the newline");

        editing.move_cursor(Motion::DocumentStart, false);
        assert_eq!(editing.cursor(), 0);
        editing.move_cursor(Motion::DocumentEnd, false);
        assert_eq!(editing.cursor(), editing.text().len());
    }

    /// A byte offset that splits a character would panic `replace_range`. Snapping is the
    /// difference between a misplaced caret and a crash.
    #[test]
    fn an_offset_inside_a_character_snaps_rather_than_panicking() {
        let mut editing = session("café");
        editing.place(4, false); // between the two bytes of `é`
        assert_eq!(editing.cursor(), 3);
        editing.place(9_999, false);
        assert_eq!(editing.cursor(), editing.text().len());
        assert!(editing.backspace());
        assert_eq!(editing.text(), "caf");
    }

    /// Enter is an insertion like any other in a plain buffer — worth pinning, because
    /// the layout's paragraph handling depends on the `\n` actually being there.
    #[test]
    fn enter_inserts_a_separator() {
        let mut editing = session("ab");
        editing.place(1, false);
        assert!(editing.insert("\n"));
        assert_eq!(editing.text(), "a\nb");
        assert_eq!(editing.cursor(), 2);
    }

    // ----- styling across an edit ------------------------------------------------------

    use vellum_doc::{SpanStyle, StyledText, TextSpan};

    /// `"plain"` + `"BOLD"` + `"tail"`, the shape every test below splices into.
    fn mixed() -> StyledText {
        StyledText::from_spans([
            TextSpan::plain("plain "),
            TextSpan::new("BOLD", SpanStyle::bold()),
            TextSpan::plain(" tail"),
        ])
    }

    /// The runs, as `(text, bold)`, which is all these tests care about.
    fn runs(text: &StyledText) -> Vec<(String, bool)> {
        text.spans().iter().map(|s| (s.text.clone(), s.style.bold)).collect()
    }

    /// The defect, stated as a test: typing into a formatted item used to flatten it.
    ///
    /// A keystroke *outside* the bold run must leave that run bold. `StyledText::plain(text)`
    /// — what `flush_editing` used to write — fails this, and nothing else in the app would
    /// have noticed, because the words are right and only the runs are gone.
    #[test]
    fn an_edit_outside_a_run_leaves_it_formatted() {
        let after = splice_styling(&mixed(), "plain! BOLD tail");
        assert_eq!(
            runs(&after),
            vec![
                ("plain! ".to_owned(), false),
                ("BOLD".to_owned(), true),
                (" tail".to_owned(), false),
            ]
        );
    }

    /// Typing at the end of a bold run continues in bold, which is what every editor does:
    /// the inserted text inherits the style of the character *before* the cursor.
    #[test]
    fn text_typed_at_a_runs_end_inherits_that_run() {
        let after = splice_styling(&mixed(), "plain BOLDER tail");
        assert_eq!(
            runs(&after),
            vec![
                ("plain ".to_owned(), false),
                ("BOLDER".to_owned(), true),
                (" tail".to_owned(), false),
            ],
            "the two bold pieces merged into one run rather than staying adjacent twins"
        );
    }

    /// At the very start there is no preceding character, so the first run's style is used.
    /// Plain would put one stray regular word at the head of a bold paragraph.
    #[test]
    fn text_typed_at_the_head_takes_the_first_runs_style() {
        let bold = StyledText::from_spans([TextSpan::new("BOLD", SpanStyle::bold())]);
        let after = splice_styling(&bold, "xBOLD");
        assert_eq!(runs(&after), vec![("xBOLD".to_owned(), true)]);
    }

    /// A deletion spanning two runs keeps what is left of both, and keeps them apart.
    #[test]
    fn a_deletion_across_a_boundary_keeps_both_sides() {
        // "plain BOLD tail" -> "plaLD tail": drops "in " and "BO".
        let after = splice_styling(&mixed(), "plaLD tail");
        assert_eq!(
            runs(&after),
            vec![
                ("pla".to_owned(), false),
                ("LD".to_owned(), true),
                (" tail".to_owned(), false),
            ]
        );
    }

    /// Deleting a whole run removes it rather than leaving an empty one — `from_spans`
    /// normalises, and two values describing the same text must compare equal.
    #[test]
    fn deleting_a_whole_run_removes_it() {
        let after = splice_styling(&mixed(), "plain  tail");
        assert_eq!(runs(&after), vec![("plain  tail".to_owned(), false)]);
        assert_eq!(after, StyledText::plain("plain  tail"), "and normalises to the plain value");
    }

    /// The overlap case. `"aa" -> "a"` has a common prefix of one *and* a common suffix of
    /// one, and counting both would splice out nothing — leaving the text unchanged while the
    /// buffer had already moved on.
    #[test]
    fn a_repeated_character_does_not_double_count_the_overlap() {
        let doubled = StyledText::from_spans([TextSpan::new("aa", SpanStyle::bold())]);
        let after = splice_styling(&doubled, "a");
        assert_eq!(after.to_plain(), "a", "the words follow the buffer, whatever the runs do");
        assert_eq!(runs(&after), vec![("a".to_owned(), true)]);
    }

    /// Clearing everything and typing fresh — what a `replacing` session does on its first
    /// keystroke — must not resurrect the old runs.
    #[test]
    fn replacing_everything_keeps_only_the_inherited_style() {
        let after = splice_styling(&mixed(), "x");
        assert_eq!(after.to_plain(), "x");
        assert_eq!(after.spans().len(), 1);
    }

    /// Emptying the text is legal and must not panic on the empty-range arithmetic.
    #[test]
    fn emptying_the_text_is_legal() {
        let after = splice_styling(&mixed(), "");
        assert!(after.is_empty());
        assert_eq!(after.to_plain(), "");
    }

    /// Multi-byte characters: the splice is measured in characters, so an edit after an
    /// emoji must not cut it in half.
    #[test]
    fn a_splice_measured_in_characters_survives_multibyte_text() {
        let text = StyledText::from_spans([
            TextSpan::plain("héllo 👋"),
            TextSpan::new("wörld", SpanStyle::bold()),
        ]);
        let after = splice_styling(&text, "héllo 👋wörld!");
        assert_eq!(after.to_plain(), "héllo 👋wörld!");
        assert_eq!(
            runs(&after),
            vec![("héllo 👋".to_owned(), false), ("wörld!".to_owned(), true)],
            "the `!` followed the bold character before it"
        );
    }

    /// An unchanged string is returned untouched, including its block structure — which
    /// `from_spans` would otherwise drop on every keystroke that changed nothing.
    #[test]
    fn an_unchanged_string_keeps_its_blocks() {
        let listed = StyledText::plain("one\ntwo")
            .with_blocks([vellum_doc::BlockStyle::default(), vellum_doc::BlockStyle::default()]);
        let after = splice_styling(&listed, "one\ntwo");
        assert_eq!(after, listed);
    }

    /// A session runs many splices in a row, one per keystroke, and the runs have to hold
    /// through all of them — that is the case the app actually exercises.
    #[test]
    fn a_run_of_keystrokes_holds_the_formatting() {
        let mut styled = mixed();
        // Typing " more" at the very end, one character at a time.
        let mut plain = styled.to_plain();
        for c in " more".chars() {
            plain.push(c);
            styled = splice_styling(&styled, &plain);
        }
        assert_eq!(styled.to_plain(), "plain BOLD tail more");
        assert_eq!(
            runs(&styled),
            vec![
                ("plain ".to_owned(), false),
                ("BOLD".to_owned(), true),
                (" tail more".to_owned(), false),
            ],
            "twenty splices later the bold run is still exactly where it was"
        );
    }
}
