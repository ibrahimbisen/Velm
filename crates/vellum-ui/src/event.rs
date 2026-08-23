//! Everything the chrome asks the app to do.
//!
//! The chrome performs no action itself. It does not open files, mutate documents,
//! change the camera or touch the clipboard — it emits [`UiEvent`]s and the app
//! decides. That is what makes the whole crate testable without a document, a GPU or
//! a window: drive it with synthetic input, read the events back, assert on them.
//!
//! The split between [`Command`] and the payload-carrying variants is deliberate.
//! Commands are `Copy`, comparable and enumerable, so one table drives the menu bar,
//! the keymap and a future command palette; anything with a value that the user
//! chose gets its own variant instead of being smuggled into a string.

use crate::command::Command;
use crate::selection::{Border, FontWeight, VerticalAlign};
use crate::theme::{Theme, ThemePreference};
use crate::tool::{CustomShapeId, EraserMode, PenPreset, ShapeColors, ShapeGroup, Tool};
use std::path::PathBuf;
use vellum_connect::{AnchorSide, Arrowhead, LineStyle, RoutingMode};
use vellum_doc::{Align, Color, Pattern};

/// The grid, as one application-wide setting.
///
/// Pattern and ink together in one struct so [`UiEvent::GridChanged`] can carry the whole
/// thing: each of the three controls in View ▸ Grid edits one field of the value it was
/// handed, which is what stops a colour chosen a moment ago being reverted by a pattern
/// chosen now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GridSettings {
    /// Which texture, including [`Pattern::Plain`] for none at all.
    pub pattern: Pattern,
    /// What it is drawn in — colour **and** alpha, since [`Color`] carries both and they
    /// describe the same pixel. `None` follows the app's own grid ink.
    pub color: Option<Color>,
}

