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
use vellum_agent::{
    AgentRules, DisplayMode, NoteScope, Provider, ProviderChoice, RoleKind, Schedule, Territory,
};
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
    /// Which of the three roles the agent tool will place next.
    ///
    /// A *tool* setting, exactly as [`UiEvent::StickyColorChosen`] is: it names what the next
    /// agent node will be and changes nothing already on the board. Converting an existing
    /// node's role is a different verb and arrives as a style-style edit from the inspector.
    AgentRoleChosen(vellum_agent::RoleKind),
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
    /// One agent-family node's configuration was changed. See [`AgentEdit`].
    ///
    /// Its own variant rather than more [`StyleEdit`] arms because it is not styling: it
    /// writes the node's own token, and the app has to re-encode a
    /// [`vellum_agent::AgentModel`] rather than touch `vellum_doc::Style`.
    ///
    /// ⚠ **It mutates the document, so the app must close an open text session first** —
    /// exactly as it does for [`Self::Style`] and [`Self::Transform`]. `CLAUDE.md`'s
    /// feedback 30 records why: `apply_style` and `apply_transform` each open their own
    /// undo group, a group opened while the caret's is live fails, and that failure breaks
    /// every later grouped operation for the rest of the session.
    Agent(AgentEdit),
    /// The app-wide default display mode for **new** agent nodes — feature 2's second half.
    ///
    /// A preference, so it follows [`Self::AccentChanged`]'s contract: the app persists it,
    /// and every node that never chose a mode of its own moves with it. Nodes that named
    /// one do not, which is the whole reason `AgentModel::display` is an `Option`.
    DefaultDisplayModeChanged(DisplayMode),
    /// *Sign in* was chosen for a provider in Preferences ▸ Providers.
    ///
    /// The chrome does not read or write a credential — it cannot; this crate touches no
    /// files. It asks, the app raises a [`Dialog::SignIn`](crate::Dialog::SignIn), and the
    /// answer comes back as [`DialogEvent::SignedIn`] for the app to write to
    /// `<data-dir>/credentials.json` at mode `0600` and nowhere else
    /// (`docs/07-agent-canvas.md` §8a).
    ProviderSignIn(Provider),
    /// *Forget this key* was chosen. The app removes the stored credential; nothing here
    /// ever held it.
    ProviderForget(Provider),
    /// Show a file to the user — a note's `.md`, an agent's working directory.
    ///
    /// A separate verb from [`Self::OpenLink`] because one hands a path to the file manager
    /// and the other hands a URL to a browser, and because this one is only ever given a
    /// path the app itself put into the model.
    RevealPath(PathBuf),
}

/// A change to one agent-family node's configuration.
///
/// One variant per control, exactly as [`StyleEdit`] is one per control and for the same
/// reason: a partial struct would make "the user chose a provider" and "the user chose a
/// provider and cleared the working directory" the same value, and each of those is one
/// undo step.
///
/// **Single-selection**, all of it. The properties that fold across a selection —
/// running, role kind, display mode, provider — are the four on
/// [`PanelModel`](crate::PanelModel); the rest describe one node. A schedule, a rule
/// cascade and a list of attached files have no shared value, and writing one into forty
/// nodes loses forty configurations in a single gesture.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEdit {
    /// The free-text role label — feature 5. It is also the node's `text()`, so it is
    /// searchable and editable through the paths that already exist.
    Role(String),
    /// Worker, orchestrator or meta. Converting a node rather than placing one, which is
    /// why this is not [`UiEvent::AgentRoleChosen`] — that one names what the *tool* will
    /// place next and changes nothing already on the board.
    Kind(RoleKind),
    /// `None` inherits the board's default. Not the same as naming the same provider.
    Provider(Option<ProviderChoice>),
    /// `None` inherits the app-wide default.
    Display(Option<DisplayMode>),
    /// `None` is the board's project root.
    WorkingDir(Option<String>),
    /// Whether this node gets a git worktree of its own.
    ///
    /// ⚠ **Turning it off is not a removal.** An agent's worktree is never force-removed
    /// with uncommitted work in it (`docs/07-agent-canvas.md` §9), so the app is expected
    /// to confirm — and to be able to refuse — rather than treat this as a plain write.
    Worktree(bool),
    /// An orchestrator's cap on simultaneous sub-agents. Never unbounded: there is no
    /// value of this that means "as many as it likes".
    SpawnCap(u32),
    /// An orchestrator's region, in world units. `None` clears it.
    Territory(Option<Territory>),
    /// The node's own rule layer, whole.
    ///
    /// The **whole** value, for the reason [`UiEvent::BackgroundChanged`] carries a whole
    /// background: the editor writes one value and cannot end up with front matter from one
    /// event and a body from another.
    ///
    /// ⚠ `AgentRules::overrides` is a *cache of what resolution decided*. The app writes it
    /// back from `ResolvedRules::override_names()` after applying; nothing in the chrome
    /// authors it, because a hand-written provenance list is exactly the second source of
    /// truth `docs/07-agent-canvas.md` §7 exists to avoid.
    Rules(AgentRules),
    /// `None` removes the schedule. The agent then runs only when asked, which is every
    /// agent until the user says otherwise.
    Schedule(Option<Schedule>),
    AcceptsMessages(bool),
    Voice(bool),
    /// Detach the context source at this index of [`AgentSummary::context`] — an index,
    /// because that list is the one the panel just drew, from the one selected node.
    ///
    /// [`AgentSummary::context`]: crate::AgentSummary::context
    DropContext(usize),
    /// A note's scope — shared with every agent on the board, or private to one.
    NoteScope(NoteScope),
    /// Whether a file tree shows what git ignores.
    ShowIgnored(bool),
    /// A browser node's address.
    BrowserUrl(String),
    /// Whether *this page* may run an engine. Additional to the app-wide permission, never
    /// a substitute for it.
    BrowserLive(bool),
}