/// A password on its way to one request. Never printed, never stored, never compared
/// against anything but itself.
///
/// **The hand-written `Debug` is the mechanism, not a nicety.** [`UiEvent`] derives
/// `Debug` and so does [`EventSink`], so a bare `String` password inside a variant is a
/// password in whatever log line ever formats a sink. This is the same class that
/// `vellum_app::options`' `OnceLock` and `velmd`'s *nothing that holds a secret prints it*
/// test exist to prevent, and the precedent for writing `Debug` by hand rather than
/// promising in prose is `vellum_app::sync::SyncReply`.
///
/// It has **no `Display`**, so `format!("{secret}")` does not compile. The one way to read
/// the characters is [`Secret::expose`], which is named to be visible in a review.
///
/// # What this type cannot promise
///
/// The bytes are **not** erased from memory. `String::clear` sets a length and does not
/// zero a buffer, egui's text field keeps its own copy including undo history, the JSON
/// body is another, and the HTTP send buffer is a fourth. Claiming otherwise would be a
/// failure reported as a success.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub const fn new(password: String) -> Self {
        Self(password)
    }

    /// The characters, for the one request that needs them.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// One thing the user did.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// A named action, from a menu, a toolbar button or a keyboard shortcut.
    Command(Command),
    /// The active tool changed.
    ToolChanged(Tool),
    /// A shape was chosen in the shape flyout. Implies the shape tool is now active;
    /// the chrome emits [`UiEvent::ToolChanged`] alongside it rather than leaving the
    /// app to infer that.
    ShapeChosen(vellum_shapes::Shape),
    /// The colour the next sticky is placed in, from the sticky tool's flyout.
    ///
    /// A *tool* setting rather than a style edit: it names what the next note will be, and
    /// changes nothing already on the board — which is why it is its own event and not a
    /// `StyleEdit::Fill` with no selection.
    StickyColorChosen(vellum_doc::Color),
    /// One of the user's own SVG shapes was chosen. Carries the id the app gave it in
    /// [`Chrome::set_custom_shapes`](crate::Chrome::set_custom_shapes), because the
    /// chrome never parses an SVG and has nothing else to identify it by.
    CustomShapeChosen(CustomShapeId),
    /// *Browse and upload SVG shapes* was clicked. The app owns the file dialog; the
    /// chrome only asks.
    UploadShape,
    /// A shape category's *Apply colors* changed. These are the colours a newly placed
    /// shape from that category takes, not an edit to the selection — recolouring what
    /// is already on the board is [`UiEvent::Style`].
    ShapeColorsChanged { group: ShapeGroup, colors: ShapeColors },
    /// The pen flyout's settings changed.
    PenChanged(PenPreset),
    /// *Open* was clicked on a link card: the app should hand this URL to the browser.
    ///
    /// The chrome does not open it. It cannot — spawning a process is exactly the kind of
    /// action this crate leaves to the app, for the same reason it does not touch files or
    /// the clipboard.
    OpenLink(String),
    /// *Copy link* was clicked on a link card: the app should put this URL on the system
    /// pasteboard.
    ///
    /// A separate event from [`UiEvent::OpenLink`] rather than a flag on it, because the
    /// two are different verbs with different consequences — one leaves the application
    /// and one does not — and because the app has to clear its own clipboard sentinel for
    /// this one. Like opening a page, the chrome does not do it: this crate touches
    /// neither the clipboard nor the filesystem.
    CopyLink(String),
    /// The eraser flyout's mode changed: strokes or whole objects.
    ///
    /// A mode rather than only the ⇧ modifier it used to be. The app keeps the mode; ⇧
    /// still inverts it for one gesture — see
    /// [`EraserMode::with_shift`](crate::EraserMode::with_shift).
    EraserChanged(EraserMode),
    /// A styling control was changed. Applies to the whole current selection,
    /// including the items that showed *Mixed*.
    Style(StyleEdit),
    /// A numeric transform field was committed.
    Transform(TransformEdit),
    /// The board's canvas colour or pattern was chosen in Board ▸ Background.
    ///
    /// Carries the whole background rather than the one field that moved, so the app
    /// writes one value and cannot end up with a colour from one event and a pattern
    /// from another.
    BackgroundChanged(vellum_doc::Background),
    /// The grid was chosen in View ▸ Grid — its pattern, its colour or its opacity.
    ///
    /// **An application setting, not a board's**, at the user's instruction: *"grid opacity
    /// and grid color and grid should apply to all of the boards not just to that board"*.
    /// So this is its own event rather than another field on `BackgroundChanged`, which
    /// writes the open board's document — one carries a document edit, the other a
    /// preference, and folding them together is how a preference ends up in 42 files.
    ///
    /// Carries the whole grid for the same reason `BackgroundChanged` carries the whole
    /// background: the app writes one value and cannot end up with a colour from one event
    /// and a pattern from another.
    GridChanged(GridSettings),
    /// The words of the one selected item were changed.
    ///
    /// Emitted on every keystroke, so the canvas keeps up with the field. The app is
    /// expected to fold a run of these into one undo step rather than one per
    /// character — see `vellum_doc::Board::set_undo_merge_interval`.
    TextEdited(String),
    /// The zoom percentage was typed or dragged directly, as a scale factor where
    /// `1.0` is 100%.
    ZoomTo(f32),
    /// A tab was brought to the front, by a click, a shortcut or the strip's own
    /// arithmetic after a close.
    ///
    /// A **strip index**: `0` is the home tab — the board library — and `1..` are the
    /// board tabs in the order [`Chrome::tabs`](crate::Chrome::tabs) lists them, which
    /// is the order the strip draws them and not necessarily the order the app last
    /// supplied. The strip has already applied it to itself; the app's job is to put
    /// the matching document in front, and to set
    /// [`Screen`](crate::Screen) to `Library` for `0`.
    SelectTab(usize),
    /// A tab's `×` was clicked, or it was middle-clicked. Never `0`: the home tab
    /// cannot be closed.
    ///
    /// The strip has already removed it and moved the selection to a neighbour — see
    /// [`tabs::active_after_close`](crate::tabs::active_after_close) — so the app
    /// flushes and releases that document rather than deciding anything.
    CloseTab(usize),
    /// A tab was dragged to a new slot. Both indices are strip indices and neither is
    /// ever `0`, because nothing can be dropped in front of home.
    ///
    /// Reported for the whole move rather than for each slot it crossed. The strip has
    /// already rearranged itself; an app that keeps its own order can follow, and one
    /// that does not can ignore this without the strip springing back.
    ReorderTabs { from: usize, to: usize },
    /// The `+` at the end of the strip, or `Cmd+T`.
    ///
    /// A new tab shows the board library, exactly as a browser's new tab shows its
    /// start page — which is why this arrives with the home tab already selected. The
    /// app has nothing it must do; the event is here so a *New board* flow can be
    /// hung off it later without another round of wiring.
    NewBoardTab,
    /// A board in the library was acted on.
    Library(LibraryEvent),
    /// The find bar was used.
    Find(FindEvent),
    /// A modal dialog was answered.
    Dialog(DialogEvent),
    /// The user switched palettes. The app persists this; the chrome only reports it.
    ThemeChanged(Theme),
    /// The user chose light, dark, or follow-the-system in Preferences ▸ Appearance.
    /// The chrome applies it to itself immediately; the app only has to persist it.
    ThemePreferenceChanged(ThemePreference),
    /// The user moved the transparency slider in Preferences. Same contract as
    /// [`UiEvent::ThemePreferenceChanged`]: applied by the chrome, persisted by the app.
    ///
    /// Emitted when the drag **ends**, not on every frame it is held. Persisting is a
    /// synchronous `fs::write`, and a slider reports a change on every frame the button
    /// is down — roughly a hundred a second.
    GlassOpacityChanged(u8),
    /// The user picked a colour in Preferences ▸ Accent colour. Same contract again: the
    /// chrome applies it to itself, the app persists it and repaints the board with it.
    ///
    /// The **board** is why this reaches the app at all rather than staying inside the
    /// chrome: a selection ring is drawn by `vellum-render`, not by egui, so
    /// `vellum_app::theme::Theme` carries the same accent and has to be told.
    AccentChanged(crate::theme::Accent),
    /// *Sign in* was pressed on Settings ▸ Account.
    ///
    /// The chrome does no network work of its own, so this is the whole of what it knows:
    /// the three things that were typed. The app normalises the address, posts, reads the
    /// session back and reports what happened through
    /// [`Chrome::set_account_status`](crate::Chrome::set_account_status).
    ///
    /// `server` is **raw**, exactly as it was typed. Normalising it in the chrome would put
    /// a second answer to *what is a valid address* beside the app's own, and two clients
    /// disagreeing about one input is a defect this repository has already paid for.
    SignInRequested { server: String, username: String, password: Secret },
    /// *Sign out* was pressed. Nothing is carried: the app knows who is signed in.
    SignOutRequested,
}

impl UiEvent {
    /// Convenience for the common case of wrapping a command.
    pub const fn command(command: Command) -> Self {
        Self::Command(command)
    }
}

/// A change to the selection's appearance.
///
/// One variant per control rather than a partial `Style` struct: a struct would make
/// "the user changed the fill" and "the user changed the fill and cleared the border"
/// the same value, and the app needs to know which, because each is one undo step.
#[derive(Debug, Clone, PartialEq)]
pub enum StyleEdit {
    /// `None` is an explicit "no fill", not "leave it alone".
    Fill(Option<Color>),
    BorderColor(Color),
    BorderWidth(f64),
    BorderStyle(LineStyle),
    /// Removes the border entirely.
    BorderCleared,
    /// 0.0–1.0.
    Opacity(f64),
    /// `None` restores the board's default family.
    FontFamily(Option<String>),
    /// `None` is auto-fit.
    FontSize(Option<f64>),
    FontWeight(FontWeight),
    TextColor(Color),
    Align(Align),
    VerticalAlign(VerticalAlign),
    /// A multiple of the font size.
    LineHeight(f64),
    Locked(bool),
    Routing(RoutingMode),
    StartArrow(Arrowhead),
    EndArrow(Arrowhead),
    /// Which point of the attached item this end ties to. Only the pickable sides are
    /// ever emitted — see [`AnchorSide::is_pickable`](crate::AnchorSide::is_pickable).
    StartAnchor(AnchorSide),
    EndAnchor(AnchorSide),
    /// How much of a link or embed card to draw.
    CardMode(vellum_doc::CardMode),
}