/// An API key on its way from the sign-in field to the app.
///
/// **Nothing ever prints it.** The repository is public, `docs/07-agent-canvas.md` §8a puts
/// credentials in one file and nowhere else, and [`UiEvent`] derives `Debug` — so a bare
/// `String` here would format into every log line, panic message and failing-test dump that
/// touched the event. The manual `Debug` below is what makes that impossible rather than
/// merely discouraged, and [`Self::expose`] is named so that a call site which logs it
/// reads as a mistake.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretKey(String);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "SecretKey(empty)" } else { "SecretKey(«redacted»)" })
    }
}

impl SecretKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Whether anything worth storing was typed.
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// The characters, for the one caller that has to write them to disk.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The buffer the sign-in field types into.
    ///
    /// A masked `TextEdit` needs a `&mut String`, and this is the only way to get one — so
    /// the field cannot be bound to anything the redaction does not cover.
    pub fn buffer_mut(&mut self) -> &mut String {
        &mut self.0
    }
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
    /// A schedule editor was saved. `None` means *remove the schedule*.
    ///
    /// The payload rides the [`DialogId`] rather than arriving as a separate
    /// [`UiEvent::Agent`] beside a bare `Confirmed`, because the id is how the app already
    /// knows *which node* it asked about — it recorded the question against that id when it
    /// raised the dialog. Two events would make the correlation the app's problem twice.
    ScheduleSet(DialogId, Option<Schedule>),
    /// A rules editor was saved, carrying the node's whole own layer.
    RulesSet(DialogId, AgentRules),
    /// A provider sign-in was completed. The key has **never** been printed and is not
    /// printable — see [`SecretKey`].
    SignedIn(DialogId, SecretKey),
}

impl DialogEvent {
    pub const fn id(&self) -> DialogId {
        match self {
            Self::Confirmed(id)
            | Self::Cancelled(id)
            | Self::Renamed(id, _)
            | Self::ScheduleSet(id, _)
            | Self::RulesSet(id, _)
            | Self::SignedIn(id, _) => *id,
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

    pub fn agent(&mut self, edit: AgentEdit) {
        self.push(UiEvent::Agent(edit));
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
            DialogEvent::ScheduleSet(id, None),
            DialogEvent::RulesSet(id, AgentRules::default()),
            DialogEvent::SignedIn(id, SecretKey::new("sk-not-a-real-key")),
        ] {
            assert_eq!(event.id(), id);
        }
    }

    /// The repository is public and [`UiEvent`] derives `Debug`, so a key that formats is a
    /// key that ends up in a log line, a panic message or a failing test's dump.
    ///
    /// This asserts the **absence** of the characters rather than the presence of the
    /// placeholder, because that is the property that matters: a future `Debug` that
    /// printed a prefix "for debugging" would still satisfy a check for the word
    /// *redacted*.
    #[test]
    fn an_api_key_never_formats_itself() {
        let key = SecretKey::new("sk-ant-secret-value");
        let printed = format!("{key:?}");
        assert!(!printed.contains("secret-value"), "{printed}");
        assert!(!printed.contains("sk-ant"), "{printed}");

        // …and inside an event, which is the form that actually reaches a log.
        let event = UiEvent::Dialog(DialogEvent::SignedIn(DialogId(1), key.clone()));
        let printed = format!("{event:?}");
        assert!(!printed.contains("secret-value"), "{printed}");

        assert_eq!(key.expose(), "sk-ant-secret-value", "the app still has to be able to store it");
        assert!(SecretKey::new("   ").is_empty(), "whitespace is not a key");
        assert_eq!(format!("{:?}", SecretKey::default()), "SecretKey(empty)");
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