impl StyleEdit {
    /// The complete border in one edit, for the app's convenience when applying a
    /// preset rather than a single control.
    pub fn border(border: Border) -> [Self; 3] {
        [
            Self::BorderColor(border.color),
            Self::BorderWidth(border.width),
            Self::BorderStyle(border.style),
        ]
    }
}

/// A committed numeric position or size field, in world units.
///
/// For a multi-selection the values describe the selection's bounding box: `X` moves
/// every item so the box starts there. Width and height are not emitted for a
/// multi-selection at all — see [`PanelModel::size_editable`](crate::PanelModel).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransformEdit {
    X(f64),
    Y(f64),
    Width(f64),
    Height(f64),
    /// Degrees clockwise, matching `Placement::rotation`.
    Rotation(f64),
}

/// Something the user did to a board — or to a space — in the library.
///
/// Spaces are identified by name rather than by their index in the list the app
/// supplied. An index is only valid until the app next replaces the list, and it will
/// replace it in response to these very events; a name is what the user typed and what
/// they see on the row they clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryEvent {
    Open(PathBuf),
    Duplicate(PathBuf),
    /// Move a board to Recently deleted. **Not** a removal: nothing on disk is touched,
    /// and `Restore` puts it straight back. See `Library::trash`.
    Delete(PathBuf),
    /// Take a board back out of Recently deleted.
    Restore(PathBuf),
    /// Delete a board **for good**, from Recently deleted. This is the only path in the
    /// interface that removes a `.vellum` file, and the app is expected to confirm it.
    Purge(PathBuf),
    /// Empty Recently deleted: [`Self::Purge`] for everything in it, behind one
    /// confirmation rather than one per board.
    EmptyTrash,
    /// Rename was chosen from a board's context menu, with the title it has now. The
    /// app decides what to ask — typically by queueing a [`Dialog::Rename`] and
    /// hearing the answer back as [`DialogEvent::Renamed`].
    ///
    /// [`Dialog::Rename`]: crate::Dialog::Rename
    Rename { path: PathBuf, title: String },
    /// The star on a board row or card was clicked. Carries the state it should end
    /// up in rather than "toggle", so a double click cannot desynchronise it.
    SetStarred { path: PathBuf, starred: bool },
    /// File a board under a space, or take it out of the one it is in with `None`.
    MoveToSpace { path: PathBuf, space: Option<String> },
    /// The `+` beside the Spaces header. The app names the new space — typically by
    /// queueing a [`Dialog::Rename`](crate::Dialog::Rename) seeded with a blank.
    CreateSpace,
    /// Rename was chosen on a space, with the name it has now.
    RenameSpace(String),
    /// Delete was chosen on a space. Deletes the folder, never the boards in it — the
    /// app decides whether to confirm.
    DeleteSpace(String),
    /// A space was pinned or unpinned. Pinned spaces sort to the top of the sidebar.
    SetSpacePinned { space: String, pinned: bool },
    /// The search box changed. Filtering is done by the chrome for display, but the
    /// app may want it for a cross-board content search.
    SearchChanged(String),
}

/// Something the user did in the find bar.
///
/// The chrome owns the field and the buttons; the app owns the index and the camera.
/// `docs/features/README.md` §3 puts find over `vellum-search`'s inverted index, which
/// is not something a panel can do in a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindEvent {
    /// The query changed. Empty when the field was cleared.
    Query(String),
    /// Step to the next or previous match.
    Next,
    Previous,
    /// The bar was closed — Escape, or its close button. The app should drop any
    /// highlight it is drawing.
    Closed,
}

/// Identifies a dialog so the app can correlate the answer with the question it
/// asked. The chrome never interprets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DialogId(pub u64);

/// The outcome of a modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogEvent {
    Confirmed(DialogId),
    Cancelled(DialogId),
    Renamed(DialogId, String),
}

impl DialogEvent {
    pub const fn id(&self) -> DialogId {
        match self {
            Self::Confirmed(id) | Self::Cancelled(id) | Self::Renamed(id, _) => *id,
        }
    }
}

/// Collects events during a frame.
///
/// A plain `Vec` behind a named type so the panels take one argument rather than
/// threading a mutable vector, and so ordering — events come out in the order the
/// user caused them — is a property of the type rather than a convention.
#[derive(Debug, Default)]
pub struct EventSink {
    events: Vec<UiEvent>,
}

impl EventSink {
    pub fn push(&mut self, event: UiEvent) {
        self.events.push(event);
    }

    pub fn command(&mut self, command: Command) {
        self.push(UiEvent::Command(command));
    }

    pub fn style(&mut self, edit: StyleEdit) {
        self.push(UiEvent::Style(edit));
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn take(&mut self) -> Vec<UiEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn as_slice(&self) -> &[UiEvent] {
        &self.events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_preserves_the_order_events_were_caused_in() {
        let mut sink = EventSink::default();
        assert!(sink.is_empty());
        sink.command(Command::Undo);
        sink.style(StyleEdit::Opacity(0.5));
        sink.command(Command::Save);

        assert_eq!(
            sink.take(),
            vec![
                UiEvent::Command(Command::Undo),
                UiEvent::Style(StyleEdit::Opacity(0.5)),
                UiEvent::Command(Command::Save),
            ]
        );
        assert!(sink.is_empty(), "taking drains the sink");
    }

    #[test]
    fn every_dialog_outcome_carries_the_id_it_was_asked_with() {
        let id = DialogId(7);
        for event in [
            DialogEvent::Confirmed(id),
            DialogEvent::Cancelled(id),
            DialogEvent::Renamed(id, "Engine bay".to_owned()),
        ] {
            assert_eq!(event.id(), id);
        }
    }

    /// The reason [`Secret`] exists at all. `UiEvent` derives `Debug` and `EventSink`
    /// derives it too, so a bare `String` password in a variant is a password in whatever
    /// log line ever formats a sink. Asserted rather than promised, because the promise is
    /// not the mechanism.
    #[test]
    fn nothing_that_holds_a_password_prints_it() {
        let secret = Secret::new("hunter2-and-a-half".to_owned());
        assert_eq!(secret.expose(), "hunter2-and-a-half", "the request still gets it");

        let event = UiEvent::SignInRequested {
            server: "https://boards.example.com/".to_owned(),
            username: "sam".to_owned(),
            password: secret.clone(),
        };
        let mut sink = EventSink::default();
        sink.push(event.clone());

        for printed in [format!("{secret:?}"), format!("{event:?}"), format!("{sink:?}")] {
            assert!(!printed.contains("hunter2"), "{printed}");
            assert!(printed.contains("redacted"), "{printed}");
        }
        // The rest of the event is still useful to a log line: only the password is gone.
        assert!(format!("{event:?}").contains("sam"));
    }

    #[test]
    fn a_border_preset_expands_to_its_three_controls() {
        let edits = StyleEdit::border(Border {
            color: Color::rgb(1, 2, 3),
            width: 4.0,
            style: LineStyle::Dashed,
        });
        assert_eq!(
            edits,
            [
                StyleEdit::BorderColor(Color::rgb(1, 2, 3)),
                StyleEdit::BorderWidth(4.0),
                StyleEdit::BorderStyle(LineStyle::Dashed),
            ]
        );
    }
}
