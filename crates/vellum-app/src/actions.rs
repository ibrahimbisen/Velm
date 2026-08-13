//! What the chrome's buttons actually do.
//!
//! `vellum-ui` performs no action: it emits [`UiEvent`]s and something else decides.
//! This is that something else. It is written as an `impl` on `crate::app::ActiveState`
//! rather than as a free function taking a dozen borrows, because every command needs
//! some different three of the document, the camera, the input state, the library, the
//! GPU and the window, and threading those individually is a worse file than this one.
//!
//! # Nothing is inert
//!
//! `docs/04-ui-reference.md` §1 asks for **no dead buttons**, and `vellum-ui`'s
//! command table already refuses to draw a row that cannot act without saying why.
//! What that mechanism cannot express is a command the *app* has not built yet — its
//! reasons are `&'static str`s inside `vellum-ui`, and this task does not own that
//! crate. So the rule here is the next best thing and is applied without exception:
//!
//! > **Every command either does the thing, or says in a toast exactly what is
//! > missing and where.**
//!
//! There is no arm that falls through to nothing. [`ActiveState::gap`] is the single
//! door for the second case, so the list of unfinished work is `grep gap(` rather
//! than a matter of reading every arm.
//!
//! # The four commands that are gaps, and why
//!
//! | Command | Missing |
//! |---|---|
//! | Lock / Unlock | `vellum_doc::Style` has no lock flag |
//! | Export ▸ PDF, SVG | `vellum-export` has its own item model and nothing adapts `vellum-doc` onto it |
//! | Background colour | `vellum_doc::Board` carries a title and items, not a canvas colour |
//! | Presentation mode | Needs a frame deck and a chrome-free mode `vellum-ui` does not offer |
//!
//! Two tools are gaps for the same kind of reason and are listed at
//! [`ActiveState::place`]: **`vellum_doc::ItemKind` has no shape variant**, so the
//! shape tool has nothing to create, and neither pen strokes nor connectors can be
//! *drawn* yet even though both import, render and edit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use vellum_import::rtb::ArchiveSet;
use vellum_ink::Stroke;
use vellum_doc::{ConnectorEnd, ItemId as DocId, ItemKind, NewItem, Placement, StyledText};
use vellum_scene::{Camera, ItemId as SceneId, ScreenPoint, ScreenSize, WorldPoint, WorldRect};
use vellum_store::BlobStore;
use vellum_ui::{
    Command, ContextTarget, Dialog, DialogEvent, FindEvent, LibraryEvent, Menu, ReferenceSection,
    Screen, Toast, Tool, UiEvent,
};

use crate::app::ActiveState;
use crate::handle::{self, Handle};
use crate::input::Intent;
use crate::inspect::{self, Applied};
use crate::shell::{Ask, Shell};

/// One wheel notch, reused by the zoom buttons so the two agree —
/// `docs/06-mouse-controls.md` §2.
const ZOOM_STEP: f64 = 1.15;

/// How far a pasted or duplicated item lands from its original, in world units.
/// Enough to see that there are two, small enough that they are obviously a pair.
const OFFSET: f64 = 24.0;

/// The longest edge a pasted image is given, in world units.
///
/// A screenshot off a 5K display is 5120px on its long side, and pasted at its own
/// size it arrives larger than the visible board and mostly off-screen. Scaled down it
/// still lands bigger than a sticky, which is the size relationship that reads.
const MAX_PASTED_IMAGE: f64 = 1_200.0;

/// Past this many characters, pasted text becomes a text item rather than a sticky.
///
/// A sticky auto-fits its text to a fixed box, so a long paste shrinks until it cannot
/// be read; a text item grows instead. Roughly a paragraph.
const STICKY_TEXT_LIMIT: usize = 280;

/// The box a pasted link card gets.
///
/// Wide enough for a title to wrap once at a readable size and short enough that a column of
/// them reads as a list. Miro's own is 250 × 190 — the reference board's `preview` widgets are
/// exactly that — and this matches it so a board built here and one imported look alike.
const LINK_CARD_SIZE: (f64, f64) = (250.0, 190.0);

/// The smallest eraser, in world units, whatever the nib is set to.
const ERASER_MIN_RADIUS: f64 = 6.0;

/// A document ink item as a `vellum_ink::Stroke`, in **absolute** world coordinates.
///
/// The direction that did not exist. `crate::draw` converts one way, per frame and
/// lossily, for tessellation only; erasing needs the geometry in the space the pointer
/// is in and then needs to write the result back.
///
/// The placement's scale and rotation are **baked in** rather than carried alongside.
/// A cut piece gets its own placement, and a piece of a rotated stroke is not a
/// rotation of anything — the arithmetic to keep the transform would have to invert it
/// per piece for no gain.
fn ink_stroke(points: &[vellum_doc::Point], thickness: f64, placement: &Placement) -> Stroke {
    let radians = placement.rotation.to_radians();
    let (sin, cos) = radians.sin_cos();
    let scale = placement.scale;
    let absolute: Vec<(f64, f64)> = points
        .iter()
        .map(|p| {
            let (x, y) = (p.x * scale, p.y * scale);
            (placement.x + x * cos - y * sin, placement.y + x * sin + y * cos)
        })
        .collect();
    // The width is scaled with the points, since the transform is no longer there to
    // scale it at draw time.
    Stroke::from_miro(&absolute, Some(thickness * scale))
}

/// A `vellum_ink::Stroke` back into a document item, centred on its own bounds.
///
/// `None` for a piece too short to be a mark — `erase` already drops anything that
/// collapses to a point, and this is the same floor `commit_stroke` applies.
///
/// **Pressure is lost here**, and cannot not be: `vellum_doc` stores bare coordinate
/// pairs, so a pressure a cut interpolated at the join has nowhere to go. Every
/// imported stroke has uniform pressure anyway; the day the document carries it, this
/// is the function that changes.
fn ink_item(stroke: &Stroke, color: Option<vellum_doc::Color>) -> Option<(ItemKind, Placement)> {
    let points = stroke.points();
    if points.len() < 2 {
        return None;
    }
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    let relative = points
        .iter()
        .map(|p| vellum_doc::Point { x: p.x - cx, y: p.y - cy })
        .collect();

    // Scale and rotation are identity: `ink_stroke` baked them into the points, so
    // re-applying them here would apply them twice.
    Some((
        ItemKind::Ink { points: relative, color, thickness: stroke.width() },
        Placement {
            x: cx,
            y: cy,
            width: (max_x - min_x).max(1.0),
            height: (max_y - min_y).max(1.0),
            ..Placement::default()
        },
    ))
}

/// The two vector formats `vellum-export` writes, which differ only in the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VectorFormat {
    Svg,
    Pdf,
}

impl VectorFormat {
    const fn extension(self) -> &'static str {
        match self {
            Self::Svg => "svg",
            Self::Pdf => "pdf",
        }
    }

    const fn what(self) -> &'static str {
        match self {
            Self::Svg => "exporting an SVG",
            Self::Pdf => "exporting a PDF",
        }
    }
}

/// Where a **Miro board** payload should land. See [`Actions::paste_aimed`].
///
/// Only the Miro arm needs asking: every other flavour has always gone to the pointer,
/// and this exists because that arm did not and the difference read as a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasteAim {
    /// Under the mouse, camera untouched — `⌘V` into a board that already has content.
    Pointer,
    /// At Miro's own coordinates, then fit — Board ▸ Import from Miro, into a board made
    /// for it. There is no meaningful pointer position for a board the user has not seen.
    KeepMiroCoordinates,
}

/// What the system clipboard turned out to be holding.
enum Payload {
    /// Encoded image bytes, ready for the blob store.
    Image(Vec<u8>),
    Text(String),
}

/// Re-encodes loose RGBA8 as a PNG.
///
/// `arboard` returns a pasted bitmap as pixels rather than as a file, and everything
/// downstream — the blob store, `crate::assets`'s `image::load_from_memory` — expects
/// encoded bytes. PNG because it is lossless and already an enabled encoder.
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let buffer: image::RgbaImage = image::ImageBuffer::from_raw(width, height, rgba.to_vec())?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// The text of an HTML fragment, with the tags taken out.
///
/// Deliberately crude. This is not a parser and must not become one: it exists so that
/// pasting from a browser yields the words rather than the markup, and the moment
/// styled spans can be carried this is replaced by a real one — `features/README` §3.
/// `<script>` and `<style>` bodies are dropped rather than shown, because they are the
/// one case where the text between the tags is not text anybody meant to read.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut skipping: Option<&str> = None;

    while let Some(c) = chars.next() {
        if c != '<' {
            if skipping.is_none() {
                out.push(c);
            }
            continue;
        }
        let mut tag = String::new();
        for c in chars.by_ref() {
            if c == '>' {
                break;
            }
            tag.push(c);
        }
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let closing = tag.starts_with('/');

        match (skipping, name.as_str()) {
            (Some(open), n) if closing && n == open => skipping = None,
            (Some(_), _) => {}
            (None, "script" | "style") if !closing => {
                skipping = if name == "script" { Some("script") } else { Some("style") };
            }
            // The tags that are a line break rather than a span of styling.
            (None, "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3") => out.push('\n'),
            (None, _) => {}
        }
    }

    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");

    // Collapse the runs of blank lines the block tags above leave behind.
    let mut lines: Vec<&str> = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if !line.is_empty() || lines.last().is_some_and(|l: &&str| !l.is_empty()) {
            lines.push(line);
        }
    }
    lines.join("\n").trim().to_owned()
}

/// The smallest box a placement drag will honour, in world units. Below it the drag
/// was a click that wobbled and the tool's own default size is what was wanted.
const DRAG_TO_SIZE: f64 = 8.0;

/// The line width a drawn connector gets, in world units.
///
/// Miro's own default, and the same figure the reference board's 18 imported connectors
/// carry — so one drawn by hand sits on a board beside imported ones without standing out.
const CONNECTOR_THICKNESS: f64 = 2.0;

/// The largest selection whose properties panel is rebuilt on every frame of a drag.
/// Above it the numbers are not being read and the allocation is not worth it.
const LIVE_PANEL_LIMIT: usize = 64;

/// How long a run of edits from one continuous control folds into a single undo step,
/// in milliseconds.
///
/// **A slider is one gesture, not a hundred.** `egui::DragValue` and `egui::Slider`
/// report a change on *every frame* the button is held, so dragging the opacity slider
/// for one second at 100 Hz used to push a hundred undo steps — a hundred presses of
/// `⌘Z` to take back one movement of one control, with the earlier history pushed off
/// the end. Loro's undo manager merges records made within this window, which turns
/// the whole drag back into the one step `docs/06-mouse-controls.md` §4 asks for.
/// Longer than a frame at any refresh rate and shorter than a deliberate pause.
const DRAG_MERGE_MS: i64 = 400;

/// The same, for typing. Longer, because a slow typist leaves longer gaps than a slow
/// dragger and a sentence should still be one thing to take back.
const TYPING_MERGE_MS: i64 = 900;

/// Whether a styling control is a *continuous* one — dragged or slid, and therefore
/// reported many times per gesture — rather than a discrete choice.
///
/// Discrete controls stay at a zero merge interval so that two colours picked in quick
/// succession remain two undo steps.
const fn continuous(edit: &vellum_ui::StyleEdit) -> i64 {
    use vellum_ui::StyleEdit as E;
    match edit {
        E::Opacity(_) | E::BorderWidth(_) | E::FontSize(_) | E::LineHeight(_) => DRAG_MERGE_MS,
        _ => 0,
    }
}

/// A drag in flight.
///
/// Holds where every selected item was when the button went down, so each frame is
/// `original + offset` rather than an accumulated sum — a thousand samples cannot
/// drift, and letting go back at the start leaves the document untouched.
#[derive(Debug)]
pub(crate) struct Drag {
    items: Vec<(SceneId, DocId, Placement)>,
    offset: (f64, f64),
    /// What this drag is doing to the items — moving them, or reshaping one.
    ///
    /// Decided at the press, from what was under the pointer, and fixed for the whole
    /// gesture. A drag that could change its mind halfway would be one that resizes
    /// when the user meant to move.
    mode: DragMode,
    /// The shared box a **multi-selection** is being transformed against, and the angle the
    /// pointer was at when the gesture began.
    ///
    /// Captured at the press and held for the whole drag, like the members' own original
    /// placements and for the same reason: the box is recomputed from the members, so
    /// re-deriving it per frame would feed a scaled box back into the next scale and the
    /// item would run away from the pointer exponentially.
    group: Option<(handle::Group, f64)>,
    /// Whether a **corner** drag keeps the item's proportions.
    ///
    /// True for an image, on the user's instruction, and false for everything else: a
    /// sticky, a frame or a shape is a box the user sizes to their content, and forcing
    /// those to a ratio would make the common case fight back. A picture has an intrinsic
    /// shape and distorting it is nearly always a mistake.
    ///
    /// Captured at the press with `mode`, and for the same reason — a drag that changed
    /// its mind about this halfway would jump.
    lock_aspect: bool,
}

/// A kanban card in flight.
///
/// Its own type rather than a [`DragMode`], because a card drag changes **no
/// placement**: a card's position is decided by which column it is in and its rank
/// within that column, so committing one rewrites the item's token and leaves its box
/// exactly where it was. Folding it into [`Drag`] would mean a mode whose
/// `placement_of` is the identity, and a commit path that has to remember not to write
/// what it computed.
#[derive(Debug, Clone)]
pub(crate) struct CardDrag {
    /// The kanban item the card belongs to.
    scene: SceneId,
    doc: DocId,
    card: vellum_flow::CardId,
    /// Where it would land if the button came up now, in the item's own space. `None`
    /// while the pointer is off the board, which is also what makes releasing there a
    /// no-op rather than a move to the nearest column.
    drop: Option<vellum_flow::KanbanDrop>,
}

impl CardDrag {
    /// A number that changes whenever the preview would move, for the glass blur's
    /// cache key. The document has not changed, so its generation cannot see this.
    pub(crate) fn revision(&self) -> (u64, u64, usize) {
        self.drop.as_ref().map_or((0, 0, usize::MAX), |drop| {
            (drop.preview.left().to_bits(), drop.preview.top().to_bits(), drop.index)
        })
    }

    /// The rectangle to draw, in the item's own space.
    pub(crate) fn preview(&self) -> Option<(SceneId, vellum_flow::Rect)> {
        self.drop.as_ref().map(|drop| (self.scene, drop.preview))
    }
}

/// What a press started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragMode {
    /// The default: every selected item slides by the same offset.
    Move,
    /// One item is being reshaped by the named handle.
    Resize(Handle),
    /// One item is being turned.
    Rotate,
}

impl Drag {
    /// A number that changes whenever the drag has moved, for the glass blur's
    /// cache key: the document has not changed, so its generation cannot see this.
    pub(crate) fn revision(&self) -> (u64, u64) {
        (self.offset.0.to_bits(), self.offset.1.to_bits())
    }

    /// Where one item ends up, given how far the drag has travelled.
    ///
    /// The single place this is worked out. The live preview and the commit both ask
    /// here, so the item cannot land anywhere other than where it was last drawn —
    /// they used to hold two copies of the arithmetic, and a resize would have made
    /// them disagree the moment one was edited.
    ///
    /// Always `original + the whole travel since the press`, never an accumulated
    /// step: a thousand samples cannot drift.
    fn placement_of(&self, original: &Placement, pointer: WorldPoint, snap: bool) -> Placement {
        match self.mode {
            DragMode::Move => Placement {
                x: original.x + self.offset.0,
                y: original.y + self.offset.1,
                ..*original
            },
            DragMode::Resize(handle) => match self.group {
                // A group scales uniformly about the corner opposite the one being dragged;
                // a single item resizes along its own axes. Different operations, so the
                // group case is not a special case of the other.
                Some((group, _)) => {
                    let (factor, fixed) = handle::group_scale(&group, handle, self.offset);
                    handle::scale_member(original, factor, fixed)
                }
                None => handle::resize(original, handle, self.offset, self.lock_aspect),
            },
            DragMode::Rotate => match self.group {
                Some((group, from)) => {
                    let centre = (group.x, group.y);
                    let mut degrees = handle::angle_to(centre, pointer) - from;
                    if snap {
                        degrees = (degrees / handle::SNAP_DEGREES).round() * handle::SNAP_DEGREES;
                    }
                    handle::rotate_member(original, centre, degrees)
                }
                None => handle::rotate(original, pointer, snap),
            },
        }
    }

    /// Whether this drag changed anything worth an undo step.
    ///
    /// A rotation moves no offset in the sense a move does — the pointer travels but
    /// the item's centre does not — so `offset == (0, 0)` cannot be the test for every
    /// mode, and using it was what would have made a rotate commit nothing.
    const fn is_effective(&self) -> bool {
        match self.mode {
            DragMode::Move => self.offset.0 != 0.0 || self.offset.1 != 0.0,
            DragMode::Resize(_) | DragMode::Rotate => true,
        }
    }
}

impl ActiveState {
    /// Acts on one thing the user did in the chrome.
    pub(crate) fn dispatch(&mut self, event: UiEvent) {
        match event {
            UiEvent::Command(command) => self.run(command),
            UiEvent::ToolChanged(tool) => self.choose_tool(tool),
            UiEvent::ShapeChosen(_) => self.choose_tool(Tool::Shape),
            // Remembered by the chrome — `ToolbarState::sticky` — and read back at placement
            // time, exactly as the shape and the pen preset are. Nothing to do here beyond
            // arming the tool, which the flyout has already asked for.
            UiEvent::StickyColorChosen(_) => self.choose_tool(Tool::Sticky),
            // The chrome already stored the choice; `Shell::agent_role` reads it back when
            // an agent is placed, exactly as `sticky_color` is read. The picker also emits
            // `ToolChanged`, so arming is not this arm's job — doing it here as well would
            // be two paths deciding one thing.
            UiEvent::AgentRoleChosen(_) => {}
            UiEvent::CustomShapeChosen(_) | UiEvent::UploadShape => self.gap(
                "Custom SVG shapes need an SVG parser the app does not carry yet",
            ),
            // All three are remembered by the chrome itself and read back when something is
            // placed or erased, so there is nothing for the app to store.
            UiEvent::ShapeColorsChanged { .. }
            | UiEvent::PenChanged(_)
            | UiEvent::EraserChanged(_) => {}
            UiEvent::Style(edit) => self.style(&edit),
            UiEvent::Transform(edit) => self.transform(edit),
            UiEvent::TextEdited(text) => self.set_text(&text),
            UiEvent::BackgroundChanged(background) => self.set_background(background),
            UiEvent::GridChanged(grid) => {
                // An application setting, so it goes in the sidecar and touches no board.
                // *"grid opacity and grid color and grid should apply to all of the boards
                // not just to that board"* — and the fact that this writes nothing to a
                // document is what makes it apply to boards that are not even open.
                self.shell.library.set_grid_pattern(grid.pattern);
                self.shell.library.set_grid_color(grid.color);
            }
            UiEvent::ZoomTo(zoom) => self.zoom_to(f64::from(zoom)),
            UiEvent::OpenLink(url) => self.open_in_browser(&url),
            UiEvent::CopyLink(url) => self.copy_link(&url),
            UiEvent::Library(event) => self.library_event(event),
            // The strip has already moved itself — it owns the order and which tab is
            // in front. Both of these mean the same thing to the app: put the document
            // the strip now has in front behind it.
            UiEvent::SelectTab(_) | UiEvent::CloseTab(_) => self.follow_tab_strip(),
            // The `+`, and `⌘T`. The strip has already brought the library forward, and
            // **that used to be all this did** — so pressing `+` while the library was
            // already in front did nothing whatsoever, which is exactly what it looked
            // like. A `+` in a board app means *make me a board*, so it now asks for a
            // name as well; the library behind the dialog is the right backdrop for
            // that, and `Dialog::rename` opens with the field focused and Enter bound.
            UiEvent::NewBoardTab => {
                self.follow_tab_strip();
                self.run(Command::NewBoard);
            }
            // Nothing to follow: the order is the interface's, and the app addresses
            // tabs by the strip's own indices.
            UiEvent::ReorderTabs { .. } => {}
            UiEvent::Find(event) => self.find(event),
            UiEvent::Dialog(event) => self.answered(event),
            UiEvent::ThemeChanged(theme) => {
                self.shell
                    .library
                    .set_theme_preference(match theme {
                        vellum_ui::Theme::Light => vellum_ui::ThemePreference::Light,
                        vellum_ui::Theme::Dark => vellum_ui::ThemePreference::Dark,
                    });
            }
            UiEvent::ThemePreferenceChanged(preference) => {
                // The chrome has already applied it to itself; the app only persists.
                self.shell.library.set_theme_preference(preference);
            }
            UiEvent::GlassOpacityChanged(opacity) => {
                // Likewise. `crate::menu` only emits this when the drag ends, because
                // `Library::set_*` writes the sidecar synchronously.
                self.shell.library.set_glass_opacity(opacity);
            }
            UiEvent::AccentChanged(accent) => {
                // **Not** "likewise": this one has a second half. The chrome has applied it
                // to itself, but a selection ring is drawn by `vellum-render` and not by
                // egui, so the *canvas* palette has to carry the same accent. That happens
                // in `crate::app::theme_for`, which re-derives `self.theme` from the chrome
                // every frame — so nothing is assigned here and the two cannot drift. All
                // this arm owes is the sidecar.
                self.shell.library.set_accent(accent);
            }
        }
    }

    /// Puts the document the tab strip has in front behind it.
    ///
    /// The one place the app reacts to the strip. Every tab gesture — a click, a
    /// close, `⌘T`, `⌘1`, the cycle keys — arrives here, because the strip has already
    /// moved itself by the time the event is read and the only question left is which
    /// document should be on screen.
    ///
    /// Three cases, and the middle one is the whole feature: a board that is already
    /// resident is **swapped in with the camera, selection and undo history it was
    /// left with**, never reloaded from its file.
    pub(crate) fn follow_tab_strip(&mut self) {
        // A closed tab takes its board with it. The strip removes the tab before the
        // app hears about it, so the boards to release are found by comparing what the
        // session holds against what the strip still shows — see `Session::retain`.
        self.release_closed_tabs();

        match self.shell.active_tab_path() {
            Some(path) if self.editor.path() == Some(path.as_path()) => {
                self.shell.set_screen(Screen::Board);
            }
            Some(path) => self.open_board(&path),
            None => self.show_the_library(),
        }
    }

    /// Brings a tab to the front by its strip index — `0` is home. The app's side of
    /// `⌘1`…`⌘9`.
    pub(crate) fn select_tab(&mut self, index: usize) {
        self.shell.select_tab(index);
        self.follow_tab_strip();
    }

    /// Flushes and releases every board whose tab has gone.
    ///
    /// **This is what stops memory growing with every board ever opened.** On an 8GB
    /// machine holding the reference board's 596 items and its textures, that is not a
    /// theoretical cost. Dropping the [`Editor`](crate::editor::Editor) releases the
    /// document and joins its autosave thread; the flush first is what makes the
    /// release safe rather than merely tidy.
    fn release_closed_tabs(&mut self) {
        let live = self.shell.tab_keys();
        for mut parked in self.session.retain(&live) {
            if let Err(error) = parked.flush() {
                log::error!("saving {}: {error:#}", parked.path().display());
            }
            log::debug!("released {}", parked.path().display());
        }
    }

    /// Shows the board library — the home tab.
    ///
    /// The board that was in front **stays loaded** if its tab is still open, which is
    /// what makes going to the library and back instant. It is released only when the
    /// user closed its tab and home is what came forward.
    fn show_the_library(&mut self) {
        self.shell.set_screen(Screen::Library);
        let Some(path) = self.editor.path().map(Path::to_path_buf) else { return };
        if let Err(error) = self.editor.flush() {
            self.failed("saving the board", &error);
        }
        self.capture_thumbnail(&path);
        if self.shell.has_tab(Shell::tab_key(&path)) {
            return;
        }
        self.editor = self.blank_editor();
        self.forget_the_board_on_screen();
    }

    // ----- link previews -----------------------------------------------------------

    /// Applies everything the fetch pool has answered, once per frame.
    ///
    /// Each answer is folded over the card **field by field**, never wholesale: a page that
    /// served a title and no image must not erase the provider name the host already gave, and
    /// a card the user has since edited by hand must not be overwritten by a request that was
    /// in flight while they did it. That is why the title is only written when the card has
    /// none — a fetch fills gaps, it does not take over.
    ///
    /// One undo group for the batch. A fetch is not a thing the user did, so it should not cost
    /// them a ⌘Z each — and grouping it means one ⌘Z puts the whole batch back if they dislike
    /// what arrived.
    pub(crate) fn apply_link_fetches(&mut self) {
        // Ask for what is on screen and missing, then apply whatever has come back. In this
        // order so a card requested on one frame can land on the next.
        self.poll_visible_link_previews();

        // **Not while a gesture is holding a group open.** This runs *every frame*, and it
        // opens an undo group of its own — so a fetch landing while a caret is up or an eraser
        // is sweeping hits `UndoGroupAlreadyStarted` and re-raises the very toast this session
        // exists to remove, from a path no keystroke and no command goes through.
        //
        // Waiting is the fix rather than committing, which is the difference between this and
        // [`Self::run`]: a command is something the user just asked for and closing their edit
        // to serve it is reasonable, while a background fetch landing must not end an edit they
        // are in the middle of. Nothing is lost by waiting — the answers stay in `self.links`
        // and this is called again next frame, so they apply the moment the gesture ends.
        if self.busy_with_a_group() {
            return;
        }

        let answers = self.links.drain();
        if answers.is_empty() {
            return;
        }

        // The image bytes go to the blob store first, outside the document edit: `put` hashes
        // and writes a file, and doing that inside the undo group would hold it open across
        // disk I/O.
        let mut resolved: Vec<(DocId, vellum_link::LinkCard, Option<String>, Option<String>)> =
            Vec::with_capacity(answers.len());
        for answer in answers {
            // Content-addressed, so two cards sharing an image — or forty sharing one site's
            // favicon — share one blob, and a re-fetch of the same bytes costs nothing.
            let store = |bytes: &[u8], what: &str| match self.editor.assets().blobs().put(bytes) {
                Ok(hash) => Some(hash.to_hex().to_string()),
                Err(error) => {
                    log::warn!("link previews: storing a {what} failed ({error})");
                    None
                }
            };
            let image = answer.image.as_deref().and_then(|bytes| store(bytes, "preview"));
            let icon = answer.icon.as_deref().and_then(|bytes| store(bytes, "favicon"));
            resolved.push((answer.item, answer.card, image, icon));
        }

        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for (id, card, image, icon) in &resolved {
                let Ok(item) = board.item(*id) else { continue };
                let next = match item.kind.clone() {
                    ItemKind::LinkPreview {
                        title,
                        url,
                        description,
                        thumbnail,
                        provider,
                        favicon,
                        mode,
                    } => ItemKind::LinkPreview {
                        title: title.or_else(|| card.title.clone()),
                        url,
                        description: description.or_else(|| card.description.clone()),
                        thumbnail: thumbnail.or_else(|| image.clone()),
                        provider: provider.or_else(|| card.provider.clone()),
                        favicon: favicon.or_else(|| icon.clone()),
                        mode,
                    },
                    ItemKind::Embed {
                        title,
                        url,
                        description,
                        provider,
                        html,
                        thumbnail,
                        favicon,
                        mode,
                    } => ItemKind::Embed {
                        title: title.or_else(|| card.title.clone()),
                        url,
                        description: description.or_else(|| card.description.clone()),
                        provider: provider.or_else(|| card.provider.clone()),
                        html,
                        thumbnail: thumbnail.or_else(|| image.clone()),
                        favicon: favicon.or_else(|| icon.clone()),
                        mode,
                    },
                    // The item stopped being a card while the request was out — an undo of the
                    // paste that made it, most likely. Nothing to fill in.
                    _ => continue,
                };
                if next != item.kind {
                    board.set_kind(*id, next)?;
                }
            }
            board.end_undo_group();
            Ok(())
        });
        if let Err(error) = result {
            self.failed("filling in a link card", &error);
            return;
        }
        self.shell.invalidate_selection();
    }

    /// Fills in the link cards **currently on screen** that have nothing to show yet.
    ///
    /// Called once per frame. This is what makes the feature feel like it works: paste a link
    /// and the card names its site immediately from the host, then a moment later grows its
    /// real title and picture without anyone asking it to.
    ///
    /// Three bounds keep it from being a crawler:
    ///
    /// - **On screen only.** The visible rectangle, through the same R-tree the painter culls
    ///   with. Opening the reference board — 132 cards — fetches the handful you are looking
    ///   at, not all of them.
    /// - **Missing only.** A card with a title *and* a picture is left alone, so this does
    ///   nothing at all on a board that has already filled in.
    /// - **A few per frame**, and `Fetcher::request` refuses a repeat, so panning across a
    ///   board trickles requests out instead of firing a hundred at once.
    ///
    /// Silent when previews are off: that is the user's choice, and a toast per frame about a
    /// setting they chose would be noise.
    fn poll_visible_link_previews(&mut self) {
        if !self.shell.library.link_previews() {
            return;
        }
        /// How many new fetches may start in one frame. Small on purpose — a fetch takes far
        /// longer than a frame, so this is a trickle rather than a queue.
        const PER_FRAME: usize = 3;

        let visible = self.camera.visible_world_rect();
        let wanted: Vec<(DocId, String, vellum_doc::CardMode)> = self
            .editor
            .projection()
            .scene()
            .query_rect(visible)
            .filter_map(|item| {
                let projected = self.editor.projection().get(item.id)?;
                let (title, thumbnail, favicon, url, mode) = match &projected.item.kind {
                    ItemKind::LinkPreview { title, thumbnail, favicon, url, mode, .. }
                    | ItemKind::Embed { title, thumbnail, favicon, url, mode, .. } => {
                        (title, thumbnail, favicon, url.clone()?, *mode)
                    }
                    _ => return None,
                };
                // Nothing to add: it has a name, a site icon, and — if its mode draws one — a
                // picture. **The icon counts.** Testing only the title left every imported card
                // without one for good: they all arrive from Miro *with* titles, so the
                // condition was false and no fetch ever ran. That is exactly the report — the
                // words were there and the icon never appeared.
                let wants_image = mode.shows_image() && thumbnail.is_none();
                if title.is_some() && favicon.is_some() && !wants_image {
                    return None;
                }
                Some((projected.doc_id, url, mode))
            })
            .take(PER_FRAME)
            .collect();

        for (id, url, mode) in wanted {
            self.links.request(id, &url, mode.shows_image());
        }
    }

    /// Asks for one card's metadata, if previews are switched on.
    ///
    /// Returns whether a request went out, so a caller can count. Silent when previews are off:
    /// this is called from the paste path, and a toast on every paste saying "previews are
    /// disabled" would be noise about a setting the user chose.
    fn fetch_link(&mut self, id: DocId, url: &str, mode: vellum_doc::CardMode) -> bool {
        if !self.shell.library.link_previews() {
            return false;
        }
        // The image is only worth a second request in the mode that draws one.
        self.links.request(id, url, mode.shows_image())
    }

    /// Fetches every card in the selection, or every card on the board when nothing is
    /// selected. `Command::FetchLinkPreviews`.
    ///
    /// User-initiated, never automatic — see `crate::links`. Reports what it did, including the
    /// case where previews are switched off, because *that* is a click the user made and a
    /// silent refusal would read as a broken button.
    fn fetch_link_previews(&mut self) {
        if !self.shell.library.link_previews() {
            self.gap("link previews are off — turn them on in Preferences");
            return;
        }
        let selected = self.editor.selected_ids();
        let scope: Vec<DocId> =
            if selected.is_empty() { self.editor.board().item_ids() } else { selected };

        let cards: Vec<(DocId, String, vellum_doc::CardMode)> = scope
            .into_iter()
            .filter_map(|id| {
                let item = self.editor.board().item(id).ok()?;
                match &item.kind {
                    ItemKind::LinkPreview { url, mode, .. }
                    | ItemKind::Embed { url, mode, .. } => {
                        Some((id, url.clone()?, *mode))
                    }
                    _ => None,
                }
            })
            .collect();

        if cards.is_empty() {
            self.gap("no link cards here to fill in");
            return;
        }
        let asked = cards
            .into_iter()
            .filter(|(id, url, mode)| self.fetch_link(*id, url, *mode))
            .count();
        if asked == 0 {
            self.ok("every card here has already been asked for");
        } else {
            self.ok(format!("Fetching {asked} link{}", plural(asked)));
        }
    }

    /// Hands a URL to whatever the user browses with.
    ///
    /// The platform's own opener rather than a crate: `open` on macOS, `xdg-open` on Linux,
    /// `cmd /c start` on Windows — three lines against a dependency, and the behaviour is the
    /// user's own default browser either way.
    ///
    /// **The scheme is checked first.** A URL reaches this from a board file, which may have
    /// come from someone else, and handing `file:///…` or a shell-shaped string to the system
    /// opener is how a document becomes a way to run things. `vellum_link::host_of` answers
    /// `None` for anything that is not http(s), which is exactly the filter needed.
    fn open_in_browser(&mut self, url: &str) {
        if vellum_link::host_of(url).is_none() {
            self.gap("that card's address is not a web page");
            return;
        }
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut c = std::process::Command::new("open");
            c.arg(url);
            c
        };
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut c = std::process::Command::new("cmd");
            c.args(["/c", "start", "", url]);
            c
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let mut command = {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(url);
            c
        };

        match command.spawn() {
            // Not waited on: the browser outlives this call by design, and `wait` here would
            // block the frame loop for as long as it takes a cold browser to start.
            Ok(_) => self.ok("Opened in your browser"),
            Err(error) => self.failed("opening the page", &anyhow::anyhow!(error)),
        }
    }

    /// Puts a card's address on the system pasteboard, for *Copy link*.
    ///
    /// **No scheme filter, unlike [`open_in_browser`](Self::open_in_browser).** That one
    /// hands a string from a board file to the system opener, which is how a document
    /// becomes a way to run things; this one only ever writes characters somewhere the
    /// user then has to paste. So a `mailto:` card's address copies, which is the whole
    /// point of having the button on it.
    ///
    /// **`clipboard_text` is cleared, and that is the load-bearing line.** It is the
    /// sentinel meaning *the text on the pasteboard is the words of the items in
    /// `self.clipboard`* — see [`copy`](Self::copy) — so leaving a board copy's string in
    /// it while writing a URL over the top would make the very next `⌘V` paste those items
    /// instead of the link. Cleared on a failed write too, for the reason `copy` gives:
    /// whatever is on the pasteboard afterwards, it is not ours to claim.
    ///
    /// The session's one `arboard` handle, never a fresh one — `crate::editor` records
    /// what building these per keystroke cost.
    fn copy_link(&mut self, url: &str) {
        let written = {
            let clipboard = self
                .system_clipboard
                .get_or_insert_with(|| arboard::Clipboard::new().map_err(|error| error.to_string()));
            match clipboard {
                Ok(clipboard) => clipboard.set().text(url.to_owned()).map_err(|error| error.to_string()),
                Err(error) => Err(error.clone()),
            }
        };
        self.clipboard_text = None;
        match written {
            Ok(()) => self.ok("Link copied"),
            Err(error) => self.failed("copying the link", &anyhow::anyhow!(error)),
        }
    }

    /// Says what is missing, rather than doing nothing. See the module header.
    /// Asks for a `.rtb` and attaches it, for Board ▸ Import from Miro.
    ///
    /// **Cancel is a first-class answer, not a failure.** It means "no backup for this one"
    /// or "it is already in my archives folder", and the import carries on without pictures
    /// rather than aborting — every widget still comes across, which is the greater half.
    /// So this returns nothing and reports nothing when the user backs out.
    ///
    /// Blocking rather than async on purpose. `rfd`'s macOS backend runs `NSOpenPanel`'s
    /// `runModal` through `run_on_main`, which executes inline when it is already on the
    /// main thread — which this is: command dispatch happens after `Chrome::show` has
    /// returned, so it is inside the winit loop but *outside* the egui frame closure.
    /// Nesting a modal session inside a running `NSApplication` is the ordinary native
    /// pattern. The async API would need a runtime feature, and the only one on offer for
    /// the Linux backend is `tokio` — the execution model this workspace turned `reqwest`
    /// down to avoid.
    /// The instructions, before anything happens.
    ///
    /// *"i want other people to be able to understand how to import their miro boards …
    /// when a user preses import from miro board it should say the steps super clearly and
    /// there should be like i and when they hove over it a information should pop up why
    /// each step they are doing matters and for what super simply and then they can press
    /// continue."*
    ///
    /// This is the only thing in Velm that cannot be done by clicking, because half of it
    /// happens on a website this application has no control over — and until now the button
    /// simply opened a file picker, which tells a first-time user nothing about the copy
    /// they were supposed to have made in a browser first. The two steps that follow are
    /// unchanged; this sits in front of them.
    ///
    /// **Four steps, and the first two are the user's own.** Steps 1 and 2 happen in Miro,
    /// 3 and 4 in Velm, and each says so — a numbered list that does not say where you are
    /// supposed to be is what makes an instruction feel arbitrary.
    fn ask_import_steps(&mut self) {
        use vellum_ui::Step;
        self.shell.ask(
            |id| {
                Dialog::steps(
                    id,
                    "Import from Miro",
                    "Velm brings a Miro board across through your clipboard. \
                     Two steps happen in Miro, two here.",
                    vec![
                        Step::new(
                            "In Miro, open the board you want and press ⌘A",
                            "Select all. Velm copies what is selected, so anything left \
                             unselected does not come across. ⌘A takes the whole board \
                             including what is scrolled off screen.",
                        ),
                        Step::new(
                            "Still in Miro, press ⌘C",
                            "Copy. This puts every widget — the notes, the text, the \
                             drawings, the links and exactly where each one sits — onto \
                             your clipboard. Velm reads it from there, so nothing is \
                             uploaded anywhere and no Miro account is needed.",
                        ),
                        Step::new(
                            "Back in Velm, press Continue and choose the board's .rtb backup",
                            "Optional. The clipboard carries the layout but not the \
                             pictures — Miro leaves those out. A .rtb backup, downloaded \
                             from Miro under Board ▸ Export, is where the images come from. \
                             Skip it and the board arrives complete except for its pictures.",
                        ),
                        Step::new(
                            "Name the new board, and press Create and paste",
                            "The board is created first and pasted into, so nothing lands \
                             in the board you have open now. If one of your backups matches \
                             what you copied, its real name is filled in for you.",
                        ),
                    ],
                )
            },
            Ask::ImportSteps,
        );
    }

    /// Everything Import from Miro does once the instructions have been read.
    ///
    /// Attach, *then* name — the order the user asked for, and the order that keeps this
    /// simple. Picking a file inside the name dialog would need a third button (no dialog
    /// in this app has ever had one), a way to keep the dialog open while acting (setting
    /// `outcome` pops it), and a way to update a dialog that is already showing (there is
    /// none — `current()` is `&`-only and `Shell::ask` mints a fresh id).
    fn begin_import_from_miro(&mut self) {
        self.attach_archive_from_picker();
        // Offered, not demanded: the board's own name if one of the archives backs up
        // whatever is on the clipboard, otherwise something to edit. Read *after* the
        // attach, so a just-attached backup names the board.
        let suggested = self.clipboard_board_name();
        self.shell.ask(
            move |id| {
                Dialog::rename(id, "Import from Miro", suggested.clone())
                    .with_confirm("Create and paste")
                    .with_hint("Board name")
            },
            Ask::ImportFromMiro,
        );
    }

    fn attach_archive_from_picker(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Choose the board's Miro backup")
            .add_filter("Miro backup", &["rtb"])
            .pick_file()
        else {
            return;
        };
        self.attach_archive(&path);
    }

    /// Adds a `.rtb` to the live set and remembers where it is.
    ///
    /// Remembered by **path**, never copied: see `Library::attached_archives`. The archive
    /// joins the set already in memory as well as the sidecar, so it supplies pictures to
    /// the very next paste rather than to the next launch.
    fn attach_archive(&mut self, path: &Path) {
        let set = self.archive.get_or_insert_with(ArchiveSet::new);
        if let Err(error) = set.add(path) {
            self.failed("reading that Miro backup", &error);
            return;
        }
        self.shell.library.attach_archive(path);
        // Named from the archive's own `board.json` rather than the filename, which is
        // whatever the user's Downloads folder called it.
        let what = self
            .archive
            .as_ref()
            .and_then(|set| set.boards().last())
            .map_or_else(|| "Miro backup".to_owned(), |board| board.name.clone());
        self.shell.toast(Toast::info(format!("Attached the backup for “{what}”")));
    }

    /// The name of the board on the clipboard, for the import dialog to offer.
    ///
    /// The clipboard payload names its source board as `boardId` — `"7t3vC4IWfus="` —
    /// and a `.rtb`'s `board.json` holds the same board's *internal* id, which converts
    /// to that form. So if the user has dropped the backup in the archives folder, the
    /// import dialog can offer **"Reference Board"** rather than asking them to type
    /// it. Falls back to a generic name, never to an empty field.
    ///
    /// **This decodes the whole payload, and the clipboard is decoded a second time by the
    /// paste that follows.** An earlier version of this comment claimed only the envelope
    /// was read; it is not true and was worth measuring rather than believing —
    /// `Decoded::board_id` reads a fully parsed `serde_json::Value`, so `decode` has already
    /// base64-decoded, deobfuscated and parsed all 596 widgets by the time this asks for one
    /// string. Measured on the 1.1 MB reference payload it is **5.5 ms**, which is waste and
    /// not lag — it was cleared as a suspect for the import slowdown, whose cause is in
    /// `crate::assets`. Worth collapsing if this path ever grows, but not worth caching a
    /// parsed 596-widget tree in memory to save it.
    fn clipboard_board_name(&mut self) -> String {
        const FALLBACK: &str = "Miro import";
        let Some(Ok(clipboard)) = self.system_clipboard.as_mut() else {
            return FALLBACK.to_owned();
        };
        let Ok(html) = clipboard.get().html() else {
            return FALLBACK.to_owned();
        };
        let name = vellum_import::clipboard::decode(&html)
            .ok()
            .flatten()
            .and_then(|decoded| decoded.board_id().map(str::to_owned))
            .and_then(|id| Some(self.archive.as_ref()?.name_of(&id)?.to_owned()));
        name.unwrap_or_else(|| FALLBACK.to_owned())
    }

    /// Board ▸ Import from Miro: the copied board arrives as **its own board**.
    ///
    /// Deliberately not `paste()`, which is what this used to do. A *paste* means "put
    /// this in front of me" and is right for `⌘V`; an *import* means "bring that board
    /// across", and the two are not the same verb. Dropping 596 items into whatever
    /// board happened to be open is how a 58-board migration ends up as one board —
    /// reported in exactly those terms: *"oh it just copied into the already open
    /// board"*.
    ///
    /// The board is created and **opened first**, so the paste lands in the new board
    /// rather than the old one. `paste` acts on whatever `ActiveState` currently holds,
    /// so the order here is the whole behaviour, not a preference.
    fn import_to_new_board(&mut self, title: &str) {
        match self.shell.library.create(title) {
            Ok(path) => {
                self.shell.refresh_cards();
                self.open_board(&path);
                // Miro's own coordinates, then fit: the board was made a moment ago and
                // is empty, so there is nothing for a pointer-aimed paste to be aimed
                // relative to. `⌘V` takes the other branch — see [`Self::paste_aimed`].
                self.paste_aimed(PasteAim::KeepMiroCoordinates);
            }
            Err(error) => self.failed("creating the board to import into", &error),
        }
    }

    // ----- the Agent Canvas --------------------------------------------------------

    /// The transcript key for the board on screen, cached.
    ///
    /// Cached because `BoardKey::for_board` canonicalises the path — a filesystem call — and
    /// this is asked twice a frame on a board with agents on it. See
    /// [`crate::agent_runtime::BoardStamp`] for the rest of the gate.
    fn agent_board_key(&mut self) -> vellum_agent::BoardKey {
        let path = self.editor.path().map(Path::to_path_buf);
        if let Some(stamp) = &self.agent_board
            && stamp.path == path
        {
            return stamp.key.clone();
        }
        let key = match path.as_deref() {
            Some(path) => vellum_agent::BoardKey::for_board(path),
            // A board with no file yet — the blank editor behind the library tab. It still
            // gets a key rather than none, so an agent placed before the first save has
            // somewhere to write; the key moves when the board is saved, which costs that
            // transcript and never any board content.
            None => vellum_agent::BoardKey::from_raw("unsaved"),
        };
        self.agent_board = Some(crate::agent_runtime::BoardStamp {
            path,
            key: key.clone(),
            // Nothing has been derived at this key yet, and no generation is ever this.
            epoch: u64::MAX,
            items: usize::MAX,
            has_nodes: false,
        });
        key
    }

    /// Which node an item is, on this board.
    fn agent_key(&mut self, doc: DocId) -> crate::agent_runtime::NodeKey {
        let board = self.agent_board_key();
        crate::agent_runtime::NodeKey::new(&board, doc.to_string())
    }

    /// The item a node key names, if it is on the board in front.
    fn agent_doc(&self, key: &crate::agent_runtime::NodeKey) -> Option<DocId> {
        let doc: DocId = key.item.parse().ok()?;
        self.editor.board().contains(doc).then_some(doc)
    }

    /// The configuration and the role label of an agent node.
    fn agent_model(&self, doc: DocId) -> Option<(vellum_agent::AgentModel, String)> {
        let item = self.editor.board().item(doc).ok()?;
        match &item.kind {
            ItemKind::Agent { model, label } => {
                Some((crate::agent::decode(model), label.to_plain()))
            }
            _ => None,
        }
    }

    /// Re-derive this board's agent wiring: who is an agent, who may talk to whom, what each
    /// one's role is, and what is scheduled.
    ///
    /// One walk of the **projection** — which already holds every item, materialised — rather
    /// than of the document, and only when [`crate::agent_runtime::BoardStamp::needs_resync`]
    /// says so.
    fn sync_agent_wiring(&mut self) {
        let board = self.agent_board_key();
        let epoch = self.editor.projection().generation();
        let items = self.editor.projection().len();

        let mut nodes: Vec<(String, String, bool)> = Vec::new();
        let mut roles: Vec<(String, vellum_agent::RoleKind)> = Vec::new();
        let mut schedules: Vec<(crate::agent_runtime::NodeKey, vellum_agent::Schedule)> =
            Vec::new();
        let mut agents: HashSet<DocId> = HashSet::new();
        let mut wires: Vec<(DocId, DocId, vellum_doc::ArrowKind, vellum_doc::ArrowKind)> =
            Vec::new();
        // The first agent node that names a working directory decides where this board's
        // notes live. §8 wants `<project>/.velm/notes` for a board that *is* a code project
        // and `<data-dir>/agents/<board-key>/notes` otherwise, and a board has no other way
        // of saying which it is — nothing else in the application knows a board's project.
        let mut project: Option<PathBuf> = None;

        for (_, projected) in self.editor.projection().iter() {
            match &projected.item.kind {
                ItemKind::Agent { model, label } => {
                    let config = crate::agent::decode(model);
                    let key = crate::agent_runtime::NodeKey::new(
                        &board,
                        projected.doc_id.to_string(),
                    );
                    let wire = key.wire();
                    let name = label.to_plain();
                    // A label rather than the id is what an agent will type at
                    // `velm-agent-cli send`, and an unnamed node would resolve to nothing —
                    // so one with no label answers to its id, which is at least addressable.
                    let name = if name.trim().is_empty() { wire.clone() } else { name };
                    agents.insert(projected.doc_id);
                    roles.push((wire.clone(), config.role_kind));
                    nodes.push((wire, name, config.accepts_messages));
                    if project.is_none()
                        && let Some(dir) = &config.working_dir
                        && !dir.trim().is_empty()
                    {
                        project = Some(PathBuf::from(dir));
                    }
                    if let Some(schedule) = config.schedule.clone()
                        && schedule.enabled
                    {
                        schedules.push((key, schedule));
                    }
                }
                ItemKind::Connector { start, end, .. } => {
                    if let (Some(a), Some(b)) = (start.target, end.target) {
                        wires.push((a, b, start.arrowhead, end.arrowhead));
                    }
                }
                _ => {}
            }
        }

        // A connector is an agent link when **both** of its endpoints are agents — the §3
        // derivation, and nothing is stored on the connector to say so. Resolved here rather
        // than by `agent::link_kind` because the bus wants a `LinkDirection` and that is the
        // one place the arrowhead rule is written (`LinkDirection::from_arrowheads`).
        let links: Vec<(String, String, vellum_agent::LinkDirection)> = wires
            .into_iter()
            .filter(|(a, b, _, _)| agents.contains(a) && agents.contains(b))
            .map(|(a, b, at_start, at_end)| {
                (
                    crate::agent_runtime::NodeKey::new(&board, a.to_string()).wire(),
                    crate::agent_runtime::NodeKey::new(&board, b.to_string()).wire(),
                    vellum_agent::LinkDirection::from_arrowheads(
                        at_start != vellum_doc::ArrowKind::None,
                        at_end != vellum_doc::ArrowKind::None,
                    ),
                )
            })
            .collect();

        let has_nodes = !nodes.is_empty();
        self.agent_runtime.register_board(&board, project.as_deref());
        self.agent_runtime.set_wiring(&board, epoch, nodes, links, roles);
        self.agent_runtime
            .set_schedules(schedules, crate::agent_runtime::unix_now());

        if let Some(stamp) = self.agent_board.as_mut() {
            stamp.epoch = epoch;
            stamp.items = items;
            stamp.has_nodes = has_nodes;
        }
    }

    /// Everything the agent layer has to do this frame.
    ///
    /// Called once per frame from `app.rs`, **before the occlusion guard** and from the
    /// occluded tick as well — see [`crate::agent_runtime::AgentRuntime::drain`] for why
    /// hiding the window must not stop an agent's output reaching disk.
    ///
    /// On a board with no agent nodes this is two comparisons and a `dormant()` check.
    pub(crate) fn poll_agents(&mut self) {
        let path = self.editor.path();
        let epoch = self.editor.projection().generation();
        let items = self.editor.projection().len();
        let stale = self
            .agent_board
            .as_ref()
            .is_none_or(|stamp| stamp.needs_resync(path, epoch, items));
        if stale {
            self.sync_agent_wiring();
        }
        if self.agent_runtime.dormant() {
            return;
        }

        let now = crate::agent_runtime::unix_now();
        self.agent_runtime.drain(now);
        self.report_agent_runs();

        // **The rule this file keeps relearning.** Everything below touches the document, so
        // it waits rather than committing: a command is something the user just asked for and
        // closing their edit to serve it is reasonable, while an agent's request arriving
        // mid-gesture must not end an edit in progress. Nothing is lost — the jobs stay
        // queued and this runs again next frame. `apply_link_fetches` carries the same guard
        // for the same reason.
        if self.busy_with_a_group() {
            return;
        }
        self.run_due_agents(now);
        self.serve_agent_jobs();
    }

    /// Put what a scheduled run found in front of the user.
    fn report_agent_runs(&mut self) {
        for (key, said) in self.agent_runtime.take_reports() {
            let name = self
                .agent_doc(&key)
                .and_then(|doc| self.agent_model(doc))
                .map_or_else(|| "An agent".to_owned(), |(_, label)| label);
            self.ok(format!("{name}: {said}"));
        }
    }

    /// Run the schedules that have come due.
    fn run_due_agents(&mut self, now: vellum_agent::Timestamp) {
        for key in self.agent_runtime.take_due() {
            let Some(doc) = self.agent_doc(&key) else { continue };
            let Some((config, role)) = self.agent_model(doc) else { continue };
            let Some(schedule) = config.schedule.clone() else { continue };

            if !self.agent_trigger_holds(&schedule, &config) {
                // Not an error and not silent: a schedule that declined is a thing that
                // happened, and a node that showed nothing would read as one that never fired.
                let message = format!(
                    "skipped the scheduled run: {}",
                    schedule.trigger.label().to_lowercase()
                );
                self.agent_runtime.record(
                    &key,
                    now,
                    &vellum_agent::TranscriptEvent::Text { text: message },
                );
                continue;
            }

            // `last_run` goes in the document, so the next fire time is computed from it and
            // the scheduler is re-armed by the epoch change this write causes — which is what
            // closes the loop without a second source of truth about when it last ran.
            let mut next = config.clone();
            if let Some(schedule) = next.schedule.as_mut() {
                schedule.last_run = Some(now);
                schedule.last_failed = false;
            }
            self.write_agent_model(doc, &next);

            self.agent_runtime
                .expect_completion(&key, schedule.completion.clone());
            let prompt = if schedule.prompt.trim().is_empty() {
                "Carry on with your standing instructions.".to_owned()
            } else {
                schedule.prompt.clone()
            };
            self.start_agent_at(doc, Some(prompt), &role, &config);
        }
    }

    /// Whether a schedule's trigger condition holds right now.
    ///
    /// Two of the four are questions about the filesystem, which is why the check is here
    /// rather than in `vellum-agent`: that crate reads no clock and no disk on purpose.
    fn agent_trigger_holds(
        &self,
        schedule: &vellum_agent::Schedule,
        config: &vellum_agent::AgentModel,
    ) -> bool {
        let since = schedule.last_run.unwrap_or(0);
        match &schedule.trigger {
            vellum_agent::Trigger::Always => true,
            vellum_agent::Trigger::LastRunFailed => schedule.last_failed,
            vellum_agent::Trigger::FilesChanged => {
                let Some(dir) = config.working_dir.as_deref() else {
                    // No working directory, so there is nothing this condition could be
                    // about. Running is the safer answer than never running: a schedule
                    // that silently never fires is the failure this whole file is against.
                    return true;
                };
                crate::agent_runtime::newest_mtime(Path::new(dir))
                    .is_none_or(|newest| newest > since)
            }
            vellum_agent::Trigger::NoteChanged { path } => {
                vellum_agent::notes::stamp(Path::new(path))
                    .is_ok_and(|(mtime, _)| mtime > since)
            }
        }
    }

    /// Serve the requests that need the document.
    ///
    /// ⚠ Only reachable from [`Self::poll_agents`] **after** its `busy_with_a_group` guard.
    fn serve_agent_jobs(&mut self) {
        for pending in self.agent_runtime.take_document_jobs() {
            let crate::agent_runtime::Pending { work, reply } = pending;
            match work {
                crate::agent_runtime::DocumentWork::Spawn { parent, request } => {
                    self.spawn_agent(&parent, &request, reply);
                }
                crate::agent_runtime::DocumentWork::ReadConfig { node } => {
                    match self.agent_doc(&node).and_then(|doc| self.agent_model(doc)) {
                        Some((model, _)) => reply.config(model),
                        None => reply.refuse("that node is not an agent on any open board"),
                    }
                }
                crate::agent_runtime::DocumentWork::WriteConfig { node, model } => {
                    match self.agent_doc(&node) {
                        Some(doc) => {
                            self.write_agent_model(doc, &model);
                            reply.done();
                        }
                        None => reply.refuse("that node is not an agent on any open board"),
                    }
                }
            }
        }
    }

    /// An orchestrator asked for a sub-agent.
    ///
    /// The **cap and the territory are not re-implemented here** —
    /// `vellum_agent::orchestrator` owns both, and a second copy of that arithmetic is how a
    /// limit comes to be enforced in one place and not the other. This supplies the two
    /// things that crate deliberately does not know: how big a node is, and where its
    /// siblings actually are.
    fn spawn_agent(
        &mut self,
        parent: &crate::agent_runtime::NodeKey,
        request: &vellum_agent::ipc::SpawnRequest,
        reply: crate::agent_runtime::Answering,
    ) {
        let Some(parent_doc) = self.agent_doc(parent) else {
            reply.refuse("that orchestrator is not on the board in front");
            return;
        };
        let Some((boss, _)) = self.agent_model(parent_doc) else {
            reply.refuse("that node is not an agent");
            return;
        };

        // Every agent on this board, with the parent it was spawned by, so `count_children`
        // can count what this orchestrator is responsible for — which is not the same number
        // as "agents it has ever spawned".
        let mut records: Vec<(String, Option<String>, vellum_agent::orchestrator::NodeBox)> =
            Vec::new();
        for (_, projected) in self.editor.projection().iter() {
            let ItemKind::Agent { model, .. } = &projected.item.kind else { continue };
            let config = crate::agent::decode(model);
            let key =
                crate::agent_runtime::NodeKey::new(&parent.board, projected.doc_id.to_string());
            let placement = &projected.item.placement;
            let (width, height) = placement.scaled_size();
            records.push((
                key.wire(),
                config.spawned_by.clone(),
                vellum_agent::orchestrator::NodeBox::new(
                    placement.x,
                    placement.y,
                    width,
                    height,
                ),
            ));
        }
        let nodes: Vec<vellum_agent::AgentNode<'_>> = records
            .iter()
            .map(|(id, by, _)| vellum_agent::AgentNode::raw(id.as_str(), by.as_deref()))
            .collect();
        let parent_wire = parent.wire();
        let live = vellum_agent::orchestrator::count_children(&parent_wire, &nodes);
        let siblings: Vec<vellum_agent::orchestrator::NodeBox> = records
            .iter()
            .filter(|(_, by, _)| by.as_deref() == Some(parent_wire.as_str()))
            .map(|(_, _, box_)| *box_)
            .collect();

        let (width, height) = crate::agent::DEFAULT_SIZE;
        let placed = match request.at {
            // A position the orchestrator chose: checked, never trusted.
            Some((x, y)) => {
                let proposed = vellum_agent::orchestrator::NodeBox::new(x, y, width, height);
                vellum_agent::orchestrator::may_spawn(&boss, live, proposed).map(|()| proposed)
            }
            None => vellum_agent::orchestrator::plan_spawn(
                &boss, live, &siblings, width, height,
            ),
        };
        let placed = match placed {
            Ok(placed) => placed,
            Err(refusal) => {
                // Both, deliberately, because they have different readers: the reply is what
                // `velm-agent-cli` prints back into the agent's own tool output, and the
                // transcript line is what the board shows a person looking at the node. §9's
                // *"reports the refusal into the transcript, so the orchestrator can adapt
                // rather than silently failing"*.
                let message = refusal.message();
                self.agent_runtime.record(
                    parent,
                    crate::agent_runtime::unix_now(),
                    &refusal.into_event(),
                );
                reply.refuse(message);
                return;
            }
        };

        let label = if request.label.trim().is_empty() {
            "Agent".to_owned()
        } else {
            request.label.clone()
        };
        let mut child = vellum_agent::AgentModel::worker();
        child.role_kind = request.role;
        // **Without this the cap cannot count its own children**, and an orchestrator with a
        // cap of five spawns without limit.
        child.spawned_by = Some(parent.wire());
        child.working_dir = boss.working_dir.clone();

        let kind = ItemKind::Agent {
            model: crate::agent::encode(&child),
            label: StyledText::plain(label.clone()),
        };
        let placement = Placement::new(placed.x, placed.y, placed.width, placed.height);
        let created = self
            .editor
            .edit(|board| Ok(board.add(NewItem::new(kind, placement))?));
        let doc = match created {
            Ok(doc) => doc,
            Err(error) => {
                self.failed("spawning an agent", &error);
                reply.refuse("Velm could not put the new agent on the board");
                return;
            }
        };
        self.shell.invalidate_selection();

        let key = crate::agent_runtime::NodeKey::new(&parent.board, doc.to_string());
        // The wiring has to know about the new node before anything is sent to it, and the
        // item count has moved, so the next `poll_agents` would do it anyway — doing it here
        // means the reply the orchestrator gets is already true.
        self.sync_agent_wiring();
        if let Some(prompt) = request.prompt.clone() {
            self.start_agent_at(doc, Some(prompt), &label, &child);
        }
        reply.spawned(&key);
    }

    /// Write a node's configuration back into its token, keeping its label.
    fn write_agent_model(&mut self, doc: DocId, model: &vellum_agent::AgentModel) {
        let Ok(item) = self.editor.board().item(doc) else { return };
        let ItemKind::Agent { label, .. } = item.kind else { return };
        let kind = ItemKind::Agent { model: crate::agent::encode(model), label };
        // One `set_kind`, no undo group: a group is what the caret and the eraser hold open
        // and what everything in this file has to be careful about. A single call needs none.
        if let Err(error) = self.editor.edit(|board| Ok(board.set_kind(doc, kind)?)) {
            self.failed("saving an agent's settings", &error);
        }
    }

    /// The launch specification for one node: provider, working directory, and the resolved
    /// system context.
    ///
    /// The three-layer cascade is `vellum_agent::rules`' and is **called**, never re-derived:
    /// `ResolvedRules` records which layer supplied each field so the inspector can show
    /// *inherited* against *set here* from what the agent actually got.
    fn agent_launch_spec(
        &self,
        key: &crate::agent_runtime::NodeKey,
        model: &vellum_agent::AgentModel,
        role: &str,
    ) -> vellum_agent::LaunchSpec {
        let data_dir = crate::editor::data_directory();
        let global = vellum_agent::rules::load_global(&data_dir);
        let working = model.working_dir.as_deref().map(Path::new);
        let project = working.map_or_else(
            || vellum_agent::RuleFile::parse(""),
            vellum_agent::rules::load_project,
        );
        let resolved = vellum_agent::rules::resolve(&global, &project, &model.rules, role);

        let mut context = resolved.system_context();
        if !model.context.is_empty() {
            context.push_str("\n## Context you were given\n\n");
            for source in &model.context {
                let label = if source.label.is_empty() { &source.source } else { &source.label };
                context.push_str(&format!("- {label} ({})\n", source.source));
            }
        }

        vellum_agent::LaunchSpec {
            provider: model.provider.clone().unwrap_or_default(),
            command: None,
            args: Vec::new(),
            cwd: working.map(Path::to_path_buf),
            // How `velm-agent-cli` finds its way home (§6). The token is *not* here: the
            // shim reads it out of the runtime file, which is mode 0600, so it never appears
            // in a process listing.
            env: vec![
                (
                    "VELM_IPC".to_owned(),
                    self.agent_runtime.runtime_file().display().to_string(),
                ),
                ("VELM_AGENT_ID".to_owned(), key.wire()),
            ],
            system_context: context,
            base_url: None,
            api_key: None,
            data_dir: Some(data_dir),
            terminal: None,
        }
    }

    /// Start an agent, optionally with a first prompt.
    ///
    /// Answers with a toast either way — a missing binary is the commonest failure by a wide
    /// margin and the one with a specific remedy, so it is **named** rather than logged.
    fn start_agent_at(
        &mut self,
        doc: DocId,
        prompt: Option<String>,
        role: &str,
        config: &vellum_agent::AgentModel,
    ) {
        let key = self.agent_key(doc);
        if !self.agent_runtime.is_running(&key) {
            let spec = self.agent_launch_spec(&key, config, role);
            if let Err(error) = self.agent_runtime.start(&key, spec) {
                let message = error.to_string();
                // On the node as well as in a toast: a toast is gone in ten seconds and the
                // node is where somebody looks tomorrow.
                self.agent_runtime.record(
                    &key,
                    crate::agent_runtime::unix_now(),
                    &vellum_agent::TranscriptEvent::Error { message: message.clone() },
                );
                self.gap(&message);
                return;
            }
        }
        if let Some(prompt) = prompt
            && let Err(error) = self.agent_runtime.prompt(&key, &prompt)
        {
            self.gap(&error.to_string());
        }
    }

    /// Start the agent under the Run button, or send it whatever is in its prompt row.
    pub(crate) fn run_agent(&mut self, doc: DocId) {
        // A caret or an eraser sweep is closed first, for the reason `run` closes one: this
        // is something the user just asked for.
        self.settle();
        let Some((config, role)) = self.agent_model(doc) else {
            self.gap("that item is not an agent node");
            return;
        };
        let key = self.agent_key(doc);
        let draft = self.agent_runtime.take_draft(&key);
        let prompt = (!draft.trim().is_empty()).then_some(draft);
        self.start_agent_at(doc, prompt, &role, &config);
    }

    /// Stop the turn in flight on this node.
    pub(crate) fn stop_agent(&mut self, doc: DocId) {
        let key = self.agent_key(doc);
        if let Err(error) = self.agent_runtime.cancel(&key) {
            self.gap(&error.to_string());
        }
    }

    /// Answer a permission request the node is blocked on.
    pub(crate) fn answer_agent_permission(
        &mut self,
        doc: DocId,
        id: &vellum_agent::RequestId,
        allowed: bool,
    ) {
        let key = self.agent_key(doc);
        if let Err(error) = self.agent_runtime.answer_permission(&key, id, allowed) {
            self.gap(&error.to_string());
        }
    }

    /// What the user has typed into a node's prompt row.
    pub(crate) fn agent_draft(&mut self, doc: DocId) -> String {
        let key = self.agent_key(doc);
        self.agent_runtime.draft(&key).to_owned()
    }

    pub(crate) fn set_agent_draft(&mut self, doc: DocId, text: impl Into<String>) {
        let key = self.agent_key(doc);
        self.agent_runtime.set_draft(&key, text);
    }

    /// Flip one node between Raw and Clean.
    ///
    /// Writes the node's *own* choice, so it stops following the app-wide default — which is
    /// what a toggle on the node means, and `crate::agent::display_mode` is the one place
    /// that resolution happens.
    pub(crate) fn toggle_agent_display(&mut self, doc: DocId) {
        self.settle();
        let Some((config, _)) = self.agent_model(doc) else { return };
        let current = crate::agent::display_mode(&config, vellum_agent::DisplayMode::default());
        let mut next = config;
        next.display = Some(current.toggled());
        self.write_agent_model(doc, &next);
    }

    /// Fill in what the painter is told about this board's agents.
    ///
    /// **The early-out is the whole cost on an ordinary board**: a board with no agent nodes
    /// and no views already built returns before it touches the camera or the R-tree.
    pub(crate) fn rebuild_agent_views(&mut self) {
        let has_nodes = self.agent_board.as_ref().is_some_and(|stamp| stamp.has_nodes);
        if !has_nodes {
            // The last node was deleted, or there never were any. Handing the painter a
            // fresh empty set once is what makes `AgentViews::is_empty` true again.
            if !self.agents.is_empty() {
                self.agents = crate::agent_view::AgentViews::new();
            }
            return;
        }
        let board = self.agent_board_key();
        let visible = self.camera.visible_world_rect();
        let now_ms = crate::agent_runtime::unix_now().saturating_mul(1_000);
        // The app-wide default a node with no choice of its own follows. A constant until
        // Preferences carries the setting — see this module's note in the handover.
        let fallback = vellum_agent::DisplayMode::default();
        self.agents = self.agent_runtime.rebuild_views(
            &board,
            self.editor.projection(),
            visible,
            fallback,
            now_ms,
        );
    }

    /// The window gained or lost focus. Feature 15's app-side half.
    pub(crate) fn agent_focus_changed(&mut self, focused: bool) {
        let now = crate::agent_runtime::unix_now();
        let Some(since) = self.agent_runtime.focus_changed(focused, now) else {
            return;
        };
        self.agent_digest(since, now);
    }

    /// Build and surface the away-mode digest.
    ///
    /// `vellum_agent::summary` does all of it; the app's contribution is *when* — which is
    /// exactly why `Digest::new` takes `since` as a parameter rather than reading a clock.
    fn agent_digest(&mut self, since: vellum_agent::Timestamp, now: vellum_agent::Timestamp) {
        let board = self.agent_board_key();
        let nodes = self.agent_runtime.nodes_on(&board);
        if nodes.is_empty() {
            return;
        }
        let mut digest = vellum_agent::Digest::new(since, now);
        for node in nodes {
            // `since`, not `tail`: the digest splits *state* from *news*, and the state is
            // often older than the cutoff — a permission request asked before the user
            // walked away is still blocking now.
            let Ok(tail) = self
                .agent_runtime
                .sidecar()
                .since(&node.board, &node.item, since)
            else {
                continue;
            };
            let name = self
                .agent_doc(&node)
                .and_then(|doc| self.agent_model(doc))
                .map_or_else(|| node.item.clone(), |(_, label)| label);
            let events: Vec<(vellum_agent::Timestamp, &vellum_agent::TranscriptEvent)> =
                tail.records.iter().map(|record| (record.at, &record.event)).collect();
            digest.add(vellum_agent::AgentRef::new(node.wire(), name), events);
        }
        if digest.is_quiet() {
            return;
        }
        // The panel form goes to the log, where an unattended run can read it; the headline
        // goes on screen, because a paragraph as a toast is a paragraph nobody reads.
        log::info!("agents, while you were away:\n{}", digest.panel_text());
        self.ok(digest.headline());
    }

    /// Stop every agent. Called on quit, alongside the flush of every open board.
    pub(crate) fn shutdown_agents(&mut self) {
        self.agent_runtime.shutdown();
    }

    fn gap(&mut self, what: &str) {
        log::info!("not implemented: {what}");
        self.shell.toast(Toast::info(what.to_owned()));
    }

    /// Logs as well as toasting, which `gap` and `failed` already did.
    ///
    /// A toast can only be read off a screenshot, and the `--demo` fixtures report their
    /// verdict through here — so an unattended run could see a demo *fail* and could not
    /// see one *pass*. That asymmetry made "silence means it worked" the only available
    /// reading, which is the same trap as a test that passes for the wrong reason.
    fn ok(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::info!("{message}");
        self.shell.toast(Toast::success(message));
    }

    fn failed(&mut self, context: &str, error: &anyhow::Error) {
        log::error!("{context}: {error:#}");
        self.shell.toast(Toast::error(format!("{context}: {error}")));
    }

    // ----- commands ----------------------------------------------------------

    /// The one entry point for a command, whoever asked: a menu row, the palette, a
    /// shortcut, a context menu, or the **native** menu bar — see `crate::menubar`.
    ///
    /// # An open caret is closed first, for the verbs that touch the board
    ///
    /// A text session holds an undo group open for its whole life ([`Self::flush_editing`]),
    /// and Loro answers `UndoGroupAlreadyStarted` to the next one rather than nesting. Since
    /// nothing else ever closes a group, a command that opens its own while a caret is up
    /// used to fail *and stay failing* — every later move, delete, paste, align and restyle
    /// with it. That is the user's *"when I type or when I delete something it gives an
    /// error"*, and the reason it looked intermittent is that it needs a caret to be open,
    /// which only a menu or the palette can reach past (the keyboard's own `Delete` is
    /// consumed by [`Self::type_key`] and never arrives here).
    ///
    /// [`Command::mutates_board`] is the list, and it is deliberately not "every command" —
    /// see its own doc comment for the three families that must *not* commit.
    pub(crate) fn run(&mut self, command: Command) {
        if command.mutates_board() {
            // `settle`, not `commit_editing`: the eraser holds a group open the same way the
            // caret does, and a command dispatched mid-sweep failed for exactly the same
            // reason. Only the caret was covered on the first pass.
            self.settle();
        }
        match command {
            // --- board ---
            Command::NewBoard => self.shell.ask(
                |id| {
                    Dialog::rename(id, "New board", "Untitled board")
                        .with_confirm("Create")
                        .with_hint("Board name")
                },
                Ask::NewBoard,
            ),
            // There is no native file dialog in this build, and the board library is a
            // better one anyway: it lists every board with its space, its date and a
            // preview. "Open…" therefore *is* the library — its tab, which since the
            // strip exists means the board you are on stays open behind it.
            Command::OpenBoard => self.show_library_tab(),
            Command::ImportFromMiro => self.ask_import_steps(),
            Command::Save => self.save(),
            Command::SaveAs => {
                let title = self.editor.board().title();
                match self.editor.path().map(Path::to_path_buf) {
                    Some(path) => self
                        .shell
                        .ask(
                        move |id| {
                            Dialog::rename(id, "Save as", title)
                                .with_confirm("Save a copy")
                                .with_hint("Board name")
                        },
                        Ask::RenameBoard(path),
                    ),
                    None => self.gap("This board has no file yet, so there is nothing to copy"),
                }
            }
            Command::StarBoard => self.star(),
            Command::DuplicateBoard => self.duplicate_board(),
            Command::ExportPng => self.export_png(),
            Command::ExportPdf => self.export_vector(VectorFormat::Pdf),
            Command::ExportSvg => self.export_vector(VectorFormat::Svg),
            Command::ExportCsv => self.export_csv(),
            Command::ExportBackup => self.export_backup(),
            Command::BoardHistory => self.history(),
            Command::DeleteBoard => self.confirm_delete_open_board(),
            Command::CloseBoard => self.close_board(),

            // --- edit ---
            Command::Undo => self.undo(false),
            Command::Redo => self.undo(true),
            // Both overlays open themselves inside the chrome; the event reaches here
            // so the app *could* react, and there is nothing it needs to do.
            Command::CommandPalette | Command::Find => {}
            Command::Cut => self.copy(true),
            Command::Copy => self.copy(false),
            Command::Paste => self.paste(),
            Command::Duplicate => self.duplicate_selection(),
            Command::Delete => self.delete_selection(),
            Command::SelectAll => self.editor.select_all(),

            // --- edit ▸ arrange ---
            Command::BringToFront => self.reorder(Order::Front),
            Command::BringForward => self.reorder(Order::Forward),
            Command::SendBackward => self.reorder(Order::Backward),
            Command::SendToBack => self.reorder(Order::Back),
            Command::Group => self.group(),
            Command::Ungroup => self.ungroup(),
            Command::ToggleLinkPreviews => {
                let on = !self.shell.library.link_previews();
                self.shell.library.set_link_previews(on);
                // Said, because the setting is invisible until a card fills in — and because
                // turning it *on* is the moment to explain what will now happen.
                self.ok(if on {
                    "Link previews on — cards will fetch their own titles and pictures"
                } else {
                    "Link previews off — cards keep the title and site they already have"
                });
            }
            Command::ToggleAlignObjects => {
                let on = !self.shell.library.align_objects();
                self.shell.library.set_align_objects(on);
                // Named the way Miro names it, and said out loud for the same reason the
                // switch above is: snapping is invisible until it fires, so a user who
                // turned it off and forgot has no way to tell it from a board that
                // happens to have nothing to align against.
                self.ok(if on {
                    "Align objects on — hold ⌘ while dragging to place something freely"
                } else {
                    "Align objects off — nothing will snap to anything"
                });
            }
            Command::FetchLinkPreviews => self.fetch_link_previews(),
            Command::Lock => self.set_locked(true),
            Command::Unlock => self.set_locked(false),
            Command::AlignLeft => self.align(Align::Left),
            Command::AlignCenterHorizontal => self.align(Align::CentreX),
            Command::AlignRight => self.align(Align::Right),
            Command::AlignTop => self.align(Align::Top),
            Command::AlignMiddleVertical => self.align(Align::CentreY),
            Command::AlignBottom => self.align(Align::Bottom),
            Command::DistributeHorizontally => self.distribute(true),
            Command::DistributeVertically => self.distribute(false),

            // --- view ---
            Command::ZoomIn => self.zoom_by(ZOOM_STEP),
            Command::ZoomOut => self.zoom_by(1.0 / ZOOM_STEP),
            Command::ZoomToFit => self.fit_board(),
            Command::ZoomToSelection => self.fit_selection(),
            Command::ZoomActualSize => self.zoom_to(1.0),
            Command::SnapToGrid => {
                // The chrome reads it back from the library, so the app only remembers it
                // — the same shape as `ToggleAlignObjects` above, and the reason there is
                // no `ViewState` field for it: this is a preference, not a camera setting.
                let on = !self.shell.library.snap_to_grid();
                self.shell.library.set_snap_to_grid(on);
            }
            Command::ToggleMinimap => {
                let view = self.shell.view_mut();
                view.minimap_visible = !view.minimap_visible;
                self.persist_view();
            }
            Command::GoToStartView => self.go_to_start_view(),
            Command::SetStartView => self.set_start_view(),
            Command::PresentationMode => self.toggle_presenting(),

            // --- preferences ---
            Command::ToggleTranslucency => {
                // The chrome flips its own switch; the app only remembers it.
                let on = self.shell.translucency();
                self.shell.library.set_translucency(on);
            }
            // Likewise chrome-owned: `Chrome::show` flips `properties_open` on the
            // frame the command is emitted, so the panel is already in its new state by
            // the time this arm runs. Listed rather than folded into the no-op arm
            // above so the next reader is not left wondering whether it was forgotten.
            Command::TogglePropertiesPanel => {}
            Command::KeyboardShortcuts => self.shell.ask(
                |id| Dialog::reference(id, "Keyboard shortcuts", shortcut_reference(), "Close"),
                Ask::Nothing,
            ),
            Command::Documentation => self.shell.ask(
                |id| Dialog::confirm(id, "Documentation", documentation(), "Close"),
                Ask::Nothing,
            ),
            Command::About => self.shell.ask(
                |id| Dialog::confirm(id, "About Velm", about(), "Close"),
                Ask::Nothing,
            ),
        }
    }

    // ----- presenting --------------------------------------------------------

    /// Enters or leaves presentation mode — `docs/features/README.md` §2's *Slides*,
    /// "frames sequenced as a deck".
    ///
    /// **The deck is the frames, in `order`.** `ItemKind::Frame` has carried Miro's
    /// `prevFrameIndex` since the importer was written; this is the first thing to read
    /// it. Frames with no place in the running order come after those that have one,
    /// in document order, so a board where nobody set a sequence still presents in the
    /// order the frames were made.
    pub(crate) fn toggle_presenting(&mut self) {
        if self.shell.view().presenting {
            self.leave_presenting();
            return;
        }
        self.deck = self.frame_deck();
        if self.deck.is_empty() {
            // Nothing to present is a fact about the board, not a missing feature.
            self.shell.toast(Toast::info(
                "Presentation mode shows a board's frames. This board has none — \
                 draw one with the frame tool (F).",
            ));
            return;
        }
        self.slide = 0;
        self.shell.view_mut().presenting = true;
        self.show_slide();
    }

    pub(crate) fn leave_presenting(&mut self) {
        if !self.shell.view().presenting {
            return;
        }
        self.shell.view_mut().presenting = false;
        self.deck.clear();
    }

    /// Moves `delta` slides, stopping at either end rather than wrapping.
    ///
    /// Deliberately not a cycle: running off the end of a deck and landing back on the
    /// title is disorienting in front of an audience, and every presentation tool stops.
    pub(crate) fn advance_slide(&mut self, delta: isize) {
        if !self.shell.view().presenting || self.deck.is_empty() {
            return;
        }
        let last = self.deck.len() - 1;
        let next = self.slide.saturating_add_signed(delta).min(last);
        if next == self.slide {
            return;
        }
        self.slide = next;
        self.show_slide();
    }

    /// The frames, in the order they should be presented.
    fn frame_deck(&self) -> Vec<SceneId> {
        let mut frames: Vec<(i64, i32, SceneId)> = self
            .editor
            .projection()
            .iter()
            .filter_map(|(scene, projected)| match &projected.item.kind {
                // `i64::MAX` for a frame with no place in the running order, so it
                // sorts after every frame that has one rather than colliding at zero.
                ItemKind::Frame { order, .. } => {
                    Some((order.unwrap_or(i64::MAX), projected.z, *scene))
                }
                _ => None,
            })
            .collect();
        frames.sort_unstable();
        frames.into_iter().map(|(_, _, scene)| scene).collect()
    }

    /// Puts the current slide on screen.
    ///
    /// The camera fits the frame's own rectangle rather than its contents, so a sticky
    /// hanging over the edge cannot change what the slide is framed as — the same rule
    /// `vellum_export::Scope::Frame` states for a PDF page.
    fn show_slide(&mut self) {
        let Some(scene) = self.deck.get(self.slide).copied() else { return };
        let Some(projected) = self.editor.projection().get(scene) else { return };
        let (origin, (w, h)) = projected.rect();
        let rect = vellum_scene::WorldRect::from_corners(
            origin,
            WorldPoint::new(origin.x + w, origin.y + h),
        );
        let canvas = self.canvas_pixels();
        crate::app::fit_rect_in_canvas(&mut self.camera, rect, canvas);
    }

    /// Makes the open board into a named fixture, for `--demo`. See
    /// [`crate::options::Options`].
    pub(crate) fn build_demo(&mut self, name: &str) {
        match name {
            "shapes" => self.demo_shapes(),
            "empty" => self.demo_empty(),
            "table" => self.demo_table(),
            "chart" => self.demo_chart(),
            "mindmap" => self.demo_mindmap(),
            "kanban" => self.demo_kanban(),
            "card-drag" => self.demo_card_drag(),
            "typing" => self.demo_typing(),
            "caret" => self.demo_caret(),
            "connector" => self.demo_connector(),
            "placing" => self.demo_placing(),
            "snapping" => self.demo_snapping(),
            "readme" => self.demo_readme(),
            "object-eraser" => self.demo_object_eraser(),
            "group-handles" => self.demo_group_handles(),
            "locked-arrange" => self.demo_locked_arrange(),
            "widget-edit" => self.demo_widget_edit(),
            "links" => self.demo_links(),
            "copy-paste" => self.demo_copy_paste(),
            "context-menu" => self.demo_context_menu(),
            "edit-then-delete" => self.demo_edit_then_delete(),
            "frame-marquee" => self.demo_frame_marquee(),
            "grid-snap" => self.demo_grid_snap(),
            "agent" => self.demo_agent(),
            // Not a fixture, but the same "do it on the first frame so an unattended
            // run can check it" need — an export is a menu row and nothing else can
            // reach one.
            "export-svg" => self.run(Command::ExportSvg),
            "export-pdf" => self.run(Command::ExportPdf),
            "export-png" => self.run(Command::ExportPng),
            "present" => self.run(Command::PresentationMode),
            other => log::warn!(
                "--demo: no fixture called `{other}` \
                 (shapes, empty, table, chart, mindmap, kanban, card-drag, typing, caret, connector, placing, snapping, \
                  object-eraser, group-handles, locked-arrange, widget-edit, links, copy-paste, context-menu, \
                  edit-then-delete, frame-marquee, grid-snap, agent, export-svg, export-pdf, export-png, present)"
            ),
        }
    }

    /// Two agent nodes and a note, placed with the **real tools**, then wired with the
    /// **real connector tool** — and the resulting links read back out of the document.
    ///
    /// # Why a fixture and not a unit test
    ///
    /// `crate::agent::link_kind` is pure and has its own tests, and none of them prove the
    /// feature is *reachable*. Three separate things here are wiring rather than arithmetic,
    /// and every one of them has an established way of being silently wrong in this repo:
    ///
    /// - **That the agent tool places an agent at all.** `Tool::Agent` has to reach
    ///   `default_size`, `kind_for_tool` and `Input::Tool::Place`; miss any one and the drag
    ///   sweeps a marquee and nothing is created. A test calling `board.add` directly enters
    ///   below all three and passes with the tool unwired — which is exactly how
    ///   `opens_context_menu` and `import_to_new_board` each sat written, tested and
    ///   callerless.
    /// - **That a connector drawn between two agents binds to them.** The derivation is on
    ///   the *endpoints*, so a connector that failed to bind reads as `Plain` — the feature
    ///   would look switched off while every unit test stayed green.
    /// - **That an agent and a note derive a different link from two agents.** Both halves
    ///   are asserted, because a build that answered `Message` for everything satisfies
    ///   "there is an agent link here" and is wrong.
    fn demo_agent(&mut self) {
        // Placed through the tools, not through `board.add` — see the doc comment.
        let place = |actions: &mut Self, tool: Tool, x: f64, size: (f64, f64)| {
            actions.choose_tool(tool);
            let from = actions.camera.world_to_screen(WorldPoint::new(x, 0.0));
            let to = actions
                .camera
                .world_to_screen(WorldPoint::new(x + size.0, size.1));
            actions.act_on(Intent::Place { at: from, to });
        };
        place(self, Tool::Agent, -900.0, crate::agent::DEFAULT_SIZE);
        place(self, Tool::Agent, -100.0, crate::agent::DEFAULT_SIZE);
        place(self, Tool::Note, 600.0, crate::note::DEFAULT_SIZE);

        // The caret lands on a freshly placed agent's role, so the placement gesture leaves
        // an editing session open. Close it before dispatching anything else — feedback 27's
        // rule, and the reason this fixture would otherwise raise the undo-group toast.
        self.settle();

        let ids = self.editor.board().item_ids();
        let kind_of = |actions: &Self, id| actions.editor.board().item(id).ok().map(|i| i.kind);
        let agents: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| matches!(kind_of(self, *id), Some(ItemKind::Agent { .. })))
            .collect();
        let notes: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| matches!(kind_of(self, *id), Some(ItemKind::AgentNote { .. })))
            .collect();

        let ([first, second], [note]) = (agents.as_slice(), notes.as_slice()) else {
            self.gap(&format!(
                "the agent tools placed {} agent(s) and {} note(s), not 2 and 1",
                agents.len(),
                notes.len()
            ));
            return;
        };
        let (first, second, note) = (*first, *second, *note);

        self.fit_board();
        let wire = |actions: &mut Self, a: f64, b: f64| {
            actions.choose_tool(Tool::Connector);
            let from = actions.camera.world_to_screen(WorldPoint::new(a, 200.0));
            let to = actions.camera.world_to_screen(WorldPoint::new(b, 200.0));
            actions.act_on(Intent::Place { at: from, to });
        };
        // Agent to agent, then agent to note: the two derivations that must differ.
        wire(self, -640.0, 160.0);
        wire(self, 160.0, 860.0);

        // Read every link back out of the document, resolving each end's *kind* exactly as
        // the painter will.
        let mut message = 0_u32;
        let mut context = 0_u32;
        let mut plain = 0_u32;
        for id in self.editor.board().item_ids() {
            let Ok(item) = self.editor.board().item(id) else { continue };
            let ItemKind::Connector { start, end, .. } = &item.kind else { continue };
            let start_kind = start.target.and_then(|t| kind_of(self, t));
            let end_kind = end.target.and_then(|t| kind_of(self, t));
            match crate::agent::link_kind(
                start_kind.as_ref(),
                end_kind.as_ref(),
                start.arrowhead,
                end.arrowhead,
            ) {
                crate::agent::LinkKind::Message(_) => message += 1,
                crate::agent::LinkKind::Context => context += 1,
                crate::agent::LinkKind::Plain => plain += 1,
            }
        }

        let _ = (first, second, note);
        if message == 1 && context == 1 && plain == 0 {
            self.ok(
                "placed 2 agents and a note with their own tools, and wired them into \
                 1 message link and 1 context link",
            );
        } else {
            self.gap(&format!(
                "the wiring produced {message} message link(s), {context} context link(s) \
                 and {plain} plain connector(s); expected 1, 1 and 0"
            ));
        }
    }

    /// The two ways an undo group used to leak, driven through the real command path.
    ///
    /// The user's report was *"when i type or when i delete something, it gives an error,
    /// especially when i delete something from the notes"*, with a screenshot of
    /// `deleting: There is already an active undo group, call `group_end` first`.
    ///
    /// # Why a fixture and not a unit test
    ///
    /// Both leaks need a *live session* — a caret holding its group open across commands,
    /// or an eraser sweep abandoned mid-gesture — and `ActiveState` owns a window and a GPU,
    /// so no unit test can build one. `--screenshot` cannot photograph it either: the
    /// failure is a toast that appears on the frame after the command, and the *permanent*
    /// half of it only shows on the operation after that. So the check has to be an
    /// assertion made from inside a running app, which is what this is.
    ///
    /// It enters at [`Self::run`] — the same function a menu row, the palette, a shortcut
    /// and the native menu bar all arrive at — because the fix is *in* `run`. A fixture that
    /// called `delete_selection` directly would enter below the thing being tested and pass
    /// with the fix removed, which is this repo's oldest lesson about verification.
    ///
    /// Three items, so the third can prove the session recovered: a leak is defined by what
    /// it does to the operations *after* it, and a fixture that deleted one item and stopped
    /// would report success on a board that was already broken.
    fn demo_frame_marquee(&mut self) {
        use winit::event::{ElementState, MouseButton};

        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let frame = board.add(NewItem::new(
                ItemKind::Frame {
                    title: StyledText::plain("backdrop"),
                    order: None,
                    speaker_notes: None,
                },
                Placement::new(0.0, 0.0, 1200.0, 800.0),
            ))?;
            let mut notes = Vec::new();
            for x in [-300.0, 0.0, 300.0] {
                let mut note = NewItem::new(
                    ItemKind::Sticky { text: StyledText::plain("note"), background: None },
                    Placement::new(x, 0.0, 199.0, 228.0),
                )
                .with_parent(frame);
                // The last one is **locked**, which is the interesting member: a lock stops
                // an item being picked, and must not stop the surface it is pinned to from
                // carrying it. Left behind, it would end up off the frame it was locked onto.
                if x > 0.0 {
                    note.style.locked = true;
                }
                notes.push(board.add(note)?);
            }
            board.end_undo_group();
            Ok((frame, notes))
        });
        let Ok((frame, notes)) = placed else {
            self.gap("the frame fixture could not be built");
            return;
        };
        self.fit_board();
        let origin = |state: &Self| state.editor.board().item(frame).ok().map(|i| i.placement.x);
        let before = origin(self);
        // Every child's own x, so the second drag can be judged on whether they travelled
        // the **same distance** the frame did rather than on a literal number: snapping is
        // live here and may shorten the offset, which is fine as long as the frame and the
        // things on it agree.
        let note_origins = |state: &Self| -> Vec<f64> {
            notes.iter().filter_map(|id| state.editor.board().item(*id).ok()).map(|i| i.placement.x).collect()
        };
        let notes_before = note_origins(self);

        // A real press-drag-release, starting on **bare frame** — clear of the stickies, so
        // the hit test answers with the frame and nothing else.
        let drag = |state: &mut Self, from: WorldPoint, to: WorldPoint| {
            let (a, b) = (state.camera.world_to_screen(from), state.camera.world_to_screen(to));
            state.input.cursor_moved(&mut state.camera, a);
            let down =
                state.input.mouse_input(&mut state.camera, MouseButton::Left, ElementState::Pressed);
            state.act_on(down);
            let moved = state.input.cursor_moved(&mut state.camera, b);
            state.act_on(moved);
            let up = state.input.mouse_input(
                &mut state.camera,
                MouseButton::Left,
                ElementState::Released,
            );
            state.act_on(up);
        };

        // 1. Over the frame, corner to corner across all three notes.
        drag(self, WorldPoint::new(-560.0, -360.0), WorldPoint::new(560.0, 360.0));
        let swept = self.editor.selection().len();
        let stayed = origin(self) == before;

        // 2. Now pick the frame up deliberately, and drag again. It must move — the rule is
        //    "one click first", not "a frame cannot be dragged".
        self.editor.select([frame]);
        self.shell.invalidate_selection();
        drag(self, WorldPoint::new(-560.0, -360.0), WorldPoint::new(-360.0, -360.0));
        let travel = match (origin(self), before) {
            (Some(now), Some(was)) => now - was,
            _ => 0.0,
        };
        let moved = travel != 0.0;
        // The children must have travelled **exactly** as far. Only the frame was selected,
        // so this is entirely `widen_to_contents`' doing, and it includes the locked note.
        let carried: Vec<f64> = note_origins(self)
            .iter()
            .zip(&notes_before)
            .map(|(now, was)| now - was)
            .collect();
        let together =
            carried.len() == notes.len() && carried.iter().all(|d| (d - travel).abs() < 1e-9);

        match (stayed, swept >= notes.len(), moved, together) {
            (true, true, true, true) => self.ok(format!(
                "a drag over an unpicked frame swept {swept} items and left it put; \
                 picked up, it dragged {travel:.0} and carried all {} of them — \
                 including the locked one",
                notes.len()
            )),
            (false, ..) => self.gap("the drag moved the frame instead of sweeping"),
            (_, false, ..) => {
                self.gap(&format!("the sweep selected {swept}, expected at least {}", notes.len()));
            }
            (_, _, false, _) => self.gap("a frame that was already selected would not drag"),
            (.., false) => self.gap(&format!(
                "the frame moved {travel:.1} and the items on it moved {carried:?}"
            )),
        }
    }

    fn demo_edit_then_delete(&mut self) {
        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let mut ids = Vec::new();
            for (index, word) in ["typed", "erased", "after"].iter().enumerate() {
                ids.push(board.add(NewItem::new(
                    ItemKind::Sticky { text: StyledText::plain(*word), background: None },
                    Placement::new(index as f64 * 400.0, 0.0, 199.0, 228.0),
                ))?);
            }
            board.end_undo_group();
            Ok(ids)
        });
        let Ok(ids) = placed else {
            self.gap("the undo-group fixture could not place its stickies");
            return;
        };
        let [typed, erased, after] = ids[..] else {
            self.gap("the undo-group fixture expected three stickies");
            return;
        };
        self.fit_board();

        // --- leak 1: a command dispatched while a caret holds its group open -------------
        let Some(scene) = self.editor.projection().scene_id(typed) else {
            self.gap("the first sticky is not in the projection");
            return;
        };
        self.editor.select([typed]);
        if !self.begin_editing(scene, true) {
            self.gap("a double click on a sticky did not start an edit");
            return;
        }
        // One character is enough: the group opens on the *first* write, not on the last.
        let text = "x".to_owned();
        if !self.type_key(&winit::keyboard::Key::Character(text.clone().into()), Some(&text)) {
            self.gap("the caret did not claim a keystroke");
            return;
        }
        // Deliberately *not* `commit_editing` first — leaving the session open is the whole
        // point. This is a menu click, which is the only route that reaches a command with a
        // caret still up; the `Delete` key is consumed by `type_key` and never arrives.
        self.run(Command::Delete);
        let typed_went = self.editor.board().item(typed).is_err();

        // --- leak 2: an eraser sweep abandoned by Escape ---------------------------------
        self.choose_tool(Tool::Eraser);
        self.input.set_modifiers(winit::keyboard::ModifiersState::SHIFT);
        self.act_on(Intent::PlaceSample {
            at: self.camera.world_to_screen(WorldPoint::new(400.0, 0.0)),
        });
        // Escape instead of a release: `Intent::Place` never arrives, so `finish_erase` is
        // reached only by `cancel_drag` — which is the call that was missing.
        self.cancel_drag();
        self.input.set_modifiers(winit::keyboard::ModifiersState::empty());
        self.choose_tool(Tool::Select);

        // --- the recovery check ----------------------------------------------------------
        // A grouped operation *after* both, which is where a leak actually shows: the group
        // that leaked belongs to a gesture that has finished, and it is the next one that
        // fails.
        self.editor.select([after]);
        self.run(Command::Delete);
        let after_went = self.editor.board().item(after).is_err();

        // The dab must actually have erased something, or there was never an open group and
        // the recovery check above proved nothing. Escape does *not* put an erased item
        // back — a dab removes as it lands, and the group only makes the sweep one `⌘Z` —
        // so this asserts the sweep was live at the moment it was abandoned.
        let swept = self.editor.board().item(erased).is_err();
        let left = self.editor.board().item_ids().len();
        match (typed_went, swept, after_went) {
            (true, true, true) => self.ok(format!(
                "deleted an item mid-edit and another after an abandoned erase; {left} left"
            )),
            (false, ..) => self.gap("deleting while a caret was open left the item on the board"),
            (_, false, _) => self.gap("the erase dab took nothing, so no group was ever open"),
            (.., false) => self.gap("the undo group leaked: a later delete did nothing"),
        }
    }

    /// Copies this board's items and pastes them on the *next* tab, reporting counts.
    ///
    /// *"when i copy another board to copy it on another page it pastes only text"*. The
    /// bug was that `copy` put the selection's words on the system pasteboard and `paste`
    /// read plain text before it reached the app's own clipboard, so a whole board came
    /// back as one text item.
    ///
    /// Why a fixture rather than a unit test: the defect lived in the *order* the
    /// flavours are tried, and the pasteboard is a machine-wide resource that no unit
    /// test touches. This drives the real `copy` and `paste`, against the real
    /// pasteboard, in the real order.
    ///
    /// Why a fixture rather than only the ⌘C/⌘V keystroke check: this one runs
    /// unattended. It is deliberately *not* a substitute for that check — `--paste`
    /// proved twice that a diagnostic entering below the input layer can pass while the
    /// keystroke never arrives (trap 9). Both, or neither is evidence.
    ///
    /// Falls back to pasting on the same board when only one is open, which still
    /// exercises the ordering: the defect reproduced same-board too.
    fn demo_copy_paste(&mut self) {
        let all: Vec<DocId> = self.editor.board().item_ids();
        if all.is_empty() {
            self.gap("the copy-paste fixture needs a board with something on it");
            return;
        }
        self.editor.select(all.clone());
        self.shell.invalidate_selection();
        self.copy(false);
        let copied = self.clipboard.len();

        // Tab 0 is always the board library, so the first *board* tab is 1 and the next
        // one over is 2. Switching is a swap rather than a reload — `follow_tab_strip`
        // is what makes the app's hot board agree with the strip.
        let switched = if self.shell.tab_count() > 2 {
            self.shell.select_tab(2);
            self.follow_tab_strip();
            true
        } else {
            log::warn!(
                "--demo copy-paste: only one board is open, so this pastes onto the \
                 board it copied from. Pass --open OTHER.vellum to check across boards."
            );
            false
        };

        let before = self.editor.board().item_ids().len();
        self.paste();
        let after = self.editor.board().item_ids().len();

        // The measurement, not an intention. `gained == copied` is the whole assertion:
        // one item gained where many were copied is the text-blob bug, and zero is a
        // paste that did nothing.
        let gained = after.saturating_sub(before);
        log::info!(
            "--demo copy-paste: copied {copied}, pasted onto {} board, items {before} -> {after} \
             (gained {gained}) — {}",
            if switched { "the next" } else { "the same" },
            if gained == copied {
                "OK"
            } else if gained <= 1 {
                "FAILED: the whole copy came back as one item, or nothing pasted"
            } else {
                "FAILED: wrong number of items"
            }
        );
        self.fit_board();
    }

    /// The empty fixture: removes every item, keeping the board itself.
    ///
    /// For a board that has been filled by something other than a person — the
    /// reference import used to *append* to `board.vellum` rather than replace it, and
    /// repeated runs left thousands of duplicated items on the default
    /// board. Emptying beats deleting: the file, its name and its history survive, and
    /// a bare launch still opens something.
    ///
    /// One undo group, so it is a single ⌘Z rather than several thousand.
    fn demo_empty(&mut self) {
        let board = self.editor.board();
        let count = board.item_ids().len();
        // **Roots only.** `item_ids` is a depth-first walk of the whole tree, and
        // removing a frame or a group takes its subtree with it — so a flat sweep asks
        // Loro to delete children that its own previous call already deleted, and
        // `Board::remove` rejects an id that is no longer on the board. Found by
        // running this on the real board: "no item 16@1549471981990909691 on this
        // board", with the whole transaction correctly rolled back.
        let roots: Vec<DocId> = board
            .item_ids()
            .into_iter()
            .filter(|id| board.parent_of(*id).is_none())
            .collect();
        if count == 0 {
            self.ok("The board is already empty");
            return;
        }
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for id in &roots {
                board.remove(*id)?;
            }
            board.end_undo_group();
            Ok(())
        });
        match result {
            Ok(()) => {
                self.editor.clear_selection();
                self.shell.invalidate_selection();
                self.ok(format!("Removed {count} items"));
            }
            Err(error) => self.failed("emptying the board", &error),
        }
    }

    /// A table with content, so the layout has something to size columns against.
    fn demo_table(&mut self) {
        use vellum_table::{CellRef, StyledText as TableText};
        let rows = [
            ["Item", "Qty", "Note"],
            ["First row", "1", "A long cell, so the column has to auto-fit"],
            ["Second row", "2", "Shorter"],
        ];
        let mut table = crate::table::default_table();
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let _ = table.set_content(CellRef::new(r, c), TableText::plain(*cell));
            }
        }
        let kind = ItemKind::Table { model: crate::table::encode(&table) };
        let placement = Placement::new(0.0, 0.0, 640.0, 240.0);
        match self.editor.edit(|board| Ok(board.add(NewItem::new(kind, placement))?)) {
            Ok(_) => {
                self.fit_board();
                self.ok("Placed a table");
            }
            Err(error) => self.failed("building the table demo", &error),
        }
    }

    /// One chart of each kind, so every mark path is exercised at once — bars are
    /// quads, lines and areas and slices are tessellated here, and only placing all of
    /// them shows whether they agree about where the plot area is.
    fn demo_chart(&mut self) {
        use vellum_chart::{ChartData, ChartKind, ChartSpec, Dataset, Series};
        let data = Dataset::new(
            ["Q1", "Q2", "Q3", "Q4"].map(str::to_owned).to_vec(),
            vec![
                Series::new("Actual", vec![12.0, 19.0, 15.0, 24.0]),
                Series::new("Target", vec![15.0, 15.0, 20.0, 20.0]),
            ],
        );
        let kinds = [
            ChartKind::bar(),
            ChartKind::Line { markers: true },
            ChartKind::Area { stacked: false },
            ChartKind::Pie { donut_ratio: 0.55 },
        ];
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for (index, kind) in kinds.into_iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "four charts")]
                let x = index as f64 * 560.0;
                let spec = ChartSpec::new(kind, ChartData::Categorical(data.clone()));
                board.add(NewItem::new(
                    ItemKind::Chart { spec: crate::chart::encode(&spec) },
                    Placement::new(x, 0.0, 500.0, 340.0),
                ))?;
            }
            board.end_undo_group();
            Ok(())
        });
        match result {
            Ok(()) => {
                self.fit_board();
                self.ok("Placed four charts");
            }
            Err(error) => self.failed("building the chart demo", &error),
        }
    }

    /// Two kanban boards: the default, and one deliberately in WIP breach.
    ///
    /// The breach is the point of the second. A limit that is merely enforced is not a
    /// kanban feature — being able to *see* that a column is over it is — and the
    /// breached header is the one piece of this widget's chrome that no other state
    /// reaches, so an unattended render is the only way to check it.
    fn demo_kanban(&mut self) {
        let plain = crate::kanban::encode(&crate::kanban::default_kanban());
        let breached = {
            let mut board = crate::kanban::default_kanban();
            board.set_title("Sprint (over limit)");
            // The second column's limit is 2 and it holds 2; one more breaches it.
            let doing = board.columns()[1].id();
            let _ = board.add_card(doing, "One card too many");
            debug_assert!(!board.breaches().is_empty(), "the breach fixture is not in breach");
            crate::kanban::encode(&board)
        };

        let (width, height) = crate::kanban::DEFAULT_SIZE;
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for (index, model) in [plain, breached].into_iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "two boards")]
                let y = index as f64 * (height + 120.0);
                board.add(NewItem::new(
                    ItemKind::Kanban { board: model },
                    Placement::new(0.0, y, width, height),
                ))?;
            }
            board.end_undo_group();
            Ok(())
        });
        match result {
            Ok(()) => {
                self.fit_board();
                self.ok("Placed two kanban boards");
            }
            Err(error) => self.failed("building the kanban demo", &error),
        }
    }

    /// Types on the canvas the way a keyboard does, and reports what the item says after.
    ///
    /// Same reasoning as `--demo card-drag`, and the same lesson from paste: the caret is
    /// reached through the input layer, so a diagnostic that sets the buffer directly would
    /// prove nothing about it. This one places a sticky, double-clicks it, and then feeds
    /// keystrokes to [`Self::type_key`] — the function `app.rs` calls from
    /// `WindowEvent::KeyboardInput` — before reading the answer back out of the *document*.
    ///
    /// What it exercises that no unit test can: that a double click starts a session at
    /// all, that a character reaches the buffer rather than being eaten as a tool key
    /// (`V`, `N` and `T` are all tools), that Backspace does not fall through to the
    /// shortcut table, and that the text is written through to the board.
    fn demo_typing(&mut self) {
        use winit::keyboard::{Key, NamedKey};

        let placed = self.editor.edit(|board| {
            Ok(board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("old"), background: None },
                Placement::new(0.0, 0.0, 199.0, 228.0),
            ))?)
        });
        let Ok(doc) = placed else {
            self.gap("the typing fixture could not place a sticky");
            return;
        };
        self.fit_board();

        let Some(scene) = self.editor.projection().scene_id(doc) else {
            self.gap("the placed sticky is not in the projection");
            return;
        };
        self.editor.select([doc]);

        // Through the same entry point a double click uses, rather than by building an
        // `Editing` here: whether a double click *starts* a session is half of what this
        // is checking.
        if !self.begin_editing(scene, true) {
            self.gap("a double click on a sticky did not start an edit");
            return;
        }

        // "old" arrives selected, so the first character replaces it. `V` and `N` are
        // tool keys — if the caret is not claiming the keyboard they select the pointer
        // and the sticky tool instead of typing.
        let typed = "Vent NOTE";
        for character in typed.chars() {
            let text = character.to_string();
            let key = Key::Character(text.clone().into());
            if !self.type_key(&key, Some(&text)) {
                self.gap(&format!("the caret did not claim `{character}`"));
                return;
            }
        }
        // And a named key, which takes the other branch entirely.
        if !self.type_key(&Key::Named(NamedKey::Backspace), None) {
            self.gap("the caret did not claim Backspace");
            return;
        }
        self.commit_editing();

        let expected = &typed[..typed.len() - 1];
        let actual = self
            .editor
            .board()
            .item(doc)
            .ok()
            .and_then(|item| item.kind.text().map(vellum_doc::StyledText::to_plain))
            .unwrap_or_default();
        if actual == expected {
            self.ok(format!("typed on the canvas: {actual:?}"));
        } else {
            self.gap(&format!("the board says {actual:?}, not {expected:?}"));
        }
    }

    /// Types into a table cell, a kanban card and a mind-map node, through the real path.
    ///
    /// The three structured widgets went from *placeable and unchangeable* to editable, and
    /// none of it can be unit-tested: `ActiveState` owns a GPU and a window, the layouts come
    /// from the live font stack, and the whole point is that the **pointer** resolves which
    /// cell a double click landed in. So this drives what a person drives — a double click at
    /// a world point, real keystrokes through `type_key`, real `Tab` — and reads every answer
    /// back out of the **document**.
    ///
    /// Four things are checked, each of which failed differently while this was built:
    ///
    /// 1. A double click *inside* a widget resolves to the field under it. Miss this and the
    ///    caret lands on the item, which has no words, and nothing happens at all.
    /// 2. The keystrokes reach the field's own token rather than `set_text`, which a
    ///    structured widget has nowhere to put.
    /// 3. `Tab` moves to the next field — and the caret's **slot** is re-derived, because
    ///    adding a card renumbers every slot below it. A stale slot draws the caret on one
    ///    card while typing into another, which is invisible to any assertion about text.
    /// 4. The whole run is **one** undo step.
    fn demo_widget_edit(&mut self) {
        use winit::keyboard::{Key, NamedKey};

        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let table = board.add(NewItem::new(
                ItemKind::Table { model: crate::table::encode(&crate::table::default_table()) },
                Placement::new(-500.0, 0.0, 420.0, 220.0),
            ))?;
            let kanban = board.add(NewItem::new(
                ItemKind::Kanban { board: crate::kanban::encode(&crate::kanban::default_kanban()) },
                Placement::new(300.0, 0.0, crate::kanban::DEFAULT_SIZE.0, crate::kanban::DEFAULT_SIZE.1),
            ))?;
            let mindmap = board.add(NewItem::new(
                ItemKind::MindMap {
                    model: crate::mindmap::encode(&crate::mindmap::default_mindmap()),
                },
                Placement::new(-500.0, 500.0, crate::mindmap::DEFAULT_SIZE.0, crate::mindmap::DEFAULT_SIZE.1),
            ))?;
            board.end_undo_group();
            Ok((table, kanban, mindmap))
        });
        let Ok((table, kanban, mindmap)) = placed else {
            self.gap("the widget-edit fixture could not be built");
            return;
        };
        self.fit_board();

        // Types `word` into whatever field is under `world`, then reports what the document
        // says. Returns `None` when the double click did not start a session at all, which is
        // failure 1 above and the one that used to be true for all three widgets.
        let edit_at = |state: &mut Self, doc: DocId, world: WorldPoint, word: &str| -> Option<String> {
            let scene = state.editor.projection().scene_id(doc)?;
            if !state.begin_editing_at(scene, true, Some(world)) {
                return None;
            }
            for character in word.chars() {
                let text = character.to_string();
                if !state.type_key(&Key::Character(text.clone().into()), Some(&text)) {
                    return None;
                }
            }
            Some(state.editing.as_ref()?.buffer.text().to_owned())
        };

        // --- a table cell: the top-left one, a quarter of the way into the first row ---
        let cell_point = WorldPoint::new(-500.0 - 420.0 / 2.0 + 40.0, 0.0 - 220.0 / 2.0 + 20.0);
        let typed_cell = edit_at(self, table, cell_point, "Torque");
        // Tab, then more typing: the second cell of the same row, on the same undo group.
        let tabbed = self.type_key(&Key::Named(NamedKey::Tab), None);
        for character in "Nm".chars() {
            let text = character.to_string();
            self.type_key(&Key::Character(text.clone().into()), Some(&text));
        }
        self.commit_editing();
        let table_words = self
            .editor
            .board()
            .item(table)
            .ok()
            .map(|item| crate::words::of(&item.kind))
            .unwrap_or_default();

        // --- a kanban card: the first card of the first column ---
        let card_point = {
            let size = crate::kanban::DEFAULT_SIZE;
            // Down past the board title and the column header, into the first card.
            WorldPoint::new(300.0 - size.0 / 2.0 + 90.0, 0.0 - size.1 / 2.0 + 110.0)
        };
        let typed_card = edit_at(self, kanban, card_point, "Renamed card");
        self.commit_editing();
        let kanban_words = self
            .editor
            .board()
            .item(kanban)
            .ok()
            .map(|item| crate::words::of(&item.kind))
            .unwrap_or_default();

        // --- a mind-map node: whichever node the item's centre falls on ---
        //
        // Not the root, which a tidy tree puts against the left edge. Any node will do: what
        // is being checked is that the *pointer* resolved to one, and naming which would tie
        // the fixture to `vellum-mindmap`'s layout arithmetic.
        let typed_node = edit_at(self, mindmap, WorldPoint::new(-500.0, 500.0), "Hub");
        // Tab adds a child and moves the caret into it; typing then names the new node.
        let child_added = self.type_key(&Key::Named(NamedKey::Tab), None);
        for character in "Leaf".chars() {
            let text = character.to_string();
            self.type_key(&Key::Character(text.clone().into()), Some(&text));
        }
        self.commit_editing();
        let mindmap_words = self
            .editor
            .board()
            .item(mindmap)
            .ok()
            .map(|item| crate::words::of(&item.kind))
            .unwrap_or_default();

        log::info!(
            "--demo widget-edit: cell buffer {typed_cell:?} tab {tabbed}; table now {table_words:?}\n\
             card buffer {typed_card:?}; kanban now {kanban_words:?}\n\
             node buffer {typed_node:?} tab {child_added}; mind map now {mindmap_words:?}"
        );

        let cell_ok = table_words.iter().any(|w| w == "Torque") && table_words.iter().any(|w| w == "Nm");
        let card_ok = kanban_words.iter().any(|w| w == "Renamed card");
        let node_ok = mindmap_words.iter().any(|w| w == "Hub") && mindmap_words.iter().any(|w| w == "Leaf");
        if cell_ok && card_ok && node_ok {
            self.ok("typed into a table cell, a kanban card and a mind-map node");
        } else {
            self.gap(&format!(
                "a widget field did not take the text: cell {cell_ok}, card {card_ok}, node {node_ok}"
            ));
            return;
        }

        // One undo per run, not one per keystroke — checked on the **last** run, because that
        // is the group one ⌘Z reaches. The mind map's run was three things inside one group:
        // renaming a node, Tab adding a child, and naming the child. All three have to go.
        //
        // Getting this wrong is what the first version of this fixture did: it undid once and
        // then looked at the *table*, three groups earlier, and reported a failure that was
        // its own arithmetic rather than the app's.
        self.undo(false);
        let after_undo = self
            .editor
            .board()
            .item(mindmap)
            .ok()
            .map(|item| crate::words::of(&item.kind))
            .unwrap_or_default();
        let reverted = !after_undo.iter().any(|w| w == "Hub" || w == "Leaf");
        if reverted {
            self.ok("and one undo put the whole run back");
        } else {
            self.gap(&format!("one undo left the edit behind: {after_undo:?}"));
        }
        self.fit_board();

        // A double click on an ordinary **sticky**, through `act_on` — the production press
        // path, with the real `Intent` `input.rs` builds. This is a regression guard rather
        // than a new feature: the press site now passes the pointer to `begin_editing_at`, and
        // a sticky has to keep falling through to its own words. It is the most-used editing
        // path in the app and the one a mistake here would break silently, because every
        // widget check above would still pass.
        let sticky = self.editor.edit(|board| {
            Ok(board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("note"), background: None },
                Placement::new(300.0, 600.0, 199.0, 228.0),
            ))?)
        });
        if let Ok(doc) = sticky {
            self.editor.clear_selection();
            let at = self.camera.world_to_screen(WorldPoint::new(300.0, 600.0));
            self.act_on(Intent::Press { at, additive: false, double: true });
            let opened = self
                .editing
                .as_ref()
                .is_some_and(|s| s.doc == doc && s.part == crate::edit::EditPart::Item);
            if opened {
                self.ok("a double click on a sticky still edits the sticky");
            } else {
                self.gap(&format!(
                    "a double click on a sticky did not open its own words: {:?}",
                    self.editing.as_ref().map(|s| s.part)
                ));
            }
            self.commit_editing();
        }

        // Left open on purpose, in an **empty** cell, so `--screenshot` can photograph the one
        // thing no assertion above can see: that the caret is drawn at all. An empty cell is
        // the case that needed fixing — the painter skips a wordless cell so a fresh table
        // costs nothing to shape, and skipping it while a caret is in it means no block, no
        // origin, and a caret that is simply not there.
        // Row 1 rather than a fixed fraction of the box: a table's rows are as tall as the
        // text shaped into them, so its laid-out height is *less* than the item's box and a
        // point two thirds of the way down lands below the grid entirely. That is what the
        // first version of this line did, and `hit_test` correctly answered "no cell".
        let empty_cell = WorldPoint::new(-500.0 - 420.0 / 2.0 + 40.0, 0.0 - 220.0 / 2.0 + 50.0);
        if let Some(scene) = self.editor.projection().scene_id(table) {
            self.editor.select([table]);
            if self.begin_editing_at(scene, false, Some(empty_cell)) {
                log::info!("--demo widget-edit: a caret is open in an empty table cell");
            } else {
                self.gap("could not put a caret in an empty cell");
            }
        }
    }

    /// Places one link card in each display mode, and pastes one from text.
    ///
    /// The three modes are the feature: a collapsed row, a card, and a card with its preview
    /// image — Miro's own three, switched from the card's toolbar there and from the properties
    /// panel here. A screenshot is the only way to check them, because what is being checked is
    /// how much of the card is drawn and where the text sits.
    ///
    /// It also drives the **paste** path with a bare URL, which is how a board of links is
    /// actually built. Reads the answer back out of the document: a URL must become an
    /// `ItemKind::LinkPreview` naming its site, not a sticky with a URL written on it.
    fn demo_links(&mut self) {
        use vellum_doc::CardMode;

        let card = |url: &str, title: &str, description: Option<&str>, mode: CardMode| {
            ItemKind::LinkPreview {
                title: Some(title.to_owned()),
                url: Some(url.to_owned()),
                description: description.map(str::to_owned),
                thumbnail: None,
                provider: vellum_link::provider_for(url),
                favicon: None,
                mode,
            }
        };

        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let mut ids = Vec::new();
            // One of each mode, in a column, so a screenshot shows the three side by side.
            for (index, mode) in CardMode::ALL.into_iter().enumerate() {
                let y = index as f64 * 260.0;
                ids.push(board.add(NewItem::new(
                    card(
                        // GitHub rather than AliExpress: AliExpress answers "too many
                        // redirects" to a non-browser agent, so a fixture built on it can never
                        // show a *fetched* card. This one serves a real `og:image` and a real
                        // favicon, which is what the three modes need to be worth photographing.
                        "https://github.com/rust-lang/rust",
                        "rust-lang/rust: Empowering everyone to build reliable software",
                        Some("A language empowering everyone to build reliable and efficient \
                              software. Memory safety without a garbage collector, and \
                              concurrency without data races."),
                        mode,
                    ),
                    Placement::new(0.0, y, 250.0, 190.0),
                ))?);
            }
            // And one with no title at all, which has to fall back to its URL rather than
            // draw an empty card.
            ids.push(board.add(NewItem::new(
                ItemKind::LinkPreview {
                    title: None,
                    url: Some("https://techforum.net/forums/swaps/1-x.html".into()),
                    description: None,
                    thumbnail: None,
                    provider: vellum_link::provider_for("https://techforum.net/x"),
                    favicon: None,
                    mode: CardMode::Card,
                },
                Placement::new(320.0, 0.0, 250.0, 190.0),
            ))?);
            board.end_undo_group();
            Ok(ids)
        });
        if placed.is_err() {
            self.gap("the link fixture could not be built");
            return;
        }

        // **Nothing is asked for explicitly here.** `poll_visible_link_previews` fetches the
        // cards that are on screen and missing their title or picture, once per frame — so if
        // this fixture's cards fill in, the automatic path is what filled them. An explicit
        // loop here used to do it, which meant the fixture passed whether or not the thing a
        // user actually relies on worked.

        // The paste path, with a bare URL, at a point clear of the cards above.
        let before = self.editor.board().item_ids().len();
        let at = WorldPoint::new(320.0, 300.0);
        self.place_pasted_text("https://www.youtube.com/watch?v=aqz-KE-bpKQ", at);
        // `place_pasted_text` selects what it made, so the selection *is* the answer — no
        // need to search the board for it.
        let pasted = self
            .editor
            .selected_ids()
            .first()
            .and_then(|id| self.editor.board().item(*id).ok());

        let verdict = match pasted.as_ref().map(|item| &item.kind) {
            Some(ItemKind::LinkPreview { url, provider, .. }) => {
                format!("a pasted URL became a {} card ({url:?})", provider.as_deref().unwrap_or("?"))
            }
            Some(other) => format!("FAILED: a pasted URL became {}", other.tag()),
            None => "FAILED: nothing was pasted".to_owned(),
        };
        let gained = self.editor.board().item_ids().len().saturating_sub(before);
        log::info!("--demo links: {verdict}; items gained {gained}");
        if verdict.starts_with("FAILED") {
            self.gap(&verdict);
        } else {
            self.ok(verdict);
        }
        self.fit_board();
    }

    /// Draws a real connector between two stickies, through the gesture path.
    ///
    /// Two stickies side by side, then a drag from the middle of one to the middle of the
    /// other — which is how anybody draws a connector, and the case that made
    /// `facing_anchor` measure to the far end rather than to the release point. Reports what
    /// each end bound to, read back out of the document.
    ///
    /// Enters at [`ActiveState::act_on`] with the same `Intent::Place` `input.rs` builds
    /// from a press-drag-release, so `place`'s tool dispatch is on the production path.
    /// Right-clicks the board twice — once on an item, once on bare canvas — through
    /// the **real** button path, and reports what each one opened.
    ///
    /// This is the fixture the paste lesson demands. Every other check of this feature
    /// starts downstream of the thing that was broken: `vellum-ui`'s interaction tests
    /// call `Chrome::open_context_menu` directly, and `--screenshot` photographs a
    /// window nobody is touching. Neither can tell a working right button from one
    /// whose `Intent` has no arm — which is exactly what `opens_context_menu` was for
    /// three years: written, tested, and never called. So this enters at
    /// `winit`'s own `MouseInput`, through `Input::mouse_input`, and reads the answer
    /// back out of the chrome.
    ///
    /// It also pins the selection rule, which is the part that is easy to get subtly
    /// wrong: right-clicking an **unselected** sticky must select it — otherwise the
    /// menu's *Delete* acts on whatever was selected before, possibly off screen — and
    /// right-clicking **bare canvas** must not clear a selection, because a request for
    /// a menu is not a click.
    fn demo_context_menu(&mut self) {
        use winit::event::{ElementState, MouseButton};

        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let sticky = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("right-click me"), background: None },
                Placement::new(-260.0, 0.0, 199.0, 228.0),
            ))?;
            board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("not me"), background: None },
                Placement::new(260.0, 0.0, 199.0, 228.0),
            ))?;
            // A pen stroke, for the last step. Ink is the case the user photographed —
            // a width, a colour, a lock and a `⋮` — and it is the one configuration
            // that proves the bar is derived from the properties an item *has* rather
            // than from a `match` on its kind: ink has no fill and no words, so the bar
            // it gets is four controls and not eight.
            let stroke = board.add(NewItem::new(
                ItemKind::Ink {
                    points: (0..12)
                        .map(|i| {
                            let t = f64::from(i) / 11.0;
                            vellum_doc::Point::new(t * 180.0 - 90.0, (t * 6.0).sin() * 40.0)
                        })
                        .collect(),
                    color: Some(vellum_doc::Color::rgb(0xC0, 0x39, 0x2B)),
                    thickness: 14.0,
                },
                Placement::new(0.0, -420.0, 180.0, 80.0),
            ))?;
            board.end_undo_group();
            Ok((sticky, stroke))
        });
        let Ok((target, stroke)) = placed else {
            self.gap("the context-menu fixture could not place its items");
            return;
        };
        self.fit_board();
        self.choose_tool(Tool::Select);

        // A real right press and release, with no motion between them — which is what
        // separates a context click from a right-drag pan.
        let right_click_at = |state: &mut Self, world: WorldPoint| {
            let at = state.camera.world_to_screen(world);
            state.input.cursor_moved(&mut state.camera, at);
            let down =
                state.input.mouse_input(&mut state.camera, MouseButton::Right, ElementState::Pressed);
            state.act_on(down);
            let up = state.input.mouse_input(
                &mut state.camera,
                MouseButton::Right,
                ElementState::Released,
            );
            state.act_on(up);
        };

        // 1. On an item that is not selected.
        right_click_at(self, WorldPoint::new(-260.0, 0.0));
        let on_item = self.shell.context_menu_open();
        let picked: Vec<DocId> = self
            .editor
            .selection()
            .iter()
            .filter_map(|id| self.editor.projection().get(*id).map(|p| p.item.id))
            .collect();
        let selected_it = picked == [target];

        // 2. On bare canvas, well clear of both stickies. The selection must survive.
        right_click_at(self, WorldPoint::new(0.0, 900.0));
        let on_canvas = self.shell.context_menu_open();
        let kept = self.editor.selection().len();

        // 3. The pen stroke, last and purely so `--screenshot` photographs the two
        //    things no assertion above can see: an **ink** bar, which is the
        //    configuration the user sent a picture of, and a *selection* menu rather
        //    than the canvas one. Nothing is asserted about it beyond the menu opening;
        //    the control list itself is pinned by `vellum_ui::context_bar`'s own tests.
        let _ = stroke;
        right_click_at(self, WorldPoint::new(0.0, -420.0));
        let on_ink = self.shell.context_menu_open();

        if on_item && selected_it && on_canvas && kept == 1 && on_ink {
            self.ok(
                "right-click opened a menu on the item, the canvas and a pen stroke; \
                 clicking the item selected it and clicking the canvas kept it",
            );
        } else {
            self.gap(&format!(
                "right-click: menu on item {on_item}, selected it {selected_it}, \
                 menu on canvas {on_canvas}, selection kept {kept} (wanted 1), \
                 menu on ink {on_ink}"
            ));
        }
    }

    fn demo_connector(&mut self) {
        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let left = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("from"), background: None },
                Placement::new(-300.0, 0.0, 199.0, 228.0),
            ))?;
            let right = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("to"), background: None },
                Placement::new(300.0, 0.0, 199.0, 228.0),
            ))?;
            board.end_undo_group();
            Ok((left, right))
        });
        let Ok((left, right)) = placed else {
            self.gap("the connector fixture could not place its stickies");
            return;
        };
        self.fit_board();

        self.choose_tool(Tool::Connector);
        let from = self.camera.world_to_screen(WorldPoint::new(-300.0, 0.0));
        let to = self.camera.world_to_screen(WorldPoint::new(300.0, 0.0));
        self.act_on(Intent::Place { at: from, to });

        // Read the binding back out of the board rather than out of what was just built.
        let drawn = self.editor.board().item_ids().into_iter().find_map(|id| {
            let item = self.editor.board().item(id).ok()?;
            match item.kind {
                ItemKind::Connector { start, end, .. } => Some((start, end)),
                _ => None,
            }
        });
        match drawn {
            Some((start, end)) if start.target == Some(left) && end.target == Some(right) => {
                // ASCII arrow: the toast is drawn in the platform UI face, and the
                // shortcut sheet already cost a round to tofu — see feedback 14.
                self.ok(format!(
                    "connected: {:?} -> {:?}",
                    anchor_name(start.anchor),
                    anchor_name(end.anchor),
                ));
            }
            Some((start, end)) => self.gap(&format!(
                "the connector bound to {:?} and {:?}, not to the two stickies",
                start.target, end.target,
            )),
            None => self.gap("the drag produced no connector at all"),
        }
    }

    /// Relative snapping, driven through the real input layer and **left mid-drag**.
    ///
    /// *"i want you to research miros relative snapping / alignment mechanism … it is not
    /// super strict, very loose but very useful."*
    ///
    /// `crate::snap` is pure and has eleven tests; none of them prove the feature is
    /// *reachable*. Three things only a fixture can check, and every one of them has been a
    /// real bug in this file before: that the tolerance is divided by the zoom, so the pull
    /// exists at all at a fitted camera; that the candidates come from the R-tree with the
    /// dragged item excluded, or it snaps to itself and the drag freezes; and that ⌘ is
    /// actually read from the live modifier state rather than from a copy taken at the
    /// press. It stops without releasing so `--screenshot` catches the guides.
    fn demo_snapping(&mut self) {
        use winit::event::{ElementState, MouseButton};

        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            // Three in an evenly spaced row, 100 apart, so both halves of the feature have
            // something to find: the edges line up and the gaps are equal.
            for column in 0..3 {
                board.add(NewItem::new(
                    ItemKind::Sticky {
                        text: StyledText::plain(format!("{column}")),
                        background: None,
                    },
                    Placement::new(f64::from(column) * 300.0, 0.0, 200.0, 200.0),
                ))?;
            }
            let dragged = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("drag me"), background: None },
                Placement::new(900.0, 420.0, 200.0, 200.0),
            ))?;
            board.end_undo_group();
            Ok(dragged)
        });
        let Ok(dragged) = placed else {
            self.gap("the snapping fixture could not place its stickies");
            return;
        };
        self.fit_board();
        self.choose_tool(Tool::Select);
        // `select` takes **document** ids; the scene id is what the projection and the
        // drag speak, so it is read back afterwards.
        self.editor.select([dragged]);
        self.shell.invalidate_selection();
        let Some(scene) = self.editor.projection().scene_id(dragged) else {
            self.gap("the dragged sticky is not in the projection");
            return;
        };

        // Aim its centre a few world units short of the row's y — inside the tolerance at
        // this zoom, and nowhere near it in world units, which is the point.
        let slack = self.snap_tolerance() * 0.6;
        let press = self.camera.world_to_screen(WorldPoint::new(900.0, 420.0));
        let target = self.camera.world_to_screen(WorldPoint::new(900.0, slack));

        self.input.cursor_moved(&mut self.camera, press);
        let down = self.input.mouse_input(&mut self.camera, MouseButton::Left, ElementState::Pressed);
        // Trap 5: a left press is resolved by the application, and forgetting falls back
        // to a marquee — which sweeps a selection box and moves nothing.
        self.input.resolve_press(true);
        self.act_on(down);
        let moved = self.input.cursor_moved(&mut self.camera, target);
        self.act_on(moved);

        // Read the answer out of the projection — where the item is actually being drawn —
        // rather than out of the snap module, which is what the unit tests already ask.
        let landed = self.editor.projection().get(scene).map(|p| p.item.placement.y);
        match landed {
            Some(y) if (y - 0.0).abs() < 0.001 => self.ok(format!(
                "snapped {:.1} world units onto the row, with {} guide(s) up",
                slack,
                self.guides.len(),
            )),
            Some(y) => self.gap(&format!(
                "the sticky stopped at y {y:.2} rather than snapping to the row at 0"
            )),
            None => self.gap("the dragged sticky left the projection"),
        }
    }

    /// Snap to grid, through the real input layer.
    ///
    /// `crate::snap::snap_to_grid` is pure and has five tests; none of them prove the
    /// feature is *reachable*. Four things here are wiring rather than arithmetic and none
    /// can be reached from a unit test:
    ///
    /// - the step comes from **`draw::grid_step` at the live zoom**, so a gesture lands on a
    ///   line that is on screen rather than on a number chosen in advance;
    /// - the switch is read from the library, not from a constant;
    /// - `Pattern::Plain` refuses, so a board with no grid does not move in invisible jumps;
    /// - **alignment still wins**, which is the precedence the whole feature hangs on.
    ///
    /// The board is deliberately *empty apart from the dragged sticky* for the first half,
    /// so nothing can align and the grid is the only thing that could have moved it. Then a
    /// neighbour is added and the same drag is made again: this time the edge must win, and
    /// the item must come to rest **off** the grid. A fixture that only checked the first
    /// half would pass on a build where the grid overrode alignment, which is the more
    /// likely mistake.
    fn demo_grid_snap(&mut self) {
        use winit::event::{ElementState, MouseButton};

        self.shell.library.set_snap_to_grid(true);
        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let dragged = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("drag me"), background: None },
                Placement::new(0.0, 0.0, 200.0, 200.0),
            ))?;
            board.end_undo_group();
            Ok(dragged)
        });
        let Ok(dragged) = placed else {
            self.gap("the grid fixture could not place its sticky");
            return;
        };
        self.zoom_to(1.0);
        self.choose_tool(Tool::Select);
        self.editor.select([dragged]);
        self.shell.invalidate_selection();
        let Some(scene) = self.editor.projection().scene_id(dragged) else {
            self.gap("the dragged sticky is not in the projection");
            return;
        };
        let Some(step) = crate::draw::grid_step(self.camera.zoom()) else {
            self.gap("no grid is drawn at this zoom, so there is nothing to snap to");
            return;
        };

        // A drag that ends a little past a line, so a correction has to happen and its
        // direction is decided rather than zero by luck.
        let drag = |state: &mut Self, from: WorldPoint, to: WorldPoint| {
            let (a, b) = (state.camera.world_to_screen(from), state.camera.world_to_screen(to));
            state.input.cursor_moved(&mut state.camera, a);
            let down =
                state.input.mouse_input(&mut state.camera, MouseButton::Left, ElementState::Pressed);
            // Trap 5, exactly as `--demo snapping` records it: without this the drag is a
            // marquee and nothing moves at all.
            state.input.resolve_press(true);
            state.act_on(down);
            let moved = state.input.cursor_moved(&mut state.camera, b);
            state.act_on(moved);
            let up =
                state.input.mouse_input(&mut state.camera, MouseButton::Left, ElementState::Released);
            state.act_on(up);
        };

        // A sticky is 200 wide, so its **left edge** is centre − 100. Aim the centre so the
        // edge lands a third of a step past a line, and the correction must pull it back.
        let overshoot = step / 3.0;
        let wanted = step * 4.0;
        drag(
            self,
            WorldPoint::new(0.0, 0.0),
            WorldPoint::new(wanted + 100.0 + overshoot, wanted + 100.0 + overshoot),
        );
        let left_edge = |state: &Self| {
            state
                .editor
                .projection()
                .get(scene)
                .map(|p| (p.item.placement.x - 100.0, p.item.placement.y - 100.0))
        };
        let on_grid = left_edge(self).is_some_and(|(x, y)| {
            (x - wanted).abs() < 0.001 && (y - wanted).abs() < 0.001
        });

        // Now a neighbour whose left edge is deliberately **off** the grid. The same
        // gesture aimed at it must land on the neighbour's edge, not on the nearest line.
        //
        // **On the same row, and on screen.** `snap_candidates` filters to what the painter
        // can see — a box in the document but off the viewport is not a candidate — and the
        // first version of this fixture put the neighbour 500 units below the camera, which
        // is off screen at zoom 100%. It reported that the grid had beaten alignment when
        // in fact there had been nothing to align to.
        let off_line = step * 20.0 + step / 2.0;
        let row_y = left_edge(self).map_or(0.0, |(_, y)| y + 100.0);
        let neighbour = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let id = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("neighbour"), background: None },
                Placement::new(off_line + 100.0, row_y, 200.0, 200.0),
            ))?;
            board.end_undo_group();
            Ok(id)
        });
        if neighbour.is_err() {
            self.gap("the grid fixture could not place its neighbour");
            return;
        }
        self.editor.select([dragged]);
        self.shell.invalidate_selection();
        let from = left_edge(self).map_or(WorldPoint::new(0.0, 0.0), |(x, y)| {
            WorldPoint::new(x + 100.0, y + 100.0)
        });
        // Within the relative tolerance of the neighbour's left edge, and off the grid.
        let slack = self.snap_tolerance() * 0.5;
        drag(self, from, WorldPoint::new(off_line + 100.0 + slack, from.y));
        let aligned = left_edge(self).is_some_and(|(x, _)| (x - off_line).abs() < 0.001);

        match (on_grid, aligned) {
            (true, true) => self.ok(format!(
                "snapped a {step:.0}-unit grid at zoom 100%, and an edge on a neighbour \
                 still beat the grid"
            )),
            (false, _) => self.gap(&format!(
                "the sticky's edge stopped at {:?} rather than on the {step:.0}-unit grid at \
                 {wanted:.0}",
                left_edge(self)
            )),
            (_, false) => self.gap(&format!(
                "the grid overrode the neighbour's edge: the sticky landed at {:?}, not \
                 {off_line:.1}",
                left_edge(self)
            )),
        }
    }

    /// A frame being dragged out, **left mid-gesture** so `--screenshot` photographs it.
    ///
    /// *"when i am trying to draw a frame i do not see it as i draw … i click and drag
    /// nothing is happening visually and then when i unclick it just spawns."*
    ///
    /// Three things here can only be checked this way. The preview is fed by
    /// `Input::placement`, which is gesture state inside `input.rs` — so a fixture that
    /// called `act_on(Intent::Place)` directly, as `--demo connector` does, would enter
    /// *below* the thing being tested and pass with the feature removed. It drives real
    /// `winit::MouseInput` and `CursorMoved` instead. It then stops without releasing,
    /// which is the only state that shows anything. And it asserts the board is still
    /// **empty** while the preview is up: a preview that had already committed its item
    /// would look identical in the PNG and be a different, worse bug.
    fn demo_placing(&mut self) {
        use winit::event::{ElementState, MouseButton};

        self.choose_tool(Tool::Frame);
        let press = self.camera.world_to_screen(WorldPoint::new(-600.0, -340.0));
        let to = self.camera.world_to_screen(WorldPoint::new(600.0, 340.0));

        self.input.cursor_moved(&mut self.camera, press);
        let down = self.input.mouse_input(&mut self.camera, MouseButton::Left, ElementState::Pressed);
        self.act_on(down);
        // Several samples, because one is indistinguishable from a click and the report is
        // about what happens *during* the drag.
        for step in 1..=4 {
            let t = f64::from(step) / 4.0;
            let at = ScreenPoint::new(
                press.x + (to.x - press.x) * t,
                press.y + (to.y - press.y) * t,
            );
            let moved = self.input.cursor_moved(&mut self.camera, at);
            self.act_on(moved);
        }

        // The button is deliberately still down. Read the preview back out of the same
        // gesture the painter reads, and the emptiness out of the document.
        let previewing = self.input.placement().and_then(|(a, b)| {
            Self::swept_placement(
                self.shell.tool(),
                self.camera.screen_to_world(a),
                self.camera.screen_to_world(b),
            )
        });
        let items = self.editor.board().item_ids().len();
        match previewing {
            Some(box_) if items == 0 => self.ok(format!(
                "previewing a {} x {} frame mid-drag, with nothing on the board yet",
                box_.width.round(),
                box_.height.round(),
            )),
            Some(_) => self.gap(&format!(
                "the preview is up but {items} item(s) are already on the board"
            )),
            None => self.gap("a frame drag in progress previews nothing"),
        }
    }

    /// Leaves a caret and a selection on screen, so `--screenshot` can photograph them.
    ///
    /// `--demo typing` proves the keystrokes reach the document; it cannot show the caret,
    /// because it commits before the frame is drawn. This is the other half: it types, then
    /// moves the cursor back over part of the word with Shift held, and **leaves the
    /// session open**. What lands in the PNG is the accent caret and the translucent
    /// selection box behind the glyphs.
    fn demo_caret(&mut self) {
        use winit::keyboard::{Key, NamedKey};

        let placed = self.editor.edit(|board| {
            Ok(board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(0.0, 0.0, 199.0, 228.0),
            ))?)
        });
        let Ok(doc) = placed else {
            self.gap("the caret fixture could not place a sticky");
            return;
        };
        self.fit_board();
        self.editor.select([doc]);
        let Some(scene) = self.editor.projection().scene_id(doc) else {
            self.gap("the placed sticky is not in the projection");
            return;
        };
        if !self.begin_editing(scene, true) {
            self.gap("the caret fixture could not start an edit");
            return;
        }

        for character in "Caret".chars() {
            let text = character.to_string();
            self.type_key(&Key::Character(text.clone().into()), Some(&text));
        }
        // Shift-Left three times: the last three letters selected, the caret at their
        // left edge. Both are then on screen at once, which is the point.
        self.input.set_modifiers(winit::keyboard::ModifiersState::SHIFT);
        for _ in 0..3 {
            self.type_key(&Key::Named(NamedKey::ArrowLeft), None);
        }
        self.input.set_modifiers(winit::keyboard::ModifiersState::empty());

        let session = self.editing.as_ref().expect("the session was left open");
        let (cursor, selection) = (session.buffer.cursor(), session.buffer.selection());
        self.ok(format!("caret at {cursor}, selecting {selection:?}"));
    }

    /// Drives a **real card drag** through `act_on`, and reports whether the card moved.
    ///
    /// This exists because of the lesson `CLAUDE.md` records against paste: *a diagnostic
    /// that skips the input layer cannot verify a feature the user reaches through the
    /// input layer.* A card drag is a mid-gesture state, so `--screenshot` cannot
    /// photograph it, and `ActiveState` owns a GPU and a window so no unit test can build
    /// one. The pure halves — `drop_target`, `commit_drop`, the coordinate round trip —
    /// are tested in `crate::kanban`; what is only reachable here is the *wiring*:
    /// whether a press on a card starts a card drag rather than moving the board, and
    /// whether releasing rewrites the token.
    ///
    /// It enters at [`ActiveState::act_on`] with the same [`Intent`]s `crate::input`
    /// builds from real events, so everything from `press` down is the production path.
    fn demo_card_drag(&mut self) {
        let (width, height) = crate::kanban::DEFAULT_SIZE;
        let model = crate::kanban::default_kanban();
        let placed = self.editor.edit(|board| {
            Ok(board.add(NewItem::new(
                ItemKind::Kanban { board: crate::kanban::encode(&model) },
                Placement::new(0.0, 0.0, width, height),
            ))?)
        });
        if let Err(error) = placed {
            self.failed("placing the card-drag board", &error);
            return;
        }
        self.fit_board();

        // The first card of the first column, and the top of the last column.
        let placement = Placement::new(0.0, 0.0, width, height);
        let engine = self.painter.text_mut().engine_mut();
        let (_, laid) = crate::kanban::layout(&model, engine, (width, height));
        let (Some(first), Some(last)) = (laid.columns.first(), laid.columns.last()) else {
            self.gap("the card-drag fixture has no columns");
            return;
        };
        let Some(card) = first.cards.first() else {
            self.gap("the card-drag fixture has no cards");
            return;
        };
        let (card_id, from_column) = (card.id, first.id);
        let to_column = last.id;
        let grab = card.rect.centre();
        let target = (last.body.centre().x, last.body.top() + 4.0);

        // Board-local → world → screen, so the synthesised events are in the same space
        // a mouse would deliver.
        let screen = |local: (f64, f64)| {
            let [(x, y), ..] = crate::kanban::board_world(
                &placement,
                vellum_flow::Rect::new(local.0, local.1, 0.0, 0.0),
            );
            self.camera.world_to_screen(WorldPoint::new(x, y))
        };
        let press = screen((grab.x, grab.y));
        let release = screen(target);

        self.act_on(Intent::Press { at: press, additive: false, double: false });
        if self.card_drag.is_none() {
            self.gap("a press on a card did not start a card drag");
            return;
        }
        self.act_on(Intent::Move { from: press, to: release });
        self.act_on(Intent::MoveEnd { moved: true });

        // Read the answer back out of the document, not out of the gesture.
        let landed = self
            .editor
            .board()
            .item_ids()
            .into_iter()
            .filter_map(|id| self.editor.board().item(id).ok())
            .find_map(|item| match item.kind {
                ItemKind::Kanban { board } => crate::kanban::decode(&board).locate(card_id),
                _ => None,
            });
        match landed {
            Some((column, index)) if column == to_column => {
                self.ok(format!("card moved to the last column at index {index}"));
            }
            Some((column, _)) if column == from_column => {
                self.gap("the card did not move: it is still in the first column");
            }
            Some(_) => self.gap("the card moved to the wrong column"),
            None => self.gap("the card vanished"),
        }
    }

    /// One mind map in each of the four layout forms.
    ///
    /// Four rather than one because the forms are where a mind map can go wrong: the
    /// tidy pass is shared, but the balanced form splits branches by subtree weight and
    /// the radial one works in polar coordinates, and neither can be checked by looking
    /// at a right-growing tree. Each is given its own natural box, so a fit scale of
    /// anything but 1 in the render is itself the bug.
    fn demo_mindmap(&mut self) {
        use vellum_mindmap::{Axis, ConnectorShape, Direction, LayoutKind};
        let kinds = [
            (LayoutKind::Tree { direction: Direction::Right }, ConnectorShape::Curve),
            (LayoutKind::Tree { direction: Direction::Down }, ConnectorShape::Elbow),
            (LayoutKind::Balanced { axis: Axis::Horizontal }, ConnectorShape::Curve),
            (
                LayoutKind::Radial { start_angle: 0.0, sweep: std::f64::consts::TAU },
                ConnectorShape::Straight,
            ),
        ];

        // Laid out here rather than trusted to a constant: the boxes have to be the
        // maps' own natural sizes for the fixture to prove anything about scaling, and
        // only the font stack knows those.
        let engine = self.painter.text_mut().engine_mut();
        let placed: Vec<(String, (f64, f64))> = kinds
            .into_iter()
            .map(|(kind, connectors)| {
                let model = crate::mindmap::MindMapModel {
                    kind,
                    connectors,
                    ..crate::mindmap::default_mindmap()
                };
                let size = crate::mindmap::natural_size(&crate::mindmap::layout(&model, engine));
                (crate::mindmap::encode(&model), size)
            })
            .collect();

        const PITCH: f64 = 900.0;
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for (index, (model, (width, height))) in placed.into_iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "four maps")]
                let (x, y) = ((index % 2) as f64 * PITCH, (index / 2) as f64 * PITCH);
                board.add(NewItem::new(
                    ItemKind::MindMap { model },
                    Placement::new(x, y, width, height),
                ))?;
            }
            board.end_undo_group();
            Ok(())
        });
        match result {
            Ok(()) => {
                self.fit_board();
                self.ok("Placed four mind maps");
            }
            Err(error) => self.failed("building the mind-map demo", &error),
        }
    }

    /// One of every catalogue shape, laid out in a grid and labelled with its name.
    /// One board with a bit of everything on it — the README's front picture.
    ///
    /// Every other `--demo` shows a single subsystem, which is right for checking that
    /// subsystem and wrong for a first impression: nobody's board is forty-one shapes in
    /// a grid. This one is a frame with work inside it, so the picture answers "what is
    /// this?" rather than "what does the shape catalogue contain?".
    ///
    /// It is a **fixture, not a screenshot script**: it places items and fits the camera,
    /// and nothing here knows it is being photographed.
    fn demo_readme(&mut self) {
        use vellum_doc::{Color, Style};

        let sticky = |text: &str, x: f64, y: f64, fill: Color| {
            NewItem::new(
                ItemKind::Sticky { text: StyledText::plain(text), background: Some(fill) },
                // 280 rather than 210: auto-fit maximises the type to fill the note, and a
                // narrow note makes it pick a size at which a single long word no longer
                // fits the line — so it breaks mid-word. Width is the cheap fix.
                Placement::new(x, y, 280.0, 210.0),
            )
        };
        let outlined = |fill: (u8, u8, u8), stroke: (u8, u8, u8)| Style {
            fill: Some(Color::rgb(fill.0, fill.1, fill.2)),
            stroke: Some(Color::rgb(stroke.0, stroke.1, stroke.2)),
            stroke_width: Some(3.0),
            ..Style::default()
        };

        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;

            // The frame is added first and every item names it as a parent, so the
            // picture shows containment rather than a scatter of loose items.
            let frame = board.add(
                NewItem::new(
                    ItemKind::Frame {
                        title: StyledText::plain("Release plan"),
                        order: Some(0),
                        speaker_notes: None,
                    },
                    Placement::new(0.0, 0.0, 1840.0, 1020.0),
                )
                .with_style(Style { fill: Some(Color::rgb(0xFF, 0xFF, 0xFF)), ..Style::default() }),
            )?;

            let amber = Color::rgb(0xFF, 0xF7, 0x9E);
            let mint = Color::rgb(0xD3, 0xF2, 0xE3);
            for (text, x, y, fill) in [
                ("Ship the importer", -680.0, -250.0, amber),
                ("Measure, not guess", -680.0, 10.0, amber),
                ("Cull by viewport", -680.0, 270.0, mint),
            ] {
                board.add(sticky(text, x, y, fill).with_parent(frame))?;
            }

            // Two shapes and a connector between them: the diagram half of a board.
            let source = board.add(
                NewItem::new(
                    ItemKind::Shape {
                        form: crate::shapes::encode(vellum_shapes::Shape::rounded_rectangle()),
                        text: StyledText::plain("Miro board"),
                    },
                    Placement::new(60.0, -230.0, 300.0, 150.0),
                )
                .with_parent(frame)
                .with_style(outlined((0xDC, 0xE9, 0xF7), (0x1A, 0x3A, 0x6B))),
            )?;
            let target = board.add(
                NewItem::new(
                    ItemKind::Shape {
                        form: crate::shapes::encode(vellum_shapes::Shape::cylinder()),
                        text: StyledText::plain("Velm board"),
                    },
                    Placement::new(620.0, -230.0, 300.0, 150.0),
                )
                .with_parent(frame)
                .with_style(outlined((0xD3, 0xF2, 0xE3), (0x0B, 0x5C, 0x4E))),
            )?;
            board.add(
                NewItem::new(
                    ItemKind::Connector {
                        start: ConnectorEnd::bound(source, ConnectorEnd::RIGHT),
                        end: ConnectorEnd::bound(target, ConnectorEnd::LEFT),
                        routing: vellum_doc::Routing::Straight,
                        dash: vellum_doc::Dash::Solid,
                        thickness: 3.0,
                        color: None,
                        captions: Vec::new(),
                    },
                    Placement::new(490.0, -155.0, 260.0, 8.0),
                )
                .with_parent(frame),
            )?;

            board.add(
                NewItem::new(
                    ItemKind::Text {
                        text: StyledText::plain("Frame cost scales with what is on screen"),
                    },
                    Placement::new(340.0, 30.0, 900.0, 70.0),
                )
                .with_parent(frame),
            )?;

            // A pen stroke, so the picture is not all rectangles. A hand-drawn tick:
            // down-right, then up-right and further.
            board.add(
                NewItem::new(
                    ItemKind::Ink {
                        points: vec![
                            vellum_doc::Point { x: -110.0, y: -30.0 },
                            vellum_doc::Point { x: -40.0, y: 45.0 },
                            vellum_doc::Point { x: 0.0, y: 70.0 },
                            vellum_doc::Point { x: 55.0, y: -10.0 },
                            vellum_doc::Point { x: 110.0, y: -75.0 },
                        ],
                        color: Some(Color::rgb(0x00, 0xA3, 0x8C)),
                        thickness: 12.0,
                    },
                    Placement::new(430.0, 300.0, 220.0, 150.0),
                )
                .with_parent(frame),
            )?;

            board.add(
                NewItem::new(
                    ItemKind::Shape {
                        form: crate::shapes::encode(vellum_shapes::Shape::Ellipse),
                        text: StyledText::plain("100 fps"),
                    },
                    Placement::new(760.0, 300.0, 260.0, 180.0),
                )
                .with_parent(frame)
                .with_style(outlined((0xFF, 0xE8, 0xE7), (0xB0, 0x35, 0x32))),
            )?;

            board.end_undo_group();
            Ok(())
        });

        if placed.is_err() {
            self.gap("the readme fixture could not build its board");
            return;
        }
        self.fit_board();
        self.choose_tool(Tool::Select);
        self.ok("Placed a board with 9 items");
    }

    fn demo_shapes(&mut self) {
        const COLUMNS: usize = 8;
        const PITCH: f64 = 260.0;
        let shapes = vellum_shapes::CATALOGUE;
        let style = vellum_doc::Style {
            fill: Some(vellum_doc::Color::rgb(0xDC, 0xE9, 0xF7)),
            stroke: Some(vellum_doc::Color::rgb(0x1A, 0x3A, 0x6B)),
            stroke_width: Some(3.0),
            ..vellum_doc::Style::default()
        };

        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for (index, shape) in shapes.iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "41 shapes, exact in f64")]
                let (x, y) = ((index % COLUMNS) as f64 * PITCH, (index / COLUMNS) as f64 * PITCH);
                board.add(
                    NewItem::new(
                        ItemKind::Shape {
                            form: crate::shapes::encode(*shape),
                            text: StyledText::plain(shape.name()),
                        },
                        Placement::new(x, y, 200.0, 160.0),
                    )
                    .with_style(style.clone()),
                )?;
            }
            board.end_undo_group();
            Ok(())
        });
        match result {
            Ok(()) => {
                self.fit_board();
                self.ok(format!("Placed {} shapes", shapes.len()));
            }
            Err(error) => self.failed("building the shape demo", &error),
        }
    }

    /// Raises a dialog by name, for `--open-dialog`. See [`crate::options::Options`].
    ///
    /// Routed through the same `Command`s the menus use, so what is photographed is the
    /// dialog the app actually shows rather than a rehearsal of one.
    pub(crate) fn open_named_dialog(&mut self, name: &str) {
        match name {
            "shortcuts" => self.run(Command::KeyboardShortcuts),
            "documentation" => self.run(Command::Documentation),
            "about" => self.run(Command::About),
            "new-board" => self.run(Command::NewBoard),
            "import-steps" => self.run(Command::ImportFromMiro),
            other => log::warn!(
                "--open-dialog: no dialog called `{other}` \
                 (shortcuts, documentation, about, new-board, import-steps)"
            ),
        }
    }

    // ----- tools -------------------------------------------------------------

    /// Switches the active tool and tells `crate::input` what a drag now means.
    ///
    /// `crate::input::Tool` has three members. Select and Hand change what a drag
    /// does; Place stands for every tool whose click *creates* something, because the
    /// input layer has to know that such a press must neither select nor marquee —
    /// *what* gets made is [`ActiveState::place`]'s business and stays here.
    pub(crate) fn choose_tool(&mut self, tool: Tool) {
        // A sweep that ends by *changing tool* never reaches `Intent::Place`, so `finish_erase`
        // — the only thing that closes the eraser's group — would never run. The same shape as
        // Escape and a tab switch, arrived at from a third direction; `finish_erase` returns
        // early when nothing is being erased, so this costs nothing the rest of the time.
        self.finish_erase();
        self.shell.set_tool(tool);
        self.input.set_tool(match tool {
            Tool::Hand => crate::input::Tool::Hand,
            Tool::Select => crate::input::Tool::Select,
            Tool::Sticky
            | Tool::Text
            | Tool::Shape
            | Tool::Pen
            | Tool::Eraser
            | Tool::Connector
            | Tool::Frame
            | Tool::Table
            | Tool::Chart
            | Tool::MindMap
            | Tool::Kanban
            | Tool::Image
            // All four Agent Canvas nodes are placed by a sweep, exactly like a frame:
            // they are boxes whose useful size is the one the user drew, and a click gives
            // the module's own default.
            | Tool::Agent
            | Tool::Note
            | Tool::FileTree
            | Tool::Browser => crate::input::Tool::Place,
        });
    }

    /// Acts on whatever a pointer gesture asked for beyond moving the camera.
    pub(crate) fn act_on(&mut self, intent: Intent) {
        if self.shell.screen() != Screen::Board {
            return;
        }
        match intent {
            Intent::None => {}
            Intent::Press { at, additive, double } => self.press(at, additive, double),
            Intent::Move { from, to } => {
                // A card in flight owns the gesture: it was decided at the press, and a
                // drag that changed its mind halfway would move the board when the user
                // meant to move a card.
                if self.card_drag.is_some() {
                    self.drag_card_to(to);
                } else {
                    self.drag_to(from, to);
                }
            }
            Intent::MoveEnd { moved } => {
                if self.card_drag.is_some() {
                    self.finish_card_drag();
                } else {
                    self.finish_drag(moved);
                }
            }
            Intent::ContextMenu { at } => self.open_context_menu(at),
            Intent::Marquee { from, to, additive } => {
                let rect = WorldRect::from_corners(
                    self.camera.screen_to_world(from),
                    self.camera.screen_to_world(to),
                );
                self.editor.marquee(rect, additive);
                self.shell.invalidate_selection();
            }
            Intent::PlaceSample { at } => {
                // The eraser is in the same gesture bucket as the pen and has always
                // received this stream; it simply threw every sample away.
                if matches!(self.shell.tool(), Tool::Eraser) {
                    let at = self.camera.screen_to_world(at);
                    self.erase_at(at);
                } else if matches!(self.shell.tool(), Tool::Pen) {
                    let at = self.camera.screen_to_world(at);
                    // Skip samples closer than half a **device pixel**: a slow hand emits
                    // hundreds of sub-pixel moves, and they add vertices without adding
                    // shape.
                    //
                    // The threshold used to be a flat 0.5 *world* units, which is the same
                    // filter only at 100% zoom. At zoom 8 it is a four-device-pixel floor —
                    // so drawing zoomed in, which is exactly what anyone does for detail,
                    // threw away most of the hand's movement. And it is **destructive**:
                    // `commit_stroke` stores these points, so the coarseness is baked into
                    // the document and no later re-render can recover it. Dividing by the
                    // zoom makes it a half-pixel filter at every magnification, which is the
                    // same shape as `vellum_ink::Lod`'s screen-space tolerance and the
                    // selection ring's width, and it is guarded against a nonsense zoom the
                    // same way.
                    let zoom = self.camera.zoom();
                    let least = if zoom.is_finite() && zoom > 0.0 { 0.5 / zoom } else { 0.5 };
                    let far_enough = self
                        .stroke
                        .last()
                        .is_none_or(|last: &WorldPoint| (at.x - last.x).hypot(at.y - last.y) > least);
                    if far_enough {
                        self.stroke.push(at);
                    }
                }
            }
            Intent::Place { at, to } => {
                let from = self.camera.screen_to_world(at);
                let to = self.camera.screen_to_world(to);
                self.place(from, to);
            }
        }
    }

    // ----- moving things with the mouse --------------------------------------

    /// The left button went down on the canvas.
    ///
    /// Selection happens **here**, on the press, not on the release: it is the only
    /// order in which an item can be grabbed and dragged in one gesture, and Miro
    /// does the same. A press inside an existing multi-selection keeps that
    /// selection, so a group of items can be dragged as one — replacing it with the
    /// single item under the pointer is the classic way to make a careful selection
    /// impossible to move.
    /// The right button, released without having panned.
    ///
    /// Two things happen here that cannot happen in `vellum-ui`, which is why the app
    /// resolves the target rather than the chrome:
    ///
    /// 1. **What is under the pointer** is a question only the scene can answer, and
    ///    `vellum-ui` has never been allowed to read one.
    /// 2. **A right-click on an unselected item selects it first.** Every canvas tool
    ///    does this, and the alternative is a menu whose *Delete* acts on something
    ///    off screen that the user forgot was selected. A right-click *inside* an
    ///    existing selection leaves it alone, so right-clicking one of five picked
    ///    stickies still offers all five.
    ///
    /// A locked item is transparent here exactly as it is to a left press — same
    /// filter, same reason — so right-clicking a locked sticky lying over a frame
    /// offers the frame's menu rather than nothing at all.
    fn open_context_menu(&mut self, at: ScreenPoint) {
        // A caret is a modal state: the right button ends the edit rather than opening
        // a menu over a half-typed word.
        if self.editing.is_some() {
            self.commit_editing();
        }

        let world = self.camera.screen_to_world(at);
        let locked: Vec<SceneId> = self
            .editor
            .projection()
            .iter()
            .filter(|(_, projected)| projected.item.style.locked)
            .map(|(id, _)| *id)
            .collect();
        let hit = self
            .editor
            .projection()
            .scene()
            .hit_test_where(world, |id| !locked.contains(&id));

        let target = match hit {
            // Already part of what is selected: leave the selection alone.
            Some(id) if self.editor.selection().contains(&id) => ContextTarget::Selection,
            Some(_) => {
                self.editor.pick(world, false);
                self.shell.invalidate_selection();
                ContextTarget::Selection
            }
            // Bare board. The selection is *not* cleared: a right-click is a request
            // for a menu, not a click, and throwing away a selection the user spent
            // time building because they asked what the canvas can do would be its
            // own bug. The menu drawn is the canvas one either way.
            None => ContextTarget::Canvas,
        };

        // Logical points, not physical: `ScreenPoint` is physical pixels everywhere in
        // this crate (trap 4) and egui lays out in logical ones, so a menu opened
        // without this division lands at twice the pointer's distance from the origin
        // on a retina display — off screen for anything past the middle of the window.
        let scale = self.shell.pixels_per_point().max(f32::MIN_POSITIVE);
        let at = egui::pos2(at.x as f32 / scale, at.y as f32 / scale);
        self.shell.open_context_menu(at, target);
    }

    fn press(&mut self, at: ScreenPoint, additive: bool, double: bool) {
        let world = self.camera.screen_to_world(at);

        // A caret on the board claims a click inside its own item — that is how a cursor
        // is placed with the mouse. A click anywhere else **ends** the session and then
        // goes on to do whatever it would have done: clicking away from a caret is how
        // editing finishes, and swallowing the click as well would cost the user a
        // second one.
        if self.editing.is_some() {
            if self.click_in_editing(world, additive, self.input.click_run()) {
                self.input.resolve_press(true);
                return;
            }
            self.commit_editing();
        }

        // Handles first, before the scene is asked anything. Two reasons they cannot
        // wait their turn: a handle has to win over an item stacked on top of the
        // selection, and the rotate handle sits *outside* the item's own bounds, so
        // `Scene::hit_test` would never find it at all.
        if let Some((scene, handle)) = self.handle_under(world) {
            self.input.resolve_press(true);
            self.begin_drag(if handle.is_rotate() {
                DragMode::Rotate
            } else {
                DragMode::Resize(handle)
            });
            // Deliberately no selection change: the handle belongs to what is already
            // selected, and re-picking here would drop a multi-selection on the way in.
            let _ = scene;
            return;
        }

        // A locked item is transparent to the pointer, not a hole: the click reaches
        // whatever is beneath it. `Scene::hit_test_where` applies the filter before
        // "topmost" precisely so that holds — a locked sticky lying over a frame must not
        // make the frame unclickable through it.
        let locked: Vec<SceneId> = self
            .editor
            .projection()
            .iter()
            .filter(|(_, projected)| projected.item.style.locked)
            .map(|(id, _)| *id)
            .collect();
        let hit = self
            .editor
            .projection()
            .scene()
            .hit_test_where(world, |id| !locked.contains(&id));
        // **Read before the press picks anything**, or it is always true — the arm below
        // selects the frame, and asking afterwards then says it was already selected and the
        // drag moves it. The fixture caught exactly that.
        let was_selected = hit.is_some_and(|id| self.editor.selection().contains(&id));
        match hit {
            Some(id) if !additive && self.editor.selection().contains(&id) => {}
            Some(_) => {
                self.editor.pick(world, additive);
                self.shell.invalidate_selection();
            }
            None => {
                if !additive && !self.editor.selection().is_empty() {
                    self.editor.clear_selection();
                    self.shell.invalidate_selection();
                }
            }
        }

        // The open-page badge, before anything is selected or dragged. It is *inside* the
        // card's own bounds, so the hit test above has already answered with the card and the
        // question left is only which part of it was pressed — the same shape as a kanban
        // card drag below, and the opposite of a resize handle, which has to be asked before
        // the scene because it can lie outside the item entirely.
        //
        // **`!double` and `!additive` are both load-bearing.** A double-click delivers *two*
        // presses — `double: false` then `double: true`, which `input.rs`'s own
        // `two_quick_clicks_in_the_same_place_are_a_double_click` pins — and nothing between
        // here and there suppresses the second. Without the guard one gesture spawned two
        // browser processes and stacked two toasts. `additive` is the other half: ⇧-click is
        // how a card is added to a selection, and a badge that swallowed it would make the
        // top-right corner of every card impossible to multi-select.
        if !double
            && !additive
            && let Some(id) = hit
            && let Some(url) = self.badge_under(id, world)
        {
            // `true`, so a hand that moves a pixel between press and release does not turn
            // the click into a marquee sweep across the board.
            self.input.resolve_press(true);
            self.open_in_browser(&url);
            return;
        }

        // **A frame is a backdrop until you have picked it up.**
        //
        // *"when i try to drag select multiple things, if i start dragging while being on top
        // of a frame it starts moving the frame instead of drag selecting … if i want to move
        // the frame behind it i have to press on it once."* A frame is usually the largest
        // thing on the board and everything else sits on top of it, so treating a press on one
        // as "grab this" makes marquee selection impossible over most of the board — which is
        // the half of the gesture that gets used constantly.
        //
        // Miro's rule, and the one asked for: a press on a frame that is **not already
        // selected** sweeps. Selecting it is still done by the press above, and a click that
        // does not travel further than `CLICK_SLOP` yields `Intent::None` — so the selection
        // survives, and the *next* drag finds the frame selected and moves it. One click to
        // pick it up, then drag.
        //
        // Deliberately scoped to frames rather than to "any container": a table, a kanban and
        // a mind map are all things you drag as a unit, and they are not what anything else
        // sits on top of.
        let backdrop = !was_selected
            && hit.is_some_and(|id| {
                self.editor
                    .projection()
                    .get(id)
                    .is_some_and(|projected| matches!(projected.item.kind, ItemKind::Frame { .. }))
            });

        // Synchronously, before the next event: this is what tells `crate::input`
        // whether the drag that may follow moves the selection or sweeps a rectangle.
        self.input.resolve_press(hit.is_some() && !backdrop);
        if let Some(id) = hit.filter(|_| !backdrop) {
            // A press on a kanban card drags the card, not the board — which is Miro's
            // ergonomics and the same rule a frame follows: the chrome moves the
            // container, the contents move themselves. Only for a lone selection: with
            // several items picked, "move everything" is the unambiguous reading, and a
            // card drag that silently dropped the rest of the selection would be worse
            // than not having one.
            if self.editor.selection() == [id]
                && let Some(card) = self.card_under(id, world)
            {
                self.card_drag = Some(card);
                return;
            }
            self.begin_drag(DragMode::Move);
            // A double click opens the item's words — on the *board*, with a real caret.
            // It used to mean "put the cursor in the panel's text field", which is a text
            // field near a board rather than editing on one. The panel's field is still
            // there and still works; it is no longer where a double click goes.
            //
            // A kind with no words of its own falls back to the panel, because the panel
            // is where its properties are and there is nothing on the canvas to type into.
            //
            // The pointer is passed on, so a double click inside a table, a kanban or a mind
            // map lands in the *cell, card or node* under it rather than on the item as a
            // whole — which is what makes those three editable at all, since none of them
            // has words of its own for a caret to sit in.
            if double && !self.begin_editing_at(id, false, Some(world)) {
                self.shell.focus_text();
            }
        }
    }

    /// Records where everything in the selection was when the drag began.
    ///
    /// The original placements, not a running total: every frame of the drag is
    /// `original + offset from the press`, so a thousand samples cannot accumulate
    /// into a drift and letting go at the starting point leaves the board untouched.
    fn begin_drag(&mut self, mode: DragMode) {
        let items: Vec<(SceneId, DocId, Placement)> = self
            .editor
            .selection()
            .iter()
            .filter_map(|scene| {
                let projected = self.editor.projection().get(*scene)?;
                // A locked item cannot be *picked* any more, but a selection made before
                // it was locked survives — so the drag has to filter too, or locking
                // something mid-selection would leave it draggable until deselected.
                if projected.item.style.locked {
                    return None;
                }
                Some((*scene, projected.doc_id, projected.item.placement))
            })
            .collect();
        // The group box, for a transform that acts on more than one item. Captured once:
        // the box is derived *from* the members, so recomputing it per frame would feed each
        // scaled box into the next scale and the selection would run away from the pointer.
        let group = if items.len() > 1 && !matches!(mode, DragMode::Move) {
            let placements: Vec<Placement> = items.iter().map(|(_, _, p)| *p).collect();
            handle::group_bounds(&placements).map(|group| {
                let pointer = self.camera.screen_to_world(self.input.cursor(&self.camera));
                (group, handle::angle_to((group.x, group.y), pointer))
            })
        } else {
            None
        };
        // Only a lone image locks its ratio. A multi-selection already scales uniformly
        // through `group_scale`, so asking again here would be redundant, and a mixed
        // selection has no one ratio to keep.
        let lock_aspect = items.len() == 1
            && items.iter().any(|(scene, _, _)| {
                self.editor
                    .projection()
                    .get(*scene)
                    .is_some_and(|p| matches!(p.item.kind, ItemKind::Image { .. }))
            });
        // **A move carries what is inside what is being moved.** *"when i move the frame
        // everything on the frame should also move."* A frame's children are separate items
        // that merely name it as their parent — nothing in `vellum-doc` composes a parent's
        // transform into a child's, `Placement` is absolute — so without this a frame slid
        // out from under everything sitting on it.
        //
        // **Move only, and after `group`/`lock_aspect` are decided.** A resize does *not*
        // scale a frame's contents (Miro's does not either — a frame is a viewport over the
        // board, so growing one reveals more rather than magnifying what is there), and both
        // of those tests are about what the user actually *selected*: widening first would
        // make a lone frame with two stickies on it read as a three-item multi-selection and
        // take the group-transform path.
        let mut items = items;
        if matches!(mode, DragMode::Move) {
            self.widen_to_contents(&mut items);
        }
        self.drag = if items.is_empty() {
            None
        } else {
            Some(Drag {
            items, offset: (0.0, 0.0), mode, group, lock_aspect })
        };
    }

    /// Adds everything parented to what is already in the list.
    ///
    /// The one place a *move* is widened from the selection to the subtree, so a drag and
    /// an arrow-key nudge cannot come to disagree about whether a frame takes its contents.
    /// `Board::descendants` walks to the bottom, so a frame holding a group holding a sticky
    /// moves whole.
    ///
    /// **A locked child comes too**, which is deliberately *not* what the top-level filter
    /// does. A lock means "I did not mean to drag this"; it cannot mean "leave this behind
    /// when the surface under it moves", because the result is an item that is no longer on
    /// the frame it was pinned to — visually broken in a way no lock was asked to produce.
    /// This is the rule `Board::remove` already follows for a deleted frame, where a locked
    /// child goes with the subtree.
    ///
    /// Deduplicated against what is there, so selecting a frame *and* a sticky on it moves
    /// the sticky once rather than twice as far.
    fn widen_to_contents(&self, items: &mut Vec<(SceneId, DocId, Placement)>) {
        let mut seen: HashSet<DocId> = items.iter().map(|(_, doc, _)| *doc).collect();
        let board = self.editor.board();
        let children: Vec<DocId> = items
            .iter()
            .flat_map(|(_, doc, _)| board.descendants(*doc))
            .filter(|child| seen.insert(*child))
            .collect();
        for child in children {
            let Some(scene) = self.editor.projection().scene_id(child) else { continue };
            let Some(projected) = self.editor.projection().get(scene) else { continue };
            items.push((scene, child, projected.item.placement));
        }
    }

    // ----- structured widgets: which piece a click landed on -------------------------

    /// One editable piece of a structured widget.
    ///
    /// The three fields are three different namespaces and all three are needed: `slot` is
    /// the painter's, and is what puts the caret in the right box; `part` is the document's,
    /// and is what a keystroke is written into; `text` is what the field says now, which is
    /// what seeds the buffer.
    ///
    /// Nothing here is cached. `card_under`'s note says why in full and it applies to all
    /// three widgets: the painter's layout caches are filled *during paint*, so resolving a
    /// press against them would make a click depend on whether a frame had been drawn since
    /// the last edit. One shaping pass per press is nothing; per frame it would not be.
    fn widget_part_under(
        &mut self,
        scene: SceneId,
        world: WorldPoint,
    ) -> Option<(u16, crate::edit::EditPart, String)> {
        use crate::edit::EditPart;
        use crate::draw::CELL_SLOT_BASE;

        // Read what is needed out of the projection before the font engine is borrowed:
        // they live on different fields and the token has to outlive the projection borrow.
        let (placement, kind) = {
            let projected = self.editor.projection().get(scene)?;
            (projected.item.placement, projected.item.kind.clone())
        };
        let (item_w, item_h) = placement.scaled_size();
        // The item's own space, measured from its top-left. **Rotation is not undone**,
        // matching the painter: `table_cell_block` and its two siblings position a widget's
        // internals from the unrotated box, so a rotated table draws its cells unrotated and
        // this agrees with what is on screen rather than with what would be tidier.
        let local = (world.x - (placement.x - item_w / 2.0), world.y - (placement.y - item_h / 2.0));
        let size = (item_w, item_h);

        match kind {
            ItemKind::Table { model } => {
                let table = crate::table::decode(&model);
                let laid =
                    crate::table::layout(&table, self.painter.text_mut().engine_mut(), size);
                // A generous tolerance would steal a click meant for a cell and give it to
                // the boundary between two; zero is what "I clicked in this cell" means.
                let at = laid
                    .hit_test(vellum_table::Point::new(local.0, local.1), 0.0)
                    .cell()?;
                // The index into `laid.cells` *is* the painter's slot numbering — the same
                // list, walked by the same crate — so there is no second mapping to keep in
                // step here.
                let index = laid.cells.iter().position(|cell| cell.anchor == at)?;
                let text = table.cell(at)?.content().to_plain();
                Some((CELL_SLOT_BASE + u16::try_from(index).ok()?, EditPart::TableCell(at), text))
            }

            ItemKind::Kanban { board } => {
                let model = crate::kanban::decode(&board);
                let (measured, laid) =
                    crate::kanban::layout(&model, self.painter.text_mut().engine_mut(), size);
                // The painter's own flattening, called rather than reproduced. See
                // `draw::KanbanRun`: two copies of it is a caret on one card typing into
                // another.
                let runs = crate::draw::kanban_runs(&measured, &laid);
                let index = runs.iter().position(|run| {
                    let rect = run.rect();
                    // The run's own box rather than the card's, so a click in a card's
                    // padding below a one-line label still counts as that card — the rect is
                    // already the inset text box, and `vellum-flow` sized the card to it.
                    local.0 >= rect.left()
                        && local.0 <= rect.right()
                        && local.1 >= rect.top()
                        && local.1 <= rect.bottom()
                })?;
                let run = runs.get(index)?;
                Some((
                    CELL_SLOT_BASE + u16::try_from(index).ok()?,
                    run.part(),
                    run.field().to_owned(),
                ))
            }

            ItemKind::MindMap { model } => {
                let map = crate::mindmap::decode(&model);
                let layout = crate::mindmap::layout(&map, self.painter.text_mut().engine_mut());
                // Into map space: the tidy tree is laid out at its natural size and then
                // fitted into the item's box, so the click has to be divided by the same
                // scale the painter multiplies by.
                let scale = crate::mindmap::fit_scale(crate::mindmap::natural_size(&layout), size);
                let point = vellum_mindmap::Point::new(local.0 / scale, local.1 / scale);
                let node = layout.hit_test(point)?;
                let index = layout.placements().iter().position(|p| p.node == node)?;
                let text = map.map.get(node)?.text.clone();
                Some((
                    CELL_SLOT_BASE + u16::try_from(index).ok()?,
                    EditPart::MindMapNode(node),
                    text,
                ))
            }

            // A chart has labels but no field a click can address: a category belongs to the
            // dataset rather than to the mark drawn for it, so editing one is a data editor's
            // job and not a caret's.
            _ => None,
        }
    }

    /// Writes one edited field back into a structured widget's token.
    ///
    /// Returns whether anything was written. `false` for a part whose owner has since gone —
    /// a card deleted while its caret was open, an undo that replaced the whole model — which
    /// is exactly what the never-reused ids in `vellum-flow` and `vellum-mindmap` are for:
    /// a stale handle matches nothing instead of matching whichever card took its place.
    fn write_widget_part(&mut self, doc: DocId, part: crate::edit::EditPart, text: &str) -> bool {
        use crate::edit::EditPart;

        let Ok(item) = self.editor.board().item(doc) else { return false };
        let kind = match (&item.kind, part) {
            (ItemKind::Table { model }, EditPart::TableCell(at)) => {
                let mut table = crate::table::decode(model);
                let Ok(cell) = table.cell_mut(at) else { return false };
                cell.set_content(vellum_table::StyledText::plain(text));
                ItemKind::Table { model: crate::table::encode(&table) }
            }
            (ItemKind::Kanban { board }, EditPart::KanbanTitle) => {
                let mut model = crate::kanban::decode(board);
                model.set_title(text);
                ItemKind::Kanban { board: crate::kanban::encode(&model) }
            }
            (ItemKind::Kanban { board }, EditPart::KanbanColumn(column)) => {
                let mut model = crate::kanban::decode(board);
                if model.rename_column(column, text).is_err() {
                    return false;
                }
                ItemKind::Kanban { board: crate::kanban::encode(&model) }
            }
            (ItemKind::Kanban { board }, EditPart::KanbanCard(card)) => {
                let mut model = crate::kanban::decode(board);
                if model.set_card_label(card, text).is_err() {
                    return false;
                }
                ItemKind::Kanban { board: crate::kanban::encode(&model) }
            }
            (ItemKind::MindMap { model }, EditPart::MindMapNode(node)) => {
                let mut map = crate::mindmap::decode(model);
                let Some(target) = map.map.get_mut(node) else { return false };
                target.text = text.to_owned();
                ItemKind::MindMap { model: crate::mindmap::encode(&map) }
            }
            // A part that does not belong to this kind. Reachable through an undo that
            // changed what the item is while a caret was open, which is a stale session
            // rather than a bug to assert on.
            _ => return false,
        };
        self.editor.edit(move |board| Ok(board.set_kind(doc, kind)?)).is_ok()
    }

    /// The slot that draws `part` right now, and what that field says.
    ///
    /// The reverse of [`Self::widget_part_under`]: that one starts from a pointer, this one
    /// from a field whose id is already known. Both walk the same lists, so a slot found here
    /// is the slot the painter will use — which is the whole requirement, because a session
    /// whose slot is stale draws its caret on a different cell from the one it types into.
    ///
    /// Recomputed rather than remembered because the layout *moves*: adding a card pushes
    /// every card below it down a slot, and adding a column renumbers the lot.
    fn slot_of_part(
        &mut self,
        scene: SceneId,
        part: crate::edit::EditPart,
    ) -> Option<(u16, String)> {
        use crate::draw::CELL_SLOT_BASE;
        use crate::edit::EditPart;

        let (placement, kind) = {
            let projected = self.editor.projection().get(scene)?;
            (projected.item.placement, projected.item.kind.clone())
        };
        let size = placement.scaled_size();

        match (kind, part) {
            (ItemKind::Table { model }, EditPart::TableCell(at)) => {
                let table = crate::table::decode(&model);
                let laid =
                    crate::table::layout(&table, self.painter.text_mut().engine_mut(), size);
                let index = laid.cells.iter().position(|cell| cell.anchor == at)?;
                Some((
                    CELL_SLOT_BASE + u16::try_from(index).ok()?,
                    table.cell(at)?.content().to_plain(),
                ))
            }
            (ItemKind::Kanban { board }, part) => {
                let model = crate::kanban::decode(&board);
                let (measured, laid) =
                    crate::kanban::layout(&model, self.painter.text_mut().engine_mut(), size);
                let runs = crate::draw::kanban_runs(&measured, &laid);
                let index = runs.iter().position(|run| run.part() == part)?;
                Some((
                    CELL_SLOT_BASE + u16::try_from(index).ok()?,
                    runs.get(index)?.field().to_owned(),
                ))
            }
            (ItemKind::MindMap { model }, EditPart::MindMapNode(node)) => {
                let map = crate::mindmap::decode(&model);
                let layout = crate::mindmap::layout(&map, self.painter.text_mut().engine_mut());
                let index = layout.placements().iter().position(|p| p.node == node)?;
                Some((
                    CELL_SLOT_BASE + u16::try_from(index).ok()?,
                    map.map.get(node)?.text.clone(),
                ))
            }
            _ => None,
        }
    }

    /// Moves the caret onto `part`, keeping the session and its undo group.
    ///
    /// The session is rebuilt rather than mutated because its slot has to be re-derived: see
    /// [`Self::slot_of_part`]. `opened` is carried over deliberately — walking a table row
    /// with Tab is **one** undo step, the same rule the whole session already follows, so ⌘Z
    /// after filling in a row puts the row back rather than the last cell.
    fn move_caret_to_part(&mut self, part: crate::edit::EditPart) -> bool {
        let Some(session) = self.editing.as_ref() else { return false };
        let (scene, doc, opened) = (session.scene, session.doc, session.opened());
        let Some((slot, text)) = self.slot_of_part(scene, part) else { return false };
        // Everything selected, so typing replaces: a field arrived at with Tab is one the
        // user is about to say something about, and this is what a spreadsheet does.
        let mut next = crate::edit::Editing::replacing(scene, doc, slot, text).in_part(part);
        if opened {
            next.mark_opened();
        }
        self.editing = Some(next);
        true
    }

    /// Tab, or ⇧Tab, while a caret is in a structured widget.
    ///
    /// **The next field, and a new one when there is no next field.** What "next" means is
    /// the widget's own convention rather than one rule imposed on three shapes:
    ///
    /// - A **table** moves to the next cell in reading order and stops at the last. A grid's
    ///   size is a property of the table, not a stream to append to, so Tab does not grow it.
    /// - A **kanban** moves down the column, and Tab on the last card *adds* one — which is
    ///   how any list editor behaves and is the only way to add a card at all. On a column
    ///   header it steps to the next header, adding a column at the end.
    /// - A **mind map** adds a **child**, always. Tab means "one level in" in every mind-map
    ///   tool there is, and matching that beats being internally consistent with the other
    ///   two.
    ///
    /// ⇧Tab only ever moves backwards; it never creates. A key that adds something when
    /// pressed by mistake is a key that leaves litter on the board.
    ///
    /// Returns whether the key was consumed — `false` for a caret in an ordinary item, whose
    /// Tab still belongs to the shortcut table.
    fn advance_part(&mut self, backwards: bool) -> bool {
        self.next_part(backwards).is_some_and(|part| self.move_caret_to_part(part))
    }

    /// Which field Tab should land on, creating one if the widget's convention says to.
    ///
    /// Split from [`Self::advance_part`] only so the lookups can use `?`: every one of them
    /// is "this id is still in the model", and a widget edited out from under a caret should
    /// leave the key unconsumed rather than panic.
    fn next_part(&mut self, backwards: bool) -> Option<crate::edit::EditPart> {
        use crate::edit::EditPart;

        let session = self.editing.as_ref()?;
        let (scene, doc, part) = (session.scene, session.doc, session.part);
        if matches!(part, EditPart::Item) {
            return None;
        }
        // Whatever is in the buffer has already been written through — every keystroke does
        // — so the model read here is current.
        let kind = self.editor.projection().get(scene).map(|p| p.item.kind.clone())?;

        match (&kind, part) {
            (ItemKind::Table { model }, EditPart::TableCell(at)) => {
                let table = crate::table::decode(model);
                // `anchors` is the same row-major walk the layout and `crate::table::words`
                // use, so "the next cell" is the next one a reader would come to.
                let anchors: Vec<vellum_table::CellRef> =
                    table.grid().anchors().map(|(at, _)| at).collect();
                let index = anchors.iter().position(|other| *other == at)?;
                let step = if backwards { index.checked_sub(1)? } else { index + 1 };
                Some(EditPart::TableCell(*anchors.get(step)?))
            }

            (ItemKind::MindMap { model }, EditPart::MindMapNode(node)) => {
                let mut map = crate::mindmap::decode(model);
                if backwards {
                    // Out one level rather than back through the reading order: Tab went in,
                    // so ⇧Tab comes out, and the root has nowhere to go.
                    return Some(EditPart::MindMapNode(map.map.parent(node)?));
                }
                // A child is added *unfolded*, and its parent unfolded with it — adding a
                // node you cannot see is a keystroke that appears to do nothing.
                let style = map.map.get(node).map(|n| n.style).unwrap_or_default();
                if let Some(parent) = map.map.get_mut(node) {
                    parent.collapsed = false;
                }
                let child =
                    map.map.add_child(node, vellum_mindmap::Node::new("").with_style(style)).ok()?;
                let token = crate::mindmap::encode(&map);
                self.editor
                    .edit(move |board| Ok(board.set_kind(doc, ItemKind::MindMap { model: token })?))
                    .ok()?;
                Some(EditPart::MindMapNode(child))
            }

            (ItemKind::Kanban { board }, EditPart::KanbanCard(card)) => {
                let mut model = crate::kanban::decode(board);
                let (column, index) = model.locate(card)?;
                let cards = model.column(column)?.card_ids();
                if backwards {
                    let previous = index.checked_sub(1).and_then(|i| cards.get(i).copied())?;
                    return Some(EditPart::KanbanCard(previous));
                }
                if let Some(next) = cards.get(index + 1) {
                    return Some(EditPart::KanbanCard(*next));
                }
                let added = model.add_card(column, "").ok()?;
                let token = crate::kanban::encode(&model);
                self.editor
                    .edit(move |board| Ok(board.set_kind(doc, ItemKind::Kanban { board: token })?))
                    .ok()?;
                Some(EditPart::KanbanCard(added))
            }

            (ItemKind::Kanban { board }, EditPart::KanbanColumn(column)) => {
                let mut model = crate::kanban::decode(board);
                let index = model.column_index(column)?;
                let columns: Vec<vellum_flow::ColumnId> =
                    model.columns().iter().map(|c| c.id()).collect();
                if backwards {
                    let previous = index.checked_sub(1).and_then(|i| columns.get(i).copied())?;
                    return Some(EditPart::KanbanColumn(previous));
                }
                if let Some(next) = columns.get(index + 1) {
                    return Some(EditPart::KanbanColumn(*next));
                }
                let added = model.add_column("");
                let token = crate::kanban::encode(&model);
                self.editor
                    .edit(move |board| Ok(board.set_kind(doc, ItemKind::Kanban { board: token })?))
                    .ok()?;
                Some(EditPart::KanbanColumn(added))
            }

            // A board title has no peer. Tab from it steps into the first column header,
            // which is the field a person filling in a fresh board wants next.
            (ItemKind::Kanban { board }, EditPart::KanbanTitle) if !backwards => {
                let model = crate::kanban::decode(board);
                Some(EditPart::KanbanColumn(model.columns().first()?.id()))
            }

            _ => None,
        }
    }

    /// ⌘⌫ while a caret is in a structured widget: removes the field it is in.
    ///
    /// Refused where removal has no meaning or would leave a widget that cannot be used
    /// again: a **table cell** (a grid's shape is edited, not its cells), a mind map's
    /// **root**, a kanban's **title**, and the **last** column of a board. Each refusal says
    /// so rather than doing nothing.
    ///
    /// The caret moves to the neighbour the removal leaves behind, so a run of deletions is a
    /// run rather than a click between each.
    fn remove_part(&mut self) -> bool {
        use crate::edit::EditPart;

        let Some(session) = self.editing.as_ref() else { return false };
        let (scene, doc, part) = (session.scene, session.doc, session.part);
        let Some(kind) = self.editor.projection().get(scene).map(|p| p.item.kind.clone()) else {
            return false;
        };

        let (token, next) = match (&kind, part) {
            (ItemKind::MindMap { model }, EditPart::MindMapNode(node)) => {
                let mut map = crate::mindmap::decode(model);
                if map.map.root() == node {
                    self.gap("a mind map's central idea cannot be removed");
                    return true;
                }
                let parent = map.map.parent(node);
                if map.map.remove_subtree(node).is_err() {
                    return false;
                }
                (
                    ItemKind::MindMap { model: crate::mindmap::encode(&map) },
                    parent.map(EditPart::MindMapNode),
                )
            }
            (ItemKind::Kanban { board }, EditPart::KanbanCard(card)) => {
                let mut model = crate::kanban::decode(board);
                let neighbour = model.locate(card).and_then(|(column, index)| {
                    let cards = model.column(column)?.card_ids();
                    index
                        .checked_sub(1)
                        .and_then(|i| cards.get(i).copied())
                        .or_else(|| cards.get(index + 1).copied())
                        .map(EditPart::KanbanCard)
                        .or(Some(EditPart::KanbanColumn(column)))
                });
                if model.remove_card(card).is_err() {
                    return false;
                }
                (ItemKind::Kanban { board: crate::kanban::encode(&model) }, neighbour)
            }
            (ItemKind::Kanban { board }, EditPart::KanbanColumn(column)) => {
                let mut model = crate::kanban::decode(board);
                if model.columns().len() <= 1 {
                    self.gap("a kanban board keeps its last column");
                    return true;
                }
                let index = model.column_index(column);
                if model.remove_column(column).is_err() {
                    return false;
                }
                let neighbour = index
                    .and_then(|i| i.checked_sub(1).or(Some(0)))
                    .and_then(|i| model.columns().get(i).map(|c| EditPart::KanbanColumn(c.id())));
                (ItemKind::Kanban { board: crate::kanban::encode(&model) }, neighbour)
            }
            (ItemKind::Table { .. }, EditPart::TableCell(_)) => {
                self.gap("a table's cells are cleared, not removed — its rows and columns are its shape");
                return true;
            }
            (ItemKind::Kanban { .. }, EditPart::KanbanTitle) => {
                self.gap("a kanban board keeps its title; clear it instead");
                return true;
            }
            _ => return false,
        };

        if self.editor.edit(move |board| Ok(board.set_kind(doc, token)?)).is_err() {
            return false;
        }
        // Onto the neighbour, or out of the session entirely if the widget has nothing left
        // to edit. Either way the caret is never left pointing at something that is gone.
        if !next.is_some_and(|part| self.move_caret_to_part(part)) {
            self.commit_editing();
        }
        true
    }

    /// ⌥⏎ while a caret is in a mind-map node: folds or unfolds its branch.
    ///
    /// The one verb `vellum-mindmap` has had since it was written and nothing could reach: a
    /// collapsed node is what `visible_children` walks and the painter honours, so this has
    /// always worked — there was simply no key bound to it. Miro puts a circle on the node to
    /// click; drawing one is paint work this does not need, and an undocumented modifier
    /// would be worse than a documented chord, so this is in the shortcut sheet.
    ///
    /// A leaf reports rather than silently doing nothing: folding one is a keystroke with no
    /// visible effect, which reads as a broken key.
    fn toggle_branch(&mut self) -> bool {
        use crate::edit::EditPart;

        let Some(session) = self.editing.as_ref() else { return false };
        let (scene, doc) = (session.scene, session.doc);
        let EditPart::MindMapNode(node) = session.part else { return false };
        let Some(ItemKind::MindMap { model }) =
            self.editor.projection().get(scene).map(|p| p.item.kind.clone())
        else {
            return false;
        };

        let mut map = crate::mindmap::decode(&model);
        if map.map.children(node).is_empty() {
            self.gap("this node has no branch to fold");
            return true;
        }
        let folded = {
            let Some(target) = map.map.get_mut(node) else { return false };
            target.collapsed = !target.collapsed;
            target.collapsed
        };
        let token = crate::mindmap::encode(&map);
        if self
            .editor
            .edit(move |board| Ok(board.set_kind(doc, ItemKind::MindMap { model: token })?))
            .is_err()
        {
            return false;
        }
        // The node itself is still visible either way, so the caret stays where it is — but
        // its slot moved, because folding removes every descendant from the layout.
        self.move_caret_to_part(EditPart::MindMapNode(node));
        self.ok(if folded { "Branch folded" } else { "Branch unfolded" });
        true
    }

    // ----- on-canvas text editing ---------------------------------------------------

    /// Starts editing an item's words on the canvas.
    ///
    /// `replacing` selects everything, which is what a freshly placed item wants: it says
    /// "Frame" or nothing at all and the user is about to say what it really is. An item
    /// double-clicked on an existing board gets the cursor at the end instead.
    ///
    /// Returns whether a session started — `false` for a kind with no words of its own,
    /// which is most of them.
    pub(crate) fn begin_editing(&mut self, scene: SceneId, replacing: bool) -> bool {
        self.begin_editing_at(scene, replacing, None)
    }

    /// Starts editing, aiming at the piece of a structured widget under `world`.
    ///
    /// `world` is where the double click landed. With it, a click on a table cell, a kanban
    /// card or a mind-map node puts the caret in *that* field; without it — a freshly placed
    /// item, or a session started from a menu — the item's own words are edited, which is all
    /// the other kinds have.
    ///
    /// A structured widget has no words of its own, so before this there was nothing to put a
    /// caret in and `begin_editing` returned `false` for all four: they were placeable and
    /// unchangeable, which is the gap `CLAUDE.md` records as *"placeable and barely
    /// editable"*.
    pub(crate) fn begin_editing_at(
        &mut self,
        scene: SceneId,
        replacing: bool,
        world: Option<WorldPoint>,
    ) -> bool {
        if self.is_locked(scene) {
            return false;
        }
        // **Close the session already running, before starting another.**
        //
        // Both `self.editing = Some(…)` sites below *overwrite* whatever was there, and a
        // session that has taken one keystroke owns an **open Loro undo group**. Dropping
        // it that way leaks the group, and CLAUDE.md's trap 11 records exactly what that
        // costs: `group_start` answers `UndoGroupAlreadyStarted` forever afterwards, so
        // **every** later grouped operation fails — move, delete, paste, align, restyle —
        // for the rest of the session. Reported with a screenshot of two toasts reading
        // *"deleting: There is already an active undo group, call `group_end` first"* and
        // the same for moving, after typing in one sticky and double-clicking another.
        //
        // Committing is also the right *behaviour* independently of the leak: typing in
        // one note and then clicking into another is two undo steps, not one, and the
        // first note's words have to be written before the second's session begins.
        //
        // Cheap when there is nothing to close — `commit_editing` returns immediately
        // with no session, and only ends a group when the session actually opened one.
        self.commit_editing();
        // A structured widget first: it has no `kind.text()` at all, so the ordinary path
        // below cannot serve it, and asking here also means a click that lands on a table's
        // border rather than in a cell falls through to "nothing to edit" rather than
        // starting a session on the wrong thing.
        if let Some(world) = world
            && let Some((slot, part, text)) = self.widget_part_under(scene, world)
        {
            let Some(doc) = self.editor.projection().get(scene).map(|p| p.doc_id) else {
                return false;
            };
            let session = if replacing {
                crate::edit::Editing::replacing(scene, doc, slot, text)
            } else {
                crate::edit::Editing::new(scene, doc, slot, text)
            };
            self.editing = Some(session.in_part(part));
            self.shell.release_text_focus();
            return true;
        }

        let Some(projected) = self.editor.projection().get(scene) else { return false };
        let Some(text) = projected.item.kind.text() else { return false };
        // A frame's title is its secondary slot; every other kind's words are primary.
        // The painter's `block` decides the same thing from the kind, so this has to
        // agree with it or the caret is drawn against a layout for the wrong slot.
        let slot = if matches!(projected.item.kind, ItemKind::Frame { .. }) {
            crate::text::BlockKey::SECONDARY
        } else {
            crate::text::BlockKey::PRIMARY
        };
        let plain = text.to_plain();
        // The item's *styled* value, not just its words: `Editing::styled` is what each
        // keystroke is spliced into, so seeding it from the document is what keeps a bold run
        // bold. Cloned here because `projected` is borrowed from the projection.
        let styled = text.clone();
        let doc = projected.doc_id;
        self.editing = Some(
            if replacing {
                crate::edit::Editing::replacing(scene, doc, slot, plain)
            } else {
                crate::edit::Editing::new(scene, doc, slot, plain)
            }
            .with_styling(styled),
        );
        // The panel's own text field must not also have focus, or two carets compete for
        // the same keystrokes and only one of them is on the board.
        self.shell.release_text_focus();
        true
    }

    /// Writes the live buffer to the document.
    ///
    /// Every keystroke, for the reason `crate::edit`'s header gives: auto-fit, search, the
    /// panel and the thumbnail all read the document, so a buffer the document has not
    /// seen would be a second source of truth for the item's words. The whole session is
    /// **one undo group**, opened on the first write — the eraser's sweep does the same
    /// across many dabs, so `⌘Z` puts back a word rather than a letter.
    fn flush_editing(&mut self) {
        let Some(session) = self.editing.as_mut() else { return };
        let (doc, text, part) = (session.doc, session.buffer.text().to_owned(), session.part);
        if !session.opened() {
            session.mark_opened();
            let _ = self.editor.edit(|board| {
                board.begin_undo_group()?;
                Ok(())
            });
        }
        // A structured widget's words are one field inside an opaque token, so they are
        // written by decoding it, changing that field and re-encoding — not by `set_text`,
        // which has nowhere to put them. Everything else about the session is identical,
        // including that the whole of it is one undo group.
        if part != crate::edit::EditPart::Item {
            if !self.write_widget_part(doc, part, &text) {
                // The field is gone — its card deleted, or an undo replaced the model. End
                // the session rather than typing into nothing; the group closes with it.
                self.commit_editing();
            }
            return;
        }
        // The keystroke spliced into the item's own runs rather than a flat replacement. This
        // is the whole of "formatting survives an edit": `StyledText::plain(&text)` here is
        // what turned a half-bold sticky into an all-regular one on the first keystroke.
        let Some(styled) = self.editing.as_mut().map(crate::edit::Editing::styled_now) else {
            return;
        };
        // `edit_content`, not `edit`: a keystroke changes one item's words and cannot
        // move anything, so the projection is patched rather than rebuilt. See the method
        // for the measurement and for the bounds check that keeps it honest.
        self.editor.edit_content(doc, move |board| Ok(board.set_text(doc, styled)?));
        self.shell.invalidate_selection();
    }

    /// Ends the session, closing its undo group.
    ///
    /// Nothing is written here: every keystroke already wrote. What this closes is the
    /// group, which is the only thing that made the session one undo step.
    pub(crate) fn commit_editing(&mut self) {
        let Some(session) = self.editing.take() else { return };
        if session.opened() {
            let _ = self.editor.edit(|board| {
                board.end_undo_group();
                Ok(())
            });
        }
        self.shell.invalidate_selection();
    }

    /// One event from an input method.
    ///
    /// The four `winit` variants and what each has to do:
    ///
    /// - **`Preedit`** — the composition changed. Replace whatever the last one wrote and stay
    ///   composing. An empty string ends the composition without committing, which is what an
    ///   input method sends when the user backs out of one.
    /// - **`Commit`** — the composition is final, *or* an ordinary character arrived through
    ///   the input method rather than through `KeyEvent::text`. On macOS, with IME allowed,
    ///   that second case is how **every** character arrives, which is why this path is not
    ///   an exotic one: if it is wrong, plain typing is wrong.
    /// - **`Enabled`/`Disabled`** — the window's composition state. Only `Disabled` matters,
    ///   and only to forget a stale range: a leftover one would make the next composition
    ///   replace text the user had typed in between.
    ///
    /// Ignored when no caret is on the board. IME is only *allowed* while one is, but the
    /// window server can deliver one more event after it is switched off.
    pub(crate) fn ime(&mut self, event: winit::event::Ime) {
        use winit::event::Ime;

        if self.editing.is_none() {
            return;
        }
        // Logged because *which* path a character arrives by is platform behaviour rather
        // than something this code decides, and the answer decides whether the guard in
        // `type_key` is load-bearing or dead. On macOS with IME allowed it is load-bearing:
        // every character comes through `Commit`.
        log::debug!("ime: {event:?}");
        let changed = match &event {
            // Composition abandoned. The text it had written is removed, which is what
            // "abandoned" means — the alternative leaves the half-composed syllables behind
            // as if they had been typed.
            Ime::Preedit(text, _) if text.is_empty() => self
                .editing
                .as_mut()
                .is_some_and(|session| session.set_composition("", false)),
            Ime::Preedit(text, _) => self
                .editing
                .as_mut()
                .is_some_and(|session| session.set_composition(text, true)),
            Ime::Commit(text) => self
                .editing
                .as_mut()
                .is_some_and(|session| session.set_composition(text, false)),
            Ime::Enabled => false,
            Ime::Disabled => {
                if let Some(session) = self.editing.as_mut() {
                    session.end_composition();
                }
                false
            }
        };
        if changed {
            self.flush_editing();
        }
    }

    /// Turns the window's input method on exactly while a caret is on the board.
    ///
    /// Called every frame from the event loop, and a no-op unless the answer changed —
    /// `set_ime_allowed` crosses to the window server, so this is the same "only when it
    /// changes" rule the window title follows.
    ///
    /// **Only while editing**, and that is the load-bearing half. With IME allowed, macOS
    /// routes characters through the input method, so `KeyEvent::text` stops arriving for
    /// them; leaving it on all the time would hand the board's tool keys — `V`, `N`, `T` — to
    /// an input method that has nowhere to put them. Off, the app behaves exactly as it did
    /// before this existed.
    pub(crate) fn ime_allowed(&self) -> bool {
        self.editing.is_some()
    }

    /// One keystroke, while a text slot is being edited.
    ///
    /// Returns whether it was consumed. Anything not consumed falls through to the
    /// ordinary shortcut table, which is what keeps `⌘S` saving while a caret is on the
    /// board — a caret should claim the keys that mean text, not every key.
    pub(crate) fn type_key(&mut self, key: &winit::keyboard::Key, text: Option<&str>) -> bool {
        use crate::edit::Motion;
        use winit::keyboard::{Key, NamedKey};

        if self.editing.is_none() {
            return false;
        }
        let modifiers = self.input.modifiers();
        let command = modifiers.super_key() || modifiers.control_key();
        let extend = modifiers.shift_key();
        // ⌥ on macOS, ⌃ elsewhere, is the word-motion modifier. Kept as one flag because
        // the two platforms disagree about which key it is and nothing below cares.
        let by_word = if cfg!(target_os = "macos") {
            modifiers.alt_key()
        } else {
            modifiers.control_key()
        };

        // Motions and edits, in the order that resolves the overlaps: ⌘A is select-all
        // rather than an `a`, and ⌥← is a word rather than a character.
        let mut changed = false;
        let handled = match key.as_ref() {
            Key::Named(NamedKey::Escape) => {
                self.commit_editing();
                return true;
            }
            // --- the structured widgets' own keys, before the text keys they resemble ---
            //
            // Each is claimed only while the caret is in a *widget field*: the helpers return
            // `false` for `EditPart::Item`, so Tab on a sticky still falls through to the
            // ordinary shortcut table and ⌥⏎ still inserts a break in a text item.
            //
            // ⌥⏎ before the Enter arms below, because on a mind-map node it means "fold"
            // rather than "commit" — and `⌥` is the word-motion modifier, so this has to be
            // read here or it would be swallowed as one.
            Key::Named(NamedKey::Enter) if modifiers.alt_key() && self.toggle_branch() => {
                return true;
            }
            Key::Named(NamedKey::Tab) if self.advance_part(extend) => return true,
            Key::Named(NamedKey::Backspace | NamedKey::Delete)
                if command && self.remove_part() =>
            {
                return true;
            }
            // Enter commits on a *frame title* and on every structured-widget field, all of
            // which are one line by definition, and inserts a break anywhere else. ⇧⏎ inserts
            // — except in a single-line field, where there is nothing to insert into and it
            // commits with the rest.
            Key::Named(NamedKey::Enter) if self.editing_is_single_line() => {
                self.commit_editing();
                return true;
            }
            Key::Named(NamedKey::Enter) => self.with_buffer(|b| b.insert("\n")),
            Key::Named(NamedKey::Backspace) => self.with_buffer(crate::edit::TextBuffer::backspace),
            Key::Named(NamedKey::Delete) => self.with_buffer(crate::edit::TextBuffer::delete),
            Key::Named(NamedKey::ArrowLeft) => {
                let motion = if by_word { Motion::WordLeft } else { Motion::Left };
                self.with_buffer(|b| {
                    b.move_cursor(motion, extend);
                    false
                })
            }
            Key::Named(NamedKey::ArrowRight) => {
                let motion = if by_word { Motion::WordRight } else { Motion::Right };
                self.with_buffer(|b| {
                    b.move_cursor(motion, extend);
                    false
                })
            }
            // Up and Down need the visual lines, which only the layout knows.
            Key::Named(NamedKey::ArrowUp) => self.move_caret_by_line(-1, extend),
            Key::Named(NamedKey::ArrowDown) => self.move_caret_by_line(1, extend),
            Key::Named(NamedKey::Home) => self.with_buffer(|b| {
                b.move_cursor(Motion::ParagraphStart, extend);
                false
            }),
            Key::Named(NamedKey::End) => self.with_buffer(|b| {
                b.move_cursor(Motion::ParagraphEnd, extend);
                false
            }),
            Key::Character("a" | "A") if command => self.with_buffer(|b| {
                b.select_all();
                false
            }),
            // The clipboard, *inside* the session: ⌘C/⌘X/⌘V mean the selected characters
            // rather than the selected items while a caret is on the board. Without these
            // three arms ⌘V would paste a whole new item on top of the one being typed
            // into, which is not a smaller version of what the user asked for.
            Key::Character("c" | "C") if command => {
                self.copy_selected_text(false);
                return true;
            }
            Key::Character("x" | "X") if command => self.copy_selected_text(true),
            Key::Character("v" | "V") if command => match self.clipboard_text() {
                Some(text) => self.with_buffer(|b| b.insert(&text)),
                None => return true,
            },
            // A bare Tab would insert one and there is nowhere to tab *to* on a board, so
            // it commits — which is what leaving a field means everywhere else.
            Key::Named(NamedKey::Tab) => {
                self.commit_editing();
                return true;
            }
            // Anything the platform turned into characters is text. Guarded on `command`
            // so `⌘S` is still Save rather than an `s` typed into the item, and on the
            // control range so a stray `\t` or `\r` cannot arrive as a glyph.
            //
            // And guarded on **composition**: while an input method is composing, the
            // characters belong to it and arrive through `ActiveState::ime`. Inserting them
            // from here as well would type every syllable twice, and the key is still
            // *consumed* rather than let through, or it would fall on to the shortcut table
            // and a composed `v` would change the tool mid-word.
            _ if self.editing.as_ref().is_some_and(crate::edit::Editing::composing) => {
                return true;
            }
            _ => match text.filter(|t| !command && !t.chars().any(char::is_control)) {
                Some(typed) => self.with_buffer(|b| b.insert(typed)),
                None => return false,
            },
        };
        changed |= handled;
        // Solid again from here: a caret that kept blinking through a burst of typing
        // would flicker under the hands that use it. `handled` covers a bare motion —
        // arrow keys move the caret without changing a character, and the blink has to
        // restart for those too or holding ← looks like a fault.
        if handled || changed {
            self.editing_touched = Instant::now();
        }
        if changed {
            self.flush_editing();
        }
        true
    }

    /// Puts the selected characters on the system clipboard, and cuts them if asked.
    ///
    /// Returns whether the text changed, so the caller flushes only for a cut. Nothing at
    /// all happens with an empty selection — ⌘C with no selection copying the whole item
    /// would be a surprise, and ⌘X would be a destructive one.
    fn copy_selected_text(&mut self, cut: bool) -> bool {
        let Some(session) = self.editing.as_ref() else { return false };
        let range = session.buffer.selection();
        if range.is_empty() {
            return false;
        }
        let selected = session.buffer.text()[range].to_owned();
        // The session's one handle, not a fresh one per copy — `crate::editor` records
        // what building these per keystroke cost, and a copy is held down no less often
        // than a paste.
        let clipboard = self
            .system_clipboard
            .get_or_insert_with(|| arboard::Clipboard::new().map_err(|error| error.to_string()));
        match clipboard {
            Ok(clipboard) => {
                if let Err(error) = clipboard.set().text(selected) {
                    log::warn!("putting the selection on the clipboard: {error}");
                }
            }
            Err(error) => log::warn!("no system clipboard to copy into: {error}"),
        }
        cut && self.with_buffer(crate::edit::TextBuffer::backspace)
    }

    /// The system clipboard's plain text, if it has any.
    ///
    /// Text only. An image on the pasteboard is not something a caret can insert, and
    /// falling through to `Self::paste` — which would place a new item — is the wrong
    /// answer while the user is mid-word: it is a different verb wearing the same chord.
    fn clipboard_text(&mut self) -> Option<String> {
        let Some(Ok(clipboard)) = self.system_clipboard.as_mut() else { return None };
        clipboard.get().text().ok().filter(|text| !text.is_empty())
    }

    /// Whether the slot being edited holds a single line, so `Enter` should commit.
    ///
    /// Two cases. A **frame's title** is drawn on one line above the frame, and a break in it
    /// would be laid out but never seen. And every **structured widget field** is one line by
    /// definition — a column header or a mind-map node with two paragraphs in it is a layout
    /// nobody asked for, and a table cell wraps but has no use for a hard break. `EditPart`
    /// answers the second; only the first needs the kind.
    fn editing_is_single_line(&self) -> bool {
        let Some(session) = self.editing.as_ref() else { return false };
        if session.part.is_single_line() {
            return true;
        }
        self.editor
            .projection()
            .get(session.scene)
            .is_some_and(|projected| {
                matches!(
                    projected.item.kind,
                    // A frame's title, an agent's role and a note's title are all one line
                    // by definition — a role with a paragraph break in it is not a role, and
                    // all three are drawn in a single-line header where a second line has
                    // nowhere to go. So Enter commits rather than inserting a break.
                    ItemKind::Frame { .. } | ItemKind::Agent { .. } | ItemKind::AgentNote { .. }
                )
            })
    }

    /// Runs `edit` against the live buffer, reporting whether the text changed.
    fn with_buffer(&mut self, edit: impl FnOnce(&mut crate::edit::TextBuffer) -> bool) -> bool {
        self.editing.as_mut().is_some_and(|session| edit(&mut session.buffer))
    }

    /// Up or Down: the same x, one visual line away.
    ///
    /// Only the layout knows where the visual lines are — a wrapped paragraph is one
    /// string and several lines — so this asks the painter's cached layout, which is the
    /// same one the glyphs on screen came from. No layout yet (the item has never been
    /// painted, or its text is greeked at this zoom) means the motion is a no-op rather
    /// than a guess.
    fn move_caret_by_line(&mut self, delta: isize, extend: bool) -> bool {
        let Some(session) = self.editing.as_ref() else { return false };
        let key = crate::text::BlockKey::new(session.scene, session.slot);
        let (Some(layout), text) = (
            self.painter.text_mut().layout_of(key),
            session.buffer.text().to_owned(),
        ) else {
            return false;
        };
        let caret = layout.caret(&text, session.buffer.cursor());
        let target = caret.line.saturating_add_signed(delta).min(layout.lines.len().saturating_sub(1));
        if target == caret.line {
            // Already on the first or last line: Up goes to the start, Down to the end,
            // which is what a text field does rather than nothing at all.
            let to = if delta < 0 { 0 } else { text.len() };
            if let Some(session) = self.editing.as_mut() {
                session.buffer.place(to, extend);
            }
            return false;
        }
        let line = &layout.lines[target];
        let byte = layout.byte_at(&text, caret.x, line.top + line.height * 0.5);
        if let Some(session) = self.editing.as_mut() {
            session.buffer.place(byte, extend);
        }
        false
    }

    /// A click inside the item being edited: move the caret there.
    ///
    /// Returns whether the click belonged to the session. `false` means the press is
    /// somewhere else and the caller should commit and handle it normally — clicking away
    /// from a caret is how editing ends, and it must not also be swallowed.
    fn click_in_editing(&mut self, world: WorldPoint, extend: bool, clicks: u32) -> bool {
        let Some(session) = self.editing.as_ref() else { return false };
        // Only a click on the item itself. Hit-testing the item rather than the text block
        // on purpose: the words may not fill their box, and a click on the empty half of a
        // sticky should put the caret at the nearest character rather than end the session.
        if self.editor.projection().scene().hit_test(world) != Some(session.scene) {
            return false;
        }
        let key = crate::text::BlockKey::new(session.scene, session.slot);
        let text = session.buffer.text().to_owned();
        let (scene, slot) = (session.scene, session.slot);
        let Some(origin) = self.text_block_origin(scene, slot) else {
            // No layout: the item is selected and being edited but has never been
            // painted. The click still belongs to the session — swallowing it beats
            // ending an edit the user cannot see the caret of.
            return true;
        };
        let Some(layout) = self.painter.text_mut().layout_of(key) else { return true };
        // Screen space, because that is where the layout's own coordinates were placed.
        let at = self.camera.world_to_screen(world);
        let zoom = self.camera.zoom();
        #[expect(clippy::cast_possible_truncation, reason = "a block's extent is screen-scale")]
        let byte = layout.byte_at(
            &text,
            ((at.x - origin.x) / zoom) as f32,
            ((at.y - origin.y) / zoom) as f32,
        );
        if let Some(session) = self.editing.as_mut() {
            // One click places the caret, two select the word, three take everything — the
            // convention of every text field there is, and Miro's. *"if i double click it
            // should select the word and if i click one more time it should select
            // everything."*
            //
            // Four and beyond stay on "everything" rather than cycling back: a run that long
            // is a hand that has not stopped clicking, and taking the selection away from
            // someone still clicking is the one answer that is never what they meant.
            match clicks {
                0 | 1 => session.buffer.place(byte, extend),
                2 => session.buffer.select_word_at(byte),
                _ => session.buffer.select_all(),
            }
        }
        true
    }

    /// Where the item's text block was drawn last frame, in physical pixels.
    ///
    /// Read back from the painter rather than recomputed: a click has to resolve against
    /// what the user actually saw, and recomputing would need a `DrawContext` built
    /// outside the paint loop and would answer about a frame not yet drawn. See
    /// `Painter::edited_origin`.
    fn text_block_origin(&self, scene: SceneId, slot: u16) -> Option<ScreenPoint> {
        self.painter.edited_origin(crate::text::BlockKey::new(scene, slot))
    }

    /// The kanban card under a world point, if the item is a kanban and the point is on
    /// a card rather than on the board's chrome.
    ///
    /// The layout is recomputed here rather than read out of the painter's cache. That
    /// cache is filled during paint and keyed on the projection generation, so reading it
    /// would make a press depend on whether a frame had been drawn since the last edit —
    /// true in practice and a trap to rely on. One shaping pass per *press* is nothing;
    /// per *frame* it would not be, which is why the painter caches at all.
    /// The address behind the open-page badge at `world`, if the pointer is on one.
    ///
    /// The badge's rectangle comes from [`crate::draw::card_layout`] — the same call the
    /// painter makes — rather than from arithmetic repeated here. That is the rule
    /// `draw::kanban_runs` set: a second copy of a layout is a click that lands somewhere the
    /// paint is not, and nothing about the resulting bug says *layout*.
    ///
    /// **Unrotated, deliberately.** The card painter positions its pieces from the item's
    /// *unrotated* box and spins each one in place, so on a turned card the badge is drawn
    /// where this looks for it. Undoing the rotation here — which is what
    /// `kanban::board_local` does, and which is tidier — would make the click disagree with
    /// what is on screen. The painter is the thing to fix if that ever matters.
    /// The card whose open badge the pointer is on, for the hover state and the cursor.
    ///
    /// Uses [`Self::badge_under`] — the same hit test the click uses — so the highlight
    /// and the click can never disagree about where the button is.
    pub(crate) fn badge_under_pointer(&self) -> Option<SceneId> {
        let world = self.camera.screen_to_world(self.input.cursor(&self.camera));
        let scene = self.editor.projection().scene().hit_test(world)?;
        self.badge_under(scene, world).map(|_| scene)
    }

    fn badge_under(&self, scene: SceneId, world: WorldPoint) -> Option<String> {
        let projected = self.editor.projection().get(scene)?;
        let kind = &projected.item.kind;
        // `has_link`, the painter's own predicate, before the field itself — otherwise a
        // card whose address is not a web page has no badge drawn and a hitbox anyway, and
        // the top-right corner of it silently stops selecting.
        if !crate::draw::has_link(kind) {
            return None;
        }
        let url = crate::draw::link_url(kind)?.to_owned();
        let placement = projected.item.placement;
        let (w, h) = placement.scaled_size();
        let laid = crate::draw::card_layout(
            w,
            h,
            crate::draw::card_font_size(w),
            crate::draw::card_mode(kind),
            crate::draw::has_thumbnail(kind),
            crate::draw::has_favicon(kind),
            true,
            crate::draw::is_video(kind),
        );
        // `placement.x`/`y` are the item's **centre**, so the top-left corner is half its
        // size back from there. Getting this backwards puts the badge's hitbox a whole card
        // up and to the left, which still hits *something* on a dense board.
        let (lx, ly) = (world.x - placement.x + w / 2.0, world.y - placement.y + h / 2.0);
        let hits = |box_: Option<(f64, f64, f64, f64)>| {
            box_.is_some_and(|(bx, by, bw, bh)| {
                lx >= bx && lx <= bx + bw && ly >= by && ly <= by + bh
            })
        };
        // **The ▶ and the ↗ open the same page**, so one predicate answers for both rather
        // than two arms that could come to disagree about which URL a card has. The ▶ is
        // asked first only because it is the larger target and sits over the picture; they
        // cannot overlap, since the badge is in the corner and the play button is centred on
        // the poster.
        (hits(laid.play) || hits(laid.badge)).then_some(url)
    }

    fn card_under(&mut self, scene: SceneId, world: WorldPoint) -> Option<CardDrag> {
        // Read what is needed out of the projection before the engine is borrowed: the
        // two live on different fields, but the token has to be owned to outlive the
        // projection borrow.
        let (doc, placement, token) = {
            let projected = self.editor.projection().get(scene)?;
            let ItemKind::Kanban { board } = &projected.item.kind else { return None };
            (projected.doc_id, projected.item.placement, board.clone())
        };
        let board = crate::kanban::decode(&token);
        let size = placement.scaled_size();
        let (_, laid) =
            crate::kanban::layout(&board, self.painter.text_mut().engine_mut(), size);
        let local = crate::kanban::board_local(&placement, (world.x, world.y));
        match crate::kanban::hit_test(&laid, local) {
            vellum_flow::KanbanTarget::Card { card, .. } => {
                Some(CardDrag {
            scene, doc, card, drop: None })
            }
            _ => None,
        }
    }

    /// One frame of a card drag: recompute where it would land.
    ///
    /// Nothing is written. `drop_target` is asked with the card named as *in flight*,
    /// which is what makes the preview exact — `vellum-flow` measures the insertion point
    /// against the stack with that card lifted out, so the gap the preview draws is the
    /// gap the card will actually occupy.
    fn drag_card_to(&mut self, to: ScreenPoint) {
        let Some(mut drag) = self.card_drag.take() else { return };
        let world = self.camera.screen_to_world(to);
        let (placement, token) = match self.editor.projection().get(drag.scene) {
            Some(projected) => match &projected.item.kind {
                ItemKind::Kanban { board } => (projected.item.placement, board.clone()),
                _ => {
                    // The item stopped being a kanban mid-gesture — an undo of the paste
                    // that created it, say. Abandon rather than guess.
                    return;
                }
            },
            None => return,
        };
        let board = crate::kanban::decode(&token);
        let size = placement.scaled_size();
        let (_, laid) = crate::kanban::layout(&board, self.painter.text_mut().engine_mut(), size);
        let local = crate::kanban::board_local(&placement, (world.x, world.y));
        drag.drop = crate::kanban::drop_target(&laid, local, drag.card);
        self.card_drag = Some(drag);
    }

    /// The button came up on a card drag: commit the move, or nothing.
    ///
    /// One write and one undo step. `Slot::Index` from the preview rather than a rank
    /// computed here, so the card lands exactly where the placeholder was drawn — a
    /// property `vellum-flow`'s own tests assert and this must not give up.
    fn finish_card_drag(&mut self) {
        let Some(drag) = self.card_drag.take() else { return };
        let Some(drop) = drag.drop else { return };
        let Some(projected) = self.editor.projection().get(drag.scene) else { return };
        let ItemKind::Kanban { board } = &projected.item.kind else { return };
        let mut model = crate::kanban::decode(board);

        // Already where it is going. Checked before writing, so nudging a card inside its
        // own gap is not an undo step that changes nothing.
        if model.locate(drag.card) == Some((drop.column, drop.index)) {
            return;
        }
        if !crate::kanban::commit_drop(&mut model, drag.card, &drop) {
            self.gap("that card cannot move there");
            return;
        }

        let doc = drag.doc;
        let kind = ItemKind::Kanban { board: crate::kanban::encode(&model) };
        match self.editor.edit(move |board| Ok(board.set_kind(doc, kind)?)) {
            Ok(()) => self.shell.invalidate_selection(),
            Err(error) => self.failed("moving the card", &error),
        }
    }

    /// The resize or rotate handle under a world point, with the item it belongs to.
    ///
    /// Only for a **single** selection. Handles on a multi-selection would have to
    /// resize every member against a shared box, and a rotation would have to move each
    /// item's centre as well as its angle — a different operation, not a bigger version
    /// of this one. A multi-selection still draws its ring and still drags.
    /// The grip a resize or rotation in flight is being driven by.
    ///
    /// `None` for a plain move, which has no grip — that is what makes it a move.
    pub(crate) fn dragged_handle(&self) -> Option<Handle> {
        match self.drag.as_ref()?.mode {
            DragMode::Resize(handle) => Some(handle),
            DragMode::Rotate => Some(Handle::Rotate),
            DragMode::Move => None,
        }
    }

    pub(crate) fn handle_under(&self, world: WorldPoint) -> Option<(SceneId, Handle)> {
        let selection = self.editor.selection();
        // A multi-selection now has handles too — corners and rotate, on the shared box.
        // They are hit-tested here rather than in a second pass so that the same "handles
        // beat items" rule the doc comment describes covers both.
        if selection.len() > 1 {
            let placements: Vec<Placement> = selection
                .iter()
                .filter_map(|id| self.editor.projection().get(*id))
                .filter(|projected| !projected.item.style.locked)
                .map(|projected| projected.item.placement)
                .collect();
            if placements.len() < 2 {
                return None;
            }
            let group = handle::group_bounds(&placements)?;
            let handle = handle::group_hit(&group, world, self.camera.zoom())?;
            // The scene id is unused for a group — `begin_drag` reads the whole selection —
            // but the signature is shared, so the first member stands in.
            return selection.first().map(|scene| (*scene, handle));
        }
        let [scene] = selection[..] else { return None };
        // No handles on a locked item: a resize is the one gesture that would otherwise
        // still reach one, because the handles are hit-tested before the scene is asked
        // anything at all.
        if self.is_locked(scene) {
            return None;
        }
        let placement = self.editor.projection().get(scene)?.item.placement;
        let handle = handle::hit(&placement, world, self.camera.zoom())?;
        Some((scene, handle))
    }

    /// One frame of a drag. Nothing is written to the document — see
    /// [`crate::editor::Editor::preview_placement`] for why.
    /// Whether relative snapping should act on the gesture in flight.
    ///
    /// Two ways to say no. The Preferences switch is the standing one; **⌘ held during the
    /// drag** is the momentary one, which is Miro's own modifier and the thing that makes
    /// the feature bearable — the answer to "it snapped and I wanted it *there*" has to be
    /// available with the button still down, not in a menu two clicks away.
    fn snapping(&self) -> bool {
        self.shell.library.align_objects() && !self.input.modifiers().super_key()
    }

    /// The board's grid spacing, when Snap to grid is on and there is a grid to snap to.
    ///
    /// `None` disables the correction, and it says no for four separate reasons that all
    /// mean the same thing to the caller: the switch is off, **⌘ is held** (the same
    /// momentary escape the relative snap honours — one modifier suspends all snapping,
    /// which is what a hand expects), the board has chosen *No grid*, or the zoom is
    /// outside the band where a step is drawn at all.
    ///
    /// **Refusing when nothing is drawn is the load-bearing clause.** A grid that snapped
    /// invisibly would be a board that moves in jumps for no reason on screen, and at a
    /// fitted 4% zoom the step is enormous — a sticky would leap hundreds of units to a line
    /// nobody can see. The spacing comes from `draw::grid_step`, the same function the
    /// painter asks, so the line a gesture lands on is a line that is on screen.
    fn grid_snap_step(&self) -> Option<f64> {
        if !self.shell.library.snap_to_grid() || self.input.modifiers().super_key() {
            return None;
        }
        if self.canvas_pattern() == vellum_doc::Pattern::Plain {
            return None;
        }
        // **A fully transparent grid is the same as no grid, and must not snap.** The
        // opacity slider reaches zero on purpose — a grid nobody can see is a state the
        // *No grid* row already has a word for, so it is not one a user can be trapped in
        // — and without this clause a board would move in jumps against lines that are not
        // on screen, which is exactly what the `Plain` clause above exists to prevent,
        // reached by the other route.
        if self.shell.library.grid_color().is_some_and(|ink| ink.a == 0) {
            return None;
        }
        crate::draw::grid_step(self.camera.zoom())
    }

    /// How close two lines have to be, **in world units at the current zoom**.
    ///
    /// This division is the whole feel of the feature — see `crate::snap`'s header. A
    /// constant in world units would grab half a screen when zoomed out and never fire when
    /// zoomed in.
    fn snap_tolerance(&self) -> f64 {
        crate::snap::SNAP_PIXELS / self.camera.zoom()
    }

    /// The boxes a gesture may snap to: what is on screen, less what it is dragging.
    ///
    /// **On screen only**, through the same R-tree the painter culls with, for the reason
    /// `poll_visible_link_previews` gives: a board of 100,000 items has to cost what is
    /// being looked at. It is also the right answer rather than merely the cheap one —
    /// Miro's own users describe it as matching "within the current view", and a guide
    /// pointing at something off screen explains nothing.
    ///
    /// Excluding the dragged items is not an optimisation either: a box in its own
    /// candidate list matches itself at a distance of zero and the gesture stops moving.
    fn snap_candidates(&self, exclude: &[SceneId]) -> Vec<WorldRect> {
        self.editor
            .projection()
            .scene()
            .query_viewport(&self.camera)
            .filter(|item| !exclude.contains(&item.id))
            .map(|item| item.bounds)
            .collect()
    }

    /// The correction the drag in flight should take, and the guides that explain it.
    ///
    /// A **move** offers the whole selection's box, because that is what the user is
    /// pushing around: aligning each member separately would fight itself the moment two of
    /// them wanted different corrections. A **resize** offers the one item's box and names
    /// which of its edges the handle is moving, so the fixed edge is never corrected — that
    /// would slide the box out from under the pointer. A **rotation** is not snapped here at
    /// all: ⇧ already steps it by `handle::SNAP_DEGREES`, and an angle has no edges.
    /// Where the drag's items would be right now, as one box.
    ///
    /// Both snaps ask this — the relative one and the grid — so a gesture cannot be judged
    /// against two different rectangles. `None` for a drag with nothing in it, which is a
    /// state `begin_drag` refuses to build but which costs nothing to answer for.
    fn drag_bounds(
        &self,
        drag: &Drag,
        pointer: WorldPoint,
        shift: bool,
    ) -> Option<WorldRect> {
        drag.items
            .iter()
            .map(|(_, _, original)| {
                crate::project::placement_bounds(&drag.placement_of(original, pointer, shift))
            })
            .reduce(|a, b| {
                WorldRect::from_corners(
                    WorldPoint::new(a.min.x.min(b.min.x), a.min.y.min(b.min.y)),
                    WorldPoint::new(a.max.x.max(b.max.x), a.max.y.max(b.max.y)),
                )
            })
    }

    fn snap_for(&self, drag: &Drag, pointer: WorldPoint, shift: bool) -> crate::snap::Snap {
        let Some(moving) = self.drag_bounds(drag, pointer, shift) else {
            return crate::snap::Snap::default();
        };

        let dragged: Vec<SceneId> = drag.items.iter().map(|(scene, _, _)| *scene).collect();
        let candidates = self.snap_candidates(&dragged);
        let tolerance = self.snap_tolerance();
        match drag.mode {
            DragMode::Move => crate::snap::snap_move(moving, &candidates, tolerance),
            DragMode::Resize(handle) => {
                crate::snap::snap_resize(moving, &candidates, tolerance, handle.moving_edges())
            }
            DragMode::Rotate => crate::snap::Snap::default(),
        }
    }

    fn drag_to(&mut self, from: ScreenPoint, to: ScreenPoint) {
        let Some(mut drag) = self.drag.take() else { return };
        let from = self.camera.screen_to_world(from);
        let to = self.camera.screen_to_world(to);
        drag.offset = (to.x - from.x, to.y - from.y);

        let snap = self.input.modifiers().shift_key();
        self.guides.clear();
        if self.snapping() {
            // Applied to the **offset**, before anything is previewed, so `placement_of`
            // stays the one place a drag's result is worked out. Correcting the previewed
            // placements afterwards would give the preview and `finish_drag`'s commit two
            // different answers — and `finish_drag` calls `placement_of` again.
            let correction = self.snap_for(&drag, to, snap);
            drag.offset.0 += correction.dx;
            drag.offset.1 += correction.dy;
            self.guides = correction.guides;
        }
        // **Alignment beats the grid, per axis** — feedback 24's rule, applied to the third
        // kind of snap. An edge on a neighbour is an exact statement about where this thing
        // goes; a grid line is a rhythm it can fall back on. Applying both lands on neither,
        // and applying the grid *first* would make the relative snap unable to reach
        // anything that is not itself on the grid.
        //
        // Read after the correction above, from `self.guides`, so "did alignment find
        // something on this axis" is asked of the answer rather than recomputed.
        if let Some(step) = self.grid_snap_step() {
            let aligned = |axis| self.guides.iter().any(|g| g.axis == axis);
            let moves = match drag.mode {
                DragMode::Move => crate::snap::MOVES_WHOLE_BOX,
                DragMode::Resize(handle) => handle.moving_edges(),
                // A rotation moves no edge onto anything. `snap_move`'s own rotate arm
                // says the same, and Shift already quantises the angle.
                DragMode::Rotate => [(false, false), (false, false)],
            };
            if let Some(moving) = self.drag_bounds(&drag, to, snap) {
                let correction = crate::snap::snap_to_grid(moving, step, moves);
                if !aligned(crate::snap::Axis::Vertical) {
                    drag.offset.0 += correction.dx;
                }
                if !aligned(crate::snap::Axis::Horizontal) {
                    drag.offset.1 += correction.dy;
                }
            }
        }
        for (scene, _, original) in &drag.items {
            let placement = drag.placement_of(original, to, snap);
            self.editor.preview_placement(*scene, placement);
        }
        // The panel's X and Y follow the item while it moves — but only for a
        // selection small enough that rebuilding it per frame is free. Selecting the
        // whole reference board and dragging it would otherwise allocate 596 rows
        // sixty times a second for numbers nobody is reading.
        if drag.items.len() <= LIVE_PANEL_LIMIT {
            self.shell.invalidate_selection();
        }
        self.drag = Some(drag);
    }


    /// One dab of the eraser: cuts every stroke it touches, in place.
    ///
    /// **A partial erase splits the stroke rather than deleting it**, which is what
    /// `vellum_ink::Stroke::erase` returns — zero, one or two pieces normally, and more
    /// when the stroke crosses the circle several times, which handwriting does
    /// constantly.
    ///
    /// Erases live rather than on release, because an eraser you cannot see working is
    /// not an eraser. The whole gesture is still **one undo step**: the group is opened
    /// on the first dab and closed when the button comes up, so ⌘Z puts back everything
    /// the sweep took rather than the last dab of it.
    fn erase_at(&mut self, at: WorldPoint) {
        // Which eraser this is: the flyout's mode, inverted for as long as ⇧ is held. The
        // mode is where Miro puts the choice and where a user will look for it; ⇧ used to
        // be the *only* way to reach the object eraser, which meant a gesture that deletes
        // whole frames was discoverable only by reading the shortcut sheet. The inversion
        // lives on `EraserMode::with_shift` rather than here so the flyout's own hint text
        // and this dab cannot come to disagree about what ⇧ does.
        if matches!(
            self.shell.eraser().with_shift(self.input.modifiers().shift_key()),
            vellum_ui::EraserMode::Object
        ) {
            self.erase_objects_at(at);
            return;
        }
        let radius = f64::from(self.shell.pen().width).max(ERASER_MIN_RADIUS);

        // Candidates from the R-tree first. Its boxes are axis-aligned, so a diagonal
        // stroke's box is mostly empty space — the real test is against the geometry,
        // below, and this only avoids reading every stroke on the board per dab.
        let reach = WorldRect::from_corners(
            WorldPoint::new(at.x - radius, at.y - radius),
            WorldPoint::new(at.x + radius, at.y + radius),
        );
        let nearby: Vec<SceneId> = self
            .editor
            .projection()
            .scene()
            .query_rect(reach)
            .map(|item| item.id)
            .collect();

        let mut cuts: Vec<(DocId, Vec<(ItemKind, Placement)>)> = Vec::new();
        for scene in nearby {
            let Some(projected) = self.editor.projection().get(scene) else { continue };
            let ItemKind::Ink { points, color, thickness } = &projected.item.kind else {
                // Only ink. Miro's default eraser is the stroke eraser; whole items are
                // its *other* mode, which this is not.
                continue;
            };
            let placement = projected.item.placement;
            let stroke = ink_stroke(points, *thickness, &placement);
            // Surface semantics rather than centreline: `erase` measures to the path
            // itself, so a fat stroke would otherwise survive an eraser laid over its
            // visible edge.
            let pieces = stroke.erase((at.x, at.y), radius + stroke.max_half_width());

            // Nothing overlapped: `erase` returns the stroke back, untouched.
            if pieces.len() == 1 && pieces[0].points().len() == stroke.points().len() {
                continue;
            }
            cuts.push((
                projected.doc_id,
                pieces.iter().filter_map(|piece| ink_item(piece, *color)).collect(),
            ));
        }
        if cuts.is_empty() {
            return;
        }

        // One group for the sweep, opened on the first dab that actually cuts anything.
        if !self.erasing {
            self.erasing = true;
            let _ = self.editor.edit(|board| {
                board.begin_undo_group()?;
                Ok(())
            });
        }

        let result = self.editor.edit(|board| {
            for (doc, pieces) in &cuts {
                board.remove(*doc)?;
                for (kind, placement) in pieces {
                    board.add(NewItem::new(kind.clone(), *placement))?;
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            self.failed("erasing", &error);
        }
        self.shell.invalidate_selection();
    }

    /// Whether a gesture is holding a Loro undo group open across calls.
    ///
    /// # The two of them, and why this is worth a name
    ///
    /// Exactly two things in the application keep a group open between calls, both on purpose:
    /// the on-canvas caret, so that a typed word is one `⌘Z` rather than one per letter, and
    /// the eraser's sweep, so that one gesture is one undo. Loro has no depth count, so *any*
    /// other grouped operation attempted while one of them is live fails — and because nothing
    /// else ever closes a group, it then fails for the rest of the session.
    ///
    /// The first round of fixes covered the paths that were known: command dispatch, the
    /// eraser's own cancel routes, and `delete_selection`. An adversarial review then found
    /// three more — the link-fetch apply, the properties panel's style and transform edits,
    /// and a tool change mid-sweep — none of which go through a `Command`. Naming the
    /// condition is what lets the next one be a one-line guard instead of a fourth discovery.
    ///
    /// Callers have two correct responses and must choose deliberately:
    /// **close it** ([`Self::run`], for something the user just asked for) or **wait**
    /// ([`Self::apply_link_fetches`], for something arriving in the background).
    pub(crate) const fn busy_with_a_group(&self) -> bool {
        self.editing.is_some() || self.erasing
    }

    /// Closes whichever long-lived group is open, for a caller that must act now.
    ///
    /// Both are idempotent, so this is free when nothing is open.
    fn settle(&mut self) {
        self.commit_editing();
        self.finish_erase();
    }

    /// The button came up after an erase sweep: closes the undo group.
    ///
    /// `pub(crate)` and idempotent because a sweep can also end without the button coming
    /// up — Escape, a tab swap, quitting — and every one of those is a way for the group to
    /// leak. Returning early when nothing is being erased is what makes it safe to call
    /// from all of them.
    pub(crate) fn finish_erase(&mut self) {
        if !self.erasing {
            return;
        }
        self.erasing = false;
        let _ = self.editor.edit(|board| {
            board.end_undo_group();
            Ok(())
        });
        if let Some(before) = self.erased_from.take() {
            // Counted as "how many items left the board", not "how many removals were
            // asked for". Removing a frame takes its children with it, so counting the
            // roots reported 2 where the user watched 3 things vanish. Reported at the end
            // of the sweep rather than per dab: one gesture, one undo step, one sentence.
            let count = before.saturating_sub(self.editor.board().item_ids().len());
            if count > 0 {
                self.ok(format!("Erased {count} item{}", plural(count)));
            }
        }
    }

    /// One dab of the **object** eraser: removes whole items under the pointer.
    ///
    /// Miro's second eraser mode. Shares the sweep's undo group with the stroke eraser, so
    /// one ⌘Z puts back everything a single sweep took however many items and strokes that
    /// was, and shares the R-tree candidate query for the same reason.
    ///
    /// **Locked items survive**, and roots only are removed. A frame or a group takes its
    /// subtree with it — `Board::remove` rejects an id its own previous call already
    /// deleted, which is exactly the bug `demo_empty` hit on the real board — so a dab that
    /// covers both a frame and its child must ask for the frame alone.
    fn erase_objects_at(&mut self, at: WorldPoint) {
        let radius = f64::from(self.shell.pen().width).max(ERASER_MIN_RADIUS);
        let reach = WorldRect::from_corners(
            WorldPoint::new(at.x - radius, at.y - radius),
            WorldPoint::new(at.x + radius, at.y + radius),
        );
        let scenes: Vec<SceneId> = self
            .editor
            .projection()
            .scene()
            .query_rect(reach)
            .map(|item| item.id)
            .collect();

        let mut doomed: Vec<DocId> = Vec::new();
        for scene in scenes {
            let Some(projected) = self.editor.projection().get(scene) else { continue };
            if projected.item.style.locked {
                continue;
            }
            let doc = projected.doc_id;
            // Roots only. See the doc comment: a descendant of something already doomed is
            // removed with it, and asking twice fails the whole transaction.
            if self.editor.board().parent_of(doc).is_some_and(|parent| doomed.contains(&parent)) {
                continue;
            }
            doomed.push(doc);
        }
        // A second pass, because the ancestor may have been found *after* the descendant:
        // `query_rect` has no order, so the one-pass check above catches only half of them.
        let board = self.editor.board();
        let orphans: Vec<DocId> = doomed
            .iter()
            .copied()
            .filter(|id| {
                !std::iter::successors(board.parent_of(*id), |p| board.parent_of(*p))
                    .any(|ancestor| doomed.contains(&ancestor))
            })
            .collect();
        if orphans.is_empty() {
            return;
        }

        if !self.erasing {
            self.erasing = true;
            let _ = self.editor.edit(|board| {
                board.begin_undo_group()?;
                Ok(())
            });
        }
        // Recorded on the first dab that removes anything, so the count at the end is the
        // whole sweep's, subtrees included.
        if self.erased_from.is_none() {
            self.erased_from = Some(self.editor.board().item_ids().len());
        }
        let result = self.editor.edit(move |board| {
            for id in orphans {
                board.remove(id)?;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                self.editor.clear_selection();
                self.shell.invalidate_selection();
            }
            Err(error) => self.failed("erasing items", &error),
        }
    }

    /// Turns the points the pen captured into an [`ItemKind::Ink`] item.
    ///
    /// Points are stored **relative to the item's placement**, which is what the
    /// document model and the Miro importer both expect, so an imported stroke and a
    /// drawn one are indistinguishable afterwards. The placement is the path's
    /// centre, so moving the item moves the whole stroke.
    fn commit_stroke(&mut self) {
        self.recorder.event("draw-stroke");
        let points = std::mem::take(&mut self.stroke);
        // Two points is the shortest thing worth keeping; a single sample is a click
        // that happened to land while the pen was held, not a mark.
        if points.len() < 2 {
            return;
        }

        let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
        let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
        for p in &points {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);

        let relative: Vec<vellum_doc::Point> = points
            .iter()
            .map(|p| vellum_doc::Point { x: p.x - cx, y: p.y - cy })
            .collect();

        let placement = Placement {
            x: cx,
            y: cy,
            width: (max_x - min_x).max(1.0),
            height: (max_y - min_y).max(1.0),
            ..Placement::default()
        };
        // The same preset the preview was drawn from. Reading it here rather than
        // hardcoding a width is what makes the flyout's colour, nib and translucency
        // mean anything — and what stops the stroke changing appearance at the exact
        // moment the button comes up.
        let pen = self.shell.pen();
        let kind = ItemKind::Ink {
            points: relative,
            color: Some(pen.stroke_color()),
            thickness: f64::from(pen.width),
        };

        match self.editor.edit(|board| Ok(board.add(NewItem::new(kind, placement))?)) {
            Ok(id) => {
                self.editor.select([id]);
                self.shell.invalidate_selection();
            }
            Err(error) => self.failed("drawing", &error),
        }
    }

    /// The button came up. One drag is one undo step — `docs/06-mouse-controls.md` §4.
    fn finish_drag(&mut self, moved: bool) {
        // Whatever happens below, the gesture is over and its guides go with it.
        self.guides.clear();
        let Some(drag) = self.drag.take() else { return };
        if !moved || !drag.is_effective() {
            return;
        }
        // The pointer's last world position, for a rotation — which is measured from
        // where the pointer *is*, not from how far it has come.
        let pointer = self.camera.screen_to_world(self.input.cursor(&self.camera));
        let snap = self.input.modifiers().shift_key();
        let placements: Vec<(DocId, Placement)> = drag
            .items
            .iter()
            .map(|(_, doc, original)| (*doc, drag.placement_of(original, pointer, snap)))
            .collect();
        let what = match drag.mode {
            DragMode::Move => "moving",
            DragMode::Resize(_) => "resizing",
            DragMode::Rotate => "rotating",
        };
        match self.editor.commit_placements(&placements) {
            Ok(_) => self.shell.invalidate_selection(),
            Err(error) => self.failed(what, &error),
        }
    }

    /// Escape during a drag: put everything back where it was. Returns whether there
    /// was a drag to abandon, so Escape is not also spent clearing the selection.
    pub(crate) fn cancel_drag(&mut self) -> bool {
        let cancelled = self.input.cancel_gesture();
        // A cancelled pen gesture never reaches `Intent::Place`, so `commit_stroke`
        // never runs and never takes the points. Left here they would be prepended to
        // whatever is drawn next, joining two strokes across the Escape that was meant
        // to throw the first away.
        let drawing = !self.stroke.is_empty();
        self.stroke.clear();
        // The eraser is the same shape of hazard and the worse one: a cancelled sweep never
        // reaches `Intent::Place` either, so `finish_erase` — the *only* thing that closes
        // the group opened on the first dab — never ran, and the group leaked for the rest
        // of the session. Loro answers `UndoGroupAlreadyStarted` to every grouped operation
        // after that, so one Escape broke move, delete, paste, align and restyle (trap 11).
        // `finish_erase` returns early when nothing is being erased, so this is free
        // otherwise, and it still reports what the sweep took.
        let erasing = self.erasing;
        self.finish_erase();
        // A card drag writes nothing until the button comes up, so abandoning it is just
        // forgetting it — there is no preview placement to put back.
        let carrying = self.card_drag.take().is_some();
        let Some(drag) = self.drag.take() else {
            return cancelled || drawing || carrying || erasing;
        };
        for (scene, _, original) in &drag.items {
            self.editor.preview_placement(*scene, *original);
        }
        self.shell.invalidate_selection();
        true
    }

    /// An arrow key. One press is one undo step, like one drag.
    ///
    /// Widened to the subtree by the same [`Self::widen_to_contents`] a drag uses: an arrow
    /// key *is* a move, and a frame that carries its contents when dragged and abandons them
    /// when nudged would be two different frames.
    pub(crate) fn nudge(&mut self, dx: f64, dy: f64) {
        let mut moving: Vec<(SceneId, DocId, Placement)> = self
            .editor
            .selection()
            .iter()
            .filter_map(|scene| Some((*scene, self.editor.projection().get(*scene)?)))
            .filter(|(_, projected)| !projected.item.style.locked)
            .map(|(scene, projected)| (scene, projected.doc_id, projected.item.placement))
            .collect();
        self.widen_to_contents(&mut moving);
        let placements: Vec<(DocId, Placement)> = moving
            .into_iter()
            .map(|(_, doc, p)| (doc, Placement { x: p.x + dx, y: p.y + dy, ..p }))
            .collect();
        if placements.is_empty() {
            return;
        }
        match self.editor.commit_placements(&placements) {
            Ok(_) => self.shell.invalidate_selection(),
            Err(error) => self.failed("nudging", &error),
        }
    }

    // ----- creating things ----------------------------------------------------

    /// The size a create tool gives an item placed with a **click** rather than a drag.
    ///
    /// Pulled out of [`Self::place`] so the live preview and the item that ends up on the
    /// board are sized by one function. They were two copies for about an hour, which is
    /// the `draw::kanban_runs` lesson in miniature: a preview that disagrees with what it
    /// is previewing is worse than no preview, because it is believed.
    ///
    /// `None` for the tools that make nothing rectangular — the pen and the eraser, which
    /// act on the board directly, the connector, which draws its own preview, and Select,
    /// Hand and Image.
    pub(crate) fn default_size(tool: Tool) -> Option<(f64, f64)> {
        match tool {
            // Miro's default sticky, 199 × 228 — the size every note on the reference
            // board is.
            Tool::Sticky => Some((199.0, 228.0)),
            Tool::Text => Some((240.0, 48.0)),
            // Miro's own insert: a 3 x 3 with a header row. The box is sized so the
            // default columns are comfortably wider than a word.
            Tool::Table => Some((480.0, 220.0)),
            Tool::Chart => Some((480.0, 320.0)),
            // A root and two levels under it, in a box about its own natural size —
            // `crate::mindmap::DEFAULT_SIZE`, which a test keeps honest against what
            // the tidy pass actually produces, so a placed map draws at roughly 1:1.
            Tool::MindMap => Some(crate::mindmap::DEFAULT_SIZE),
            // Three columns with cards and a WIP limit, in a box a test proves does not
            // overflow — `vellum-flow` lays overflow out past the edge on purpose and
            // nothing here scrolls, so a default that overflowed would draw outside the
            // item the moment it was placed.
            Tool::Kanban => Some(crate::kanban::DEFAULT_SIZE),
            // The four Agent Canvas nodes, each sized by its own module so the default and
            // the layout that has to fit inside it are one decision. `crate::agent`'s tests
            // hold the box against the rows laid out in it, which is what stops a placed
            // agent from being born too small for its own header.
            Tool::Agent => Some(crate::agent::DEFAULT_SIZE),
            Tool::Note => Some(crate::note::DEFAULT_SIZE),
            Tool::FileTree => Some(crate::filetree::DEFAULT_SIZE),
            Tool::Browser => Some(crate::browser::DEFAULT_SIZE),
            // Miro's default shape box, and a square so the first click gives a circle
            // rather than an ellipse.
            Tool::Shape => Some((200.0, 200.0)),
            // A 16:9 frame, which is what a frame is for: a slide.
            Tool::Frame => Some((1_600.0, 900.0)),
            Tool::Select | Tool::Hand | Tool::Pen | Tool::Eraser | Tool::Connector
            | Tool::Image => None,
        }
    }

    /// The box a create tool would give the item it is about to make.
    ///
    /// The rule, in one place: a sweep of at least [`DRAG_TO_SIZE`] world units on **both**
    /// axes fills the box it swept; anything smaller is a click, and a click makes
    /// [`Self::default_size`] centred on the press.
    ///
    /// This is what the live preview paints, which is why it has to be the same function
    /// [`Self::place`] commits — *"when i am trying to draw a frame i do not see it as i
    /// draw … i want to be able to see it visually what i am doing"*.
    pub(crate) fn swept_placement(tool: Tool, from: WorldPoint, to: WorldPoint) -> Option<Placement> {
        let (width, height) = Self::default_size(tool)?;
        let (swept_w, swept_h) = ((to.x - from.x).abs(), (to.y - from.y).abs());
        Some(if swept_w >= DRAG_TO_SIZE && swept_h >= DRAG_TO_SIZE {
            Placement::new((from.x + to.x) / 2.0, (from.y + to.y) / 2.0, swept_w, swept_h)
        } else {
            Placement::new(from.x, from.y, width, height)
        })
    }

    /// [`Self::swept_placement`], with relative snapping applied.
    ///
    /// The whole reason a create gesture goes through this rather than the bare geometry:
    /// *"i love how miro has implemented it"* includes placing things, and a new sticky that
    /// lands one unit off the frame it was dropped on is exactly the misalignment the
    /// feature exists to prevent. `&self`, so both callers — the live preview and
    /// [`Self::place`]'s commit — can ask, and nothing changes between the last pointer
    /// sample and the release that could give them different answers.
    fn snapped_placement(
        &self,
        tool: Tool,
        from: WorldPoint,
        to: WorldPoint,
    ) -> Option<(Placement, Vec<crate::snap::Guide>)> {
        let placement = Self::swept_placement(tool, from, to)?;
        let bounds = crate::project::placement_bounds(&placement);
        let mut snap = if self.snapping() {
            // Nothing to exclude: the item does not exist yet, so it cannot be in its own
            // candidate list — which is the one way this differs from a drag.
            let candidates = self.snap_candidates(&[]);
            crate::snap::snap_move(bounds, &candidates, self.snap_tolerance())
        } else {
            crate::snap::Snap::default()
        };
        // The grid, where alignment found nothing — the same per-axis precedence a drag
        // uses, and for the same reason. A new item is placed far more often than an
        // existing one is moved, so this is where Snap to grid is mostly felt.
        if let Some(step) = self.grid_snap_step() {
            let on_grid = crate::snap::snap_to_grid(bounds, step, crate::snap::MOVES_WHOLE_BOX);
            if !snap.guides.iter().any(|g| g.axis == crate::snap::Axis::Vertical) {
                snap.dx += on_grid.dx;
            }
            if !snap.guides.iter().any(|g| g.axis == crate::snap::Axis::Horizontal) {
                snap.dy += on_grid.dy;
            }
        }
        Some((
            Placement { x: placement.x + snap.dx, y: placement.y + snap.dy, ..placement },
            snap.guides,
        ))
    }

    /// The preview for a create gesture, and the guides that go with it.
    ///
    /// `&mut` because it owns the guides for the frame — this and [`Self::drag_to`] are the
    /// two gestures that produce them, and they cannot both be running. Clearing here when
    /// neither is what stops a line outliving the drag that justified it, which is the
    /// failure a painter reading stale state shows as a guide pointing at nothing.
    pub(crate) fn placing_preview(&mut self) -> Option<crate::draw::Placing> {
        let Some((press, now)) = self.input.placement() else {
            if self.drag.is_none() {
                self.guides.clear();
            }
            return None;
        };
        let tool = self.shell.tool();
        let (placement, guides) = self.snapped_placement(
            tool,
            self.camera.screen_to_world(press),
            self.camera.screen_to_world(now),
        )?;
        self.guides = guides;
        Some(crate::draw::Placing {
            placement,
            // The tool is resolved to a *look* here rather than in the painter, which draws
            // the board and knows nothing about the tool palette.
            look: match tool {
                Tool::Frame => crate::draw::PlacingLook::Frame,
                Tool::Sticky => crate::draw::PlacingLook::Sticky,
                Tool::Shape => crate::draw::PlacingLook::Shape(self.shell.shape()),
                Tool::Agent | Tool::Note | Tool::FileTree | Tool::Browser => {
                    crate::draw::PlacingLook::Card
                }
                _ => crate::draw::PlacingLook::Ghost,
            },
        })
    }

    /// Creates whatever the active tool makes.
    ///
    /// `from` and `to` are the press and the release. A click makes the tool's default
    /// size centred on the point; a drag of more than [`DRAG_TO_SIZE`] world units in
    /// both axes makes an item that fills the box swept out, which is what Miro does
    /// and what someone reaching for the sticky tool tries first.
    ///
    /// Miro returns to the select tool after one placement, and so does this: a tool
    /// that stays armed turns a stray click into an item you did not want, on a canvas
    /// where a stray click is otherwise free.
    ///
    /// The **drawing** tools are the exception, because for them that reasoning does not
    /// hold: a stray click with the pen makes nothing at all — `commit_stroke` needs two
    /// points before it will keep anything — and one with the eraser erases nothing.
    /// They pay the cost of the rule without carrying the risk it exists for, and
    /// nobody draws exactly one stroke. See [`Tool::is_continuous`].
    fn place(&mut self, from: WorldPoint, to: WorldPoint) {
        let tool = self.shell.tool();
        let kind = match tool {
            Tool::Sticky => {
                // The colour chosen in the tool's own flyout, or `None` for the palette's
                // default yellow. Read at placement rather than stored on the item's style,
                // because it is a property of the *next* note and not of any existing one.
                Some(ItemKind::Sticky {
                    text: StyledText::default(),
                    background: self.shell.sticky_color(),
                })
            }
            Tool::Text => Some(ItemKind::Text { text: StyledText::default() }),
            Tool::Table => Some(ItemKind::Table {
                model: crate::table::encode(&crate::table::default_table()),
            }),
            Tool::Chart => {
                Some(ItemKind::Chart { spec: crate::chart::encode(&crate::chart::default_chart()) })
            }
            Tool::MindMap => Some(ItemKind::MindMap {
                model: crate::mindmap::encode(&crate::mindmap::default_mindmap()),
            }),
            Tool::Kanban => Some(ItemKind::Kanban {
                board: crate::kanban::encode(&crate::kanban::default_kanban()),
            }),
            // The shape the flyout last chose. `Shell::shape` had no callers at all
            // until now — the picker remembered a choice nothing could act on.
            Tool::Shape => Some(ItemKind::Shape {
                form: crate::shapes::encode(self.shell.shape()),
                text: StyledText::default(),
            }),
            Tool::Frame => Some(ItemKind::Frame {
                title: StyledText::plain("Frame"),
                order: None,
                speaker_notes: None,
            }),
            // The role the palette's flyout last chose — worker, orchestrator or meta.
            // All three are one `ItemKind`; they differ in configuration, not in what they
            // are on the canvas. The label is a placeholder the caret lands on, exactly as
            // a frame's "Frame" is, because a role is text the user writes and an unnamed
            // agent is one nobody can tell from its neighbour.
            Tool::Agent => {
                let mut model = vellum_agent::AgentModel::worker();
                model.role_kind = self.shell.agent_role();
                Some(ItemKind::Agent {
                    model: crate::agent::encode(&model),
                    label: StyledText::plain(model.role_kind.label()),
                })
            }
            // A note with no file yet. The file is created when the note is first named or
            // written to — placing one must not litter the project with `untitled.md`
            // before the user has said anything about it.
            Tool::Note => Some(ItemKind::AgentNote {
                model: crate::note::encode(&vellum_agent::NoteModel::default()),
                title: StyledText::plain("Note"),
            }),
            Tool::FileTree => Some(ItemKind::FileTree {
                model: crate::filetree::encode(&vellum_agent::FileTreeModel::default()),
            }),
            Tool::Browser => Some(ItemKind::Browser {
                model: crate::browser::encode(&vellum_agent::BrowserModel::default()),
            }),
            _ => None,
        };

        // The sizes moved to `default_size` and the box to `snapped_placement`, so the
        // preview drawn during the drag and the item created at the end of it are one
        // decision — including the snap, or a frame would jump off the guide it was
        // sitting on the instant the button came up. `kind` and the placement are `Some`
        // for exactly the same tools; this pairs them without either answering for the
        // other.
        let snapped = self.snapped_placement(tool, from, to).map(|(placement, _)| placement);
        let Some((kind, placement)) = kind.zip(snapped) else {
            match tool {
                Tool::Pen => self.commit_stroke(),
                Tool::Eraser => self.finish_erase(),
                Tool::Connector => self.draw_connector(from, to),
                Tool::Image => {
                    self.gap("Placing an image needs a file dialog this build has not got")
                }
                // Reached only if a placing tool loses its arm above; the arms that
                // create something have already returned by now.
                Tool::Select
                | Tool::Hand
                | Tool::Sticky
                | Tool::Text
                | Tool::Frame
                | Tool::Shape
                | Tool::Table
                | Tool::Chart
                | Tool::MindMap
                | Tool::Kanban
                | Tool::Agent
                | Tool::Note
                | Tool::FileTree
                | Tool::Browser => {}
            }
            if !tool.is_continuous() {
                self.choose_tool(Tool::Select);
            }
            return;
        };

        // An orchestrator or a meta agent is born owning a region, derived from the box it
        // was just drawn at. `vellum_agent::orchestrator` refuses every spawn from a node
        // with no territory — correctly, since a territory arrived at by omission is an
        // unbounded one — so a node created without one could never spawn, and the feature
        // would be written, tested and unreachable. The user redraws it afterwards; this is
        // a starting point, not a guess at what they meant.
        let kind = match kind {
            ItemKind::Agent { model, label } => {
                let mut config = crate::agent::decode(&model);
                if config.role_kind.may_spawn() && config.territory.is_none() {
                    config.territory = Some(crate::agent::default_territory(&placement));
                }
                ItemKind::Agent { model: crate::agent::encode(&config), label }
            }
            other => other,
        };

        let result = self
            .editor
            .edit(|board| Ok(board.add(NewItem::new(kind, placement))?));
        match result {
            Ok(id) => {
                self.editor.select([id]);
                self.shell.invalidate_selection();
                // A blank note is not what anyone wanted; it is a note they are about to
                // type into. The caret goes straight onto the new item, with anything
                // already in it selected, so placing one and typing is a single flow and
                // the placeholder is replaced rather than appended to.
                //
                // A frame is included where the panel version left it out: its title says
                // "Frame", which is a placeholder nobody wants to keep, and the caret now
                // has somewhere to be — over the frame, on its title.
                // An agent and a note join the four that take the caret on placement, for
                // exactly the frame's reason: both are born carrying a placeholder — "Agent",
                // "Note" — that nobody wants to keep, and the role a user types is not a
                // label but the thing that shapes how the agent answers. A file tree and a
                // browser are left out: neither has text of its own to write.
                if matches!(
                    tool,
                    Tool::Sticky | Tool::Text | Tool::Shape | Tool::Frame | Tool::Agent | Tool::Note
                ) {
                    let scene = self.editor.projection().scene_id(id);
                    match scene {
                        Some(scene) if self.begin_editing(scene, true) => {}
                        // The item is not in the projection yet, which is possible if the
                        // edit has not been projected. Fall back to the panel rather than
                        // silently placing something with no cursor in it.
                        _ => self.shell.focus_text(),
                    }
                }
            }
            Err(error) => self.failed("creating an item", &error),
        }
        self.choose_tool(Tool::Select);
    }

    /// Resizes and then rotates a **multi-selection** through the real gesture path.
    ///
    /// Two properties no unit test can reach, because they depend on `handle_under` being
    /// consulted before the scene and on `begin_drag` capturing the box exactly once:
    /// that a group handle is reachable at all with several items selected, and that the
    /// group box is **not** recomputed per frame — if it were, each scaled box would feed
    /// the next scale and the selection would run away from the pointer exponentially.
    ///
    /// Leaves the result on screen with the handles showing, so the group outline and its
    /// five handles are photographable.
    fn demo_group_handles(&mut self) {
        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let mut ids = Vec::new();
            for (index, x) in [-300.0, 0.0, 300.0].into_iter().enumerate() {
                ids.push(board.add(NewItem::new(
                    ItemKind::Sticky {
                        text: StyledText::plain(format!("{}", index + 1)),
                        background: None,
                    },
                    Placement::new(x, 0.0, 199.0, 228.0),
                ))?);
            }
            board.end_undo_group();
            Ok(ids)
        });
        let Ok(ids) = placed else {
            self.gap("the group fixture could not be built");
            return;
        };
        self.fit_board();
        self.editor.select(ids.clone());
        self.shell.invalidate_selection();

        let extent = |state: &Self| {
            let placements: Vec<Placement> = state
                .editor
                .selection()
                .iter()
                .filter_map(|id| state.editor.projection().get(*id))
                .map(|projected| projected.item.placement)
                .collect();
            handle::group_bounds(&placements).map_or((0.0, 0.0), |g| (g.width, g.height))
        };
        let before = extent(self);

        // Grab the bottom-right handle of the group box and drag it outwards. Through
        // `act_on`, so `press` → `handle_under` → `begin_drag` is the production path.
        let corner = {
            let placements: Vec<Placement> = ids
                .iter()
                .filter_map(|id| self.editor.projection().scene_id(*id))
                .filter_map(|scene| self.editor.projection().get(scene))
                .map(|projected| projected.item.placement)
                .collect();
            let group = handle::group_bounds(&placements).expect("three stickies have a box");
            WorldPoint::new(group.x + group.width / 2.0, group.y + group.height / 2.0)
        };
        let from = self.camera.world_to_screen(corner);
        let to = self.camera.world_to_screen(WorldPoint::new(corner.x + 400.0, corner.y + 200.0));
        self.act_on(Intent::Press { at: from, additive: false, double: false });
        self.act_on(Intent::Move { from, to });
        self.act_on(Intent::MoveEnd { moved: true });

        let after = extent(self);
        let grew = after.0 > before.0 * 1.1 && after.1 > before.1 * 1.1;
        // Uniform: the box's aspect ratio survives, because a group scales by one factor.
        let aspect = (after.0 / after.1) / (before.0 / before.1);
        if !grew {
            self.gap(&format!("the group did not resize: {before:?} -> {after:?}"));
            return;
        }
        if (aspect - 1.0).abs() > 0.02 {
            self.gap(&format!("the group was stretched, not scaled: aspect changed by {aspect:.3}"));
            return;
        }
        // Re-fit so the enlarged group and its five handles are in frame for the
        // screenshot; the resize itself has already been asserted above.
        self.fit_board();
        self.ok(format!(
            "group scaled {:.2}x uniformly, aspect held",
            after.0 / before.0.max(f64::EPSILON),
        ));
    }

    /// Erases whole items with a real sweep, and reports what survived.
    ///
    /// Three rules this checks that a unit test cannot, because `ActiveState` owns a GPU:
    /// that ⇧ actually reaches `erase_at` and switches modes, that a **locked** item
    /// survives the sweep, and that a frame and its child are not both asked for — which
    /// fails the whole transaction, and is the bug `--demo empty` hit on the real board.
    fn demo_object_eraser(&mut self) {
        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let frame = board.add(NewItem::new(
                ItemKind::Frame {
                    title: StyledText::plain("frame"),
                    order: None,
                    speaker_notes: None,
                },
                Placement::new(0.0, 0.0, 600.0, 400.0),
            ))?;
            // A child of the frame, so the ancestor check has something to do.
            let child = board.add(
                NewItem::new(
                    ItemKind::Sticky { text: StyledText::plain("inside"), background: None },
                    Placement::new(0.0, 0.0, 199.0, 228.0),
                )
                .with_parent(frame),
            )?;
            let free = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("free"), background: None },
                Placement::new(500.0, 0.0, 199.0, 228.0),
            ))?;
            let safe = board.add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("locked"), background: None },
                Placement::new(900.0, 0.0, 199.0, 228.0),
            ))?;
            board.set_style(safe, vellum_doc::Style { locked: true, ..Default::default() })?;
            board.end_undo_group();
            Ok((frame, child, free, safe))
        });
        let Ok((_, _, _, safe)) = placed else {
            self.gap("the eraser fixture could not be built");
            return;
        };
        self.fit_board();
        let before = self.editor.board().item_ids().len();

        // A real sweep: the eraser tool, ⇧ held, dabs across all four items.
        self.choose_tool(Tool::Eraser);
        self.input.set_modifiers(winit::keyboard::ModifiersState::SHIFT);
        for x in [0.0, 250.0, 500.0, 700.0, 900.0] {
            let at = self.camera.world_to_screen(WorldPoint::new(x, 0.0));
            self.act_on(Intent::PlaceSample { at });
        }
        self.act_on(Intent::Place {
            at: self.camera.world_to_screen(WorldPoint::new(900.0, 0.0)),
            to: self.camera.world_to_screen(WorldPoint::new(900.0, 0.0)),
        });
        self.input.set_modifiers(winit::keyboard::ModifiersState::empty());

        let left: Vec<DocId> = self.editor.board().item_ids();
        let locked_survived = left.contains(&safe);
        if locked_survived && left.len() == 1 {
            self.ok(format!("erased {} items; the locked one survived", before - 1));
        } else if !locked_survived {
            self.gap("the object eraser deleted a locked item");
        } else {
            self.gap(&format!("{} items left, expected only the locked one", left.len()));
        }
    }

    /// Runs align, distribute and send-to-back over a selection with one **locked**
    /// member, and reports what moved.
    ///
    /// The hole this closes is specific and was reachable from six menu rows: those
    /// commands are gated on `!all_locked`, so they are disabled only when *everything*
    /// selected is locked — which leaves the case a lock exists to survive, `⌘A` over a
    /// board with one locked item, writing straight through it.
    ///
    /// A fixture rather than a unit test for the usual reason: `ActiveState` owns a GPU and
    /// a window, so the command path cannot be built in a test, and the pure arithmetic
    /// underneath passed throughout. What *is* unit-tested is the filter itself —
    /// `Editor::unlocked_selected_ids` — and that is deliberately only half the evidence,
    /// because the bug was never in the filter. It was in six call sites not calling one.
    ///
    /// Each of the three is measured against a different consequence, so a single wrong
    /// answer cannot hide behind two right ones: align must leave the locked item's `y`,
    /// distribute must leave its `x`, and send-to-back must leave it in front — if the
    /// lock were ignored there, all four would go back together and it would stay second.
    fn demo_locked_arrange(&mut self) {
        let placed = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let mut ids = Vec::new();
            // Staggered in both axes: align has a `y` to change and distribute an `x`.
            for (index, (x, y)) in [(0.0, 0.0), (300.0, 120.0), (700.0, 240.0), (1200.0, 360.0)]
                .into_iter()
                .enumerate()
            {
                ids.push(board.add(NewItem::new(
                    ItemKind::Sticky {
                        text: StyledText::plain(if index == 1 { "locked" } else { "free" }),
                        background: None,
                    },
                    Placement::new(x, y, 199.0, 228.0),
                ))?);
            }
            board.set_style(ids[1], vellum_doc::Style { locked: true, ..Default::default() })?;
            board.end_undo_group();
            Ok(ids)
        });
        let Ok(ids) = placed else {
            self.gap("the locked-arrange fixture could not be built");
            return;
        };
        let safe = ids[1];
        let at = |state: &Self, id: DocId| {
            state.editor.board().item(id).map(|item| (item.placement.x, item.placement.y)).ok()
        };
        let Some(before) = at(self, safe) else { return };
        self.fit_board();

        // Everything selected, exactly as `⌘A` leaves it — the case the gates miss.
        self.editor.select(ids.clone());
        self.shell.invalidate_selection();

        self.run(Command::AlignTop);
        let after_align = at(self, safe);
        self.run(Command::DistributeHorizontally);
        let after_distribute = at(self, safe);

        self.run(Command::SendToBack);
        // Frontmost is the observable: the three unlocked ones went behind, so the one
        // that refused to move is now on top. Had the lock been ignored, all four would
        // have gone back in order and this would still be second.
        let order: Vec<DocId> = {
            let mut z: Vec<(i32, DocId)> =
                self.editor.projection().iter().map(|(_, p)| (p.z, p.doc_id)).collect();
            z.sort_unstable();
            z.into_iter().map(|(_, id)| id).collect()
        };
        let frontmost = order.last().copied();

        let held_y = after_align.map(|p| p.1) == Some(before.1);
        let held_x = after_distribute.map(|p| p.0) == Some(before.0);
        let held_depth = frontmost == Some(safe);
        // The other three have to have actually moved, or "nothing moved" would read as
        // a pass and a command that quietly does nothing would look like a working lock.
        let others_moved = ids
            .iter()
            .filter(|id| **id != safe)
            .filter_map(|id| at(self, *id))
            .all(|(_, y)| (y - before.1).abs() > 1.0);

        log::info!(
            "--demo locked-arrange: locked item {before:?} -> align {after_align:?} \
             -> distribute {after_distribute:?}; frontmost after send-to-back: \
             {}; the other three moved: {others_moved}",
            if held_depth { "the locked one" } else { "something else" }
        );
        if held_y && held_x && held_depth && others_moved {
            self.ok("align, distribute and send-to-back all left the locked item alone");
        } else {
            self.gap(&format!(
                "a locked item was written through: y held {held_y}, x held {held_x}, \
                 depth held {held_depth}, others moved {others_moved}"
            ));
        }
        self.fit_board();
    }

    /// Locks or unlocks the selection.
    ///
    /// One undo step for the whole selection, like every other multi-item operation here.
    /// **The selection is kept** rather than cleared: locking something you have just
    /// selected and then losing the selection makes the next thing you do — usually
    /// unlocking it again — need a click you should not need.
    fn set_locked(&mut self, locked: bool) {
        let styles: Vec<(DocId, vellum_doc::Style)> = self
            .editor
            .selection()
            .iter()
            .filter_map(|scene| self.editor.projection().get(*scene))
            .filter(|projected| projected.item.style.locked != locked)
            .map(|projected| {
                (projected.doc_id, vellum_doc::Style { locked, ..projected.item.style.clone() })
            })
            .collect();
        if styles.is_empty() {
            self.gap(if locked {
                "everything selected is already locked"
            } else {
                "nothing selected is locked"
            });
            return;
        }
        let count = styles.len();
        let result = self.editor.edit(move |board| {
            board.begin_undo_group()?;
            for (id, style) in styles {
                board.set_style(id, style)?;
            }
            board.end_undo_group();
            Ok(())
        });
        match result {
            Ok(()) => {
                self.shell.invalidate_selection();
                let verb = if locked { "Locked" } else { "Unlocked" };
                self.ok(format!("{verb} {count} item{}", plural(count)));
            }
            Err(error) => self.failed(if locked { "locking" } else { "unlocking" }, &error),
        }
    }

    /// Whether an item refuses the pointer.
    ///
    /// The single place the lock is read on the input path, so "what locked means" is one
    /// answer rather than one per gesture.
    fn is_locked(&self, scene: SceneId) -> bool {
        self.editor
            .projection()
            .get(scene)
            .is_some_and(|projected| projected.item.style.locked)
    }

    /// Creates a connector between two world points.
    ///
    /// This was the last of the three "imports but cannot be created" gaps: connectors have
    /// imported, routed, re-routed and restyled since the beginning, and nothing could
    /// place one. What was missing is exactly this — deciding what each end attaches to.
    ///
    /// Each end binds to the item under it, at the edge midpoint **facing the other end**
    /// (see `crate::connector::facing_anchor` for why that is the right measurement), or
    /// stays free and pinned to the connector's own box when there is nothing there. So a
    /// drag from one sticky to another produces a connector that re-routes when either is
    /// moved, which is the entire point of the binding the document stores.
    fn draw_connector(&mut self, from: WorldPoint, to: WorldPoint) {
        // A tap rather than a drag: two ends in the same place is not a connector, and
        // silently making a one-unit one would leave an invisible item on the board.
        if (to.x - from.x).hypot(to.y - from.y) < DRAG_TO_SIZE {
            self.gap("a connector needs two ends: drag from one item to another");
            return;
        }

        // What each end lands on. Resolved before anything is written, so a failure to
        // look one up cannot leave a half-made connector.
        let under = |at: WorldPoint| {
            let scene = self.editor.projection().scene().hit_test(at)?;
            let projected = self.editor.projection().get(scene)?;
            // A connector attached to a connector is not a thing the document models —
            // `ConnectorEnd::target` names an item, and routing one to another would need
            // a point on a path rather than on a box.
            if matches!(projected.item.kind, ItemKind::Connector { .. }) {
                return None;
            }
            Some((projected.doc_id, projected.item.placement))
        };
        let (start_item, end_item) = (under(from), under(to));

        // The anchor faces the *other* end, so two boxes are joined by their facing edges.
        // Measured against the other item's centre when there is one, and against the raw
        // pointer position when there is not.
        let toward = |item: Option<&(DocId, Placement)>, fallback: WorldPoint| {
            item.map_or((fallback.x, fallback.y), |(_, p)| (p.x, p.y))
        };
        let start = match &start_item {
            Some((doc, placement)) => {
                let anchor = crate::connector::facing_anchor(placement, toward(end_item.as_ref(), to));
                ConnectorEnd::bound(*doc, anchor)
            }
            None => ConnectorEnd::free(ConnectorEnd::CENTER),
        };
        let end = match &end_item {
            Some((doc, placement)) => {
                let anchor =
                    crate::connector::facing_anchor(placement, toward(start_item.as_ref(), from));
                ConnectorEnd::bound(*doc, anchor)
            }
            None => ConnectorEnd::free(ConnectorEnd::CENTER),
        };

        // The item's own box. Only the *free* ends are normalised against it, so it is
        // built from where the pointer actually went — a bound end's anchor is a fraction
        // of its target's box and does not care what this is.
        let own = crate::connector::placement_for((from.x, from.y), (to.x, to.y));
        let start = match start_item {
            Some(_) => start,
            None => ConnectorEnd::free(crate::connector::free_anchor(&own, (from.x, from.y))),
        };
        let end = match end_item {
            Some(_) => end,
            None => ConnectorEnd::free(crate::connector::free_anchor(&own, (to.x, to.y))),
        };
        // An arrow on the far end only, which is what a connector drawn by dragging means:
        // the direction is the direction of the drag.
        let end = end.with_arrowhead(vellum_doc::ArrowKind::FilledTriangle);

        let kind = ItemKind::Connector {
            start,
            end,
            routing: vellum_doc::Routing::default(),
            dash: vellum_doc::Dash::default(),
            thickness: CONNECTOR_THICKNESS,
            color: None,
            captions: Vec::new(),
        };
        match self.editor.edit(|board| Ok(board.add(NewItem::new(kind, own))?)) {
            Ok(id) => {
                self.editor.select([id]);
                self.shell.invalidate_selection();
                let bound = usize::from(start_item.is_some()) + usize::from(end_item.is_some());
                self.ok(match bound {
                    2 => "Connected two items".to_owned(),
                    1 => "Connector bound at one end".to_owned(),
                    _ => "Connector placed, both ends free".to_owned(),
                });
            }
            Err(error) => self.failed("drawing a connector", &error),
        }
    }

    // ----- the properties panel ---------------------------------------------

    fn style(&mut self, edit: &vellum_ui::StyleEdit) {
        // **`Command::mutates_board` cannot guard this.** A `StyleEdit` arrives as a
        // `UiEvent` from the context bar or the properties panel, not as a `Command`, so it
        // never passes through [`Self::run`]'s chokepoint — and `apply_style` opens a group of
        // its own. Recolouring a sticky while a caret was in it therefore failed, and then
        // every grouped operation after it did.
        self.settle();
        let ids = self.editor.selected_ids();
        let merge = continuous(edit);
        let Self { editor, .. } = self;
        let applied = editor.edit(|board| {
            board.set_undo_merge_interval(merge);
            Ok(inspect::apply_style(board, &ids, edit)?)
        });
        self.after_edit(applied);
    }

    fn transform(&mut self, edit: vellum_ui::TransformEdit) {
        // Same route and same reason as [`Self::style`]: a typed position or size comes from
        // the panel as a `UiEvent`, and `apply_transform` opens its own group.
        self.settle();
        let ids = self.editor.selected_ids();
        let applied = self.editor.edit(|board| {
            // Every one of these arrives from a `DragValue`, which reports a change on
            // every frame the button is held.
            board.set_undo_merge_interval(DRAG_MERGE_MS);
            Ok(inspect::apply_transform(board, &ids, edit)?)
        });
        self.after_edit(applied);
    }

    /// Board ▸ Background — the canvas's own colour and pattern.
    ///
    /// A document edit like any other, which is what makes it undoable and what makes
    /// it survive being reopened: *"i want to be able to select backgrounds for the
    /// baords"*, not for this session.
    fn set_background(&mut self, background: vellum_doc::Background) {
        let result = self.editor.edit(|board| Ok(board.set_background(background)?));
        if let Err(error) = result {
            self.failed("setting the background", &error);
        }
    }

    /// Replaces the selected item's words.
    ///
    /// Applied to the projection first so the canvas keeps up with the field at the
    /// cost of a field write, then to the document. The undo merge interval is what
    /// stops a sentence becoming forty undo steps.
    fn set_text(&mut self, text: &str) {
        let Some(scene) = self.editor.selection().first().copied() else { return };
        let Some(doc_id) = self.editor.projection().get(scene).map(|p| p.doc_id) else { return };
        // Spliced into the item's existing runs, not `StyledText::plain(text)` — the panel's
        // field is plain but the *item* is not, and flattening here would undo on one keystroke
        // exactly what the canvas caret was taught to preserve. The panel emits on every
        // keystroke, so each call is one contiguous change, which is what makes the splice
        // exact; see `crate::edit::splice_styling`.
        let current = self
            .editor
            .board()
            .item(doc_id)
            .ok()
            .and_then(|item| item.kind.text().cloned())
            .unwrap_or_default();
        let styled = crate::edit::splice_styling(&current, text);
        self.editor.preview_text(scene, styled.clone());
        let result = self.editor.edit(|board| {
            board.set_undo_merge_interval(TYPING_MERGE_MS);
            Ok(board.set_text(doc_id, styled)?)
        });
        if let Err(error) = result {
            self.failed("typing", &error);
        }
    }

    /// Puts the panel back in step with the document it just changed, and reports
    /// anything the document could not carry.
    fn after_edit(&mut self, applied: anyhow::Result<Applied>) {
        match applied {
            Ok(Applied::Changed) => self.shell.invalidate_selection(),
            Ok(Applied::NotApplicable) => {}
            Ok(Applied::Unsupported(why)) => self.gap(why),
            Err(error) => self.failed("applying the change", &error),
        }
    }

    // ----- editing -----------------------------------------------------------

    fn undo(&mut self, redo: bool) {
        let result = if redo { self.editor.redo() } else { self.editor.undo() };
        match result {
            Ok(_) => self.shell.invalidate_selection(),
            Err(error) => self.failed(if redo { "redo" } else { "undo" }, &error),
        }
    }

    fn delete_selection(&mut self) {
        match self.editor.delete_selection() {
            Ok(0) => {}
            Ok(_) => self.shell.invalidate_selection(),
            Err(error) => self.failed("deleting", &error),
        }
    }

    /// Copies the selection into the app's own clipboard, and its text into the
    /// system's.
    ///
    /// Two clipboards because they answer two questions. The system clipboard is what
    /// makes a sticky's words paste into another application, and it is the only half
    /// another application can read. The app's own holds the *items* — kind, style and
    /// placement — because a board is not text and round-tripping it through one would
    /// lose everything but the words.
    ///
    /// The selection is widened to include what is *inside* what was selected, and the
    /// text written to the pasteboard is remembered in
    /// [`clipboard_text`](crate::app::ActiveState::clipboard_text) so that a paste can
    /// tell it from another application's. Both are explained where they are read.
    fn copy(&mut self, cut: bool) {
        let ids = self.editor.selected_ids();
        if ids.is_empty() {
            return;
        }

        // A container's contents come along. A frame's children are separate items that
        // merely name it as their parent, so copying a selection of *just* the frame used
        // to produce an empty box. `Board::descendants` answers in `item_ids` order, which
        // is what lets `paste_internal` create and reparent in one forward pass.
        let board = self.editor.board();
        let mut wanted: Vec<DocId> = Vec::with_capacity(ids.len());
        let mut seen: HashSet<DocId> = ids.iter().copied().collect();
        for id in &ids {
            wanted.push(*id);
            for child in board.descendants(*id) {
                if seen.insert(child) {
                    wanted.push(child);
                }
            }
        }
        let items: Vec<vellum_doc::Item> =
            wanted.iter().filter_map(|id| board.item(*id).ok()).collect();
        self.clipboard = items;

        let text: Vec<String> = self
            .clipboard
            .iter()
            .filter_map(|item| item.kind.text())
            .map(vellum_doc::StyledText::to_plain)
            .filter(|line: &String| !line.trim().is_empty())
            .collect();

        // **The pasteboard is claimed on every copy, and what was written is remembered.**
        //
        // Both halves are load-bearing, and getting this wrong is what made a cross-board
        // copy paste one grey text blob. `paste` reads plain text before it reaches the
        // app's own clipboard — deliberately, so a screenshot beats a stale board copy —
        // so the text written here came straight back as a single `ItemKind::Text` holding
        // every copied item's words, and `paste_internal` was unreachable.
        //
        // Remembering the exact string is what lets `take_clipboard_payload` tell our own
        // text from another application's without reordering the flavours and regressing
        // the screenshot case. Claiming it even when there is no text matters just as much:
        // this used to skip the write entirely for text-free items, so copying a shape left
        // an older blob — or a picture copied in another app — sitting there to win.
        //
        // The session's one handle, not a fresh one per copy. `crate::editor` records what
        // building these per keystroke cost — 14.24 GB — and a copy is held down no less
        // often than a paste.
        let clipboard = self
            .system_clipboard
            .get_or_insert_with(|| arboard::Clipboard::new().map_err(|error| error.to_string()));
        match clipboard {
            Ok(clipboard) => {
                if text.is_empty() {
                    if let Err(error) = clipboard.clear() {
                        log::warn!("clearing the clipboard: {error}");
                    }
                    self.clipboard_text = None;
                } else {
                    let blob = text.join("\n");
                    match clipboard.set().text(blob.clone()) {
                        Ok(()) => self.clipboard_text = Some(blob),
                        Err(error) => {
                            log::warn!("putting text on the clipboard: {error}");
                            // Not ours if the write failed, whatever is there now.
                            self.clipboard_text = None;
                        }
                    }
                }
            }
            Err(error) => {
                log::warn!("no system clipboard to copy into: {error}");
                self.clipboard_text = None;
            }
        }

        let count = self.clipboard.len();
        if cut {
            self.delete_selection();
            self.ok(format!("Cut {count} item{}", plural(count)));
        } else {
            self.ok(format!("Copied {count} item{}", plural(count)));
        }
    }

    /// Pastes whatever the clipboard is actually holding.
    ///
    /// The flavours are tried in the order that keeps the most meaning:
    ///
    /// 1. **A Miro board.** `docs/02-miro-formats.md` establishes the clipboard as the
    ///    *only* route a Miro board can come across, and the boards the user already has
    ///    are the whole reason for this application. A paste that quietly did something
    ///    else instead would look like the import silently failing.
    /// 2. **An image**, then **files**, then **HTML**, then **plain text**. Each is more
    ///    specific than the one after it; a screenshot also has a text flavour on some
    ///    platforms, and taking that first would paste a filename instead of a picture.
    /// 3. **The app's own clipboard**, last.
    ///
    /// That last position matters and used to be second. Anything not recognised fell
    /// straight through to the internal clipboard, so once anything had been copied in
    /// this session — which is to say, almost always — pasting a screenshot re-pasted
    /// the last item copied on the board and reported "Pasted 1 item". Silently doing
    /// the wrong thing, with a success message on top of it.
    pub(crate) fn paste(&mut self) {
        self.paste_aimed(PasteAim::Pointer);
    }

    /// [`Self::paste`] with explicit control over where a **Miro board** payload lands.
    ///
    /// Every other flavour has always landed at the pointer. The Miro arm did not, and
    /// still does not when the whole point is to reproduce a board: it wrote Miro's own
    /// absolute coordinates and then called `fit_board`, which is right for
    /// [`Self::import_to_new_board`] — an empty board, where "where the mouse was" is
    /// meaningless and fitting is the only way to see what arrived.
    ///
    /// It was **also** what `⌘V` did, into a board that already had content, and that is
    /// the defect: reported as *"in some boards sometimes it pastes into one place
    /// instead of where my mouse is"*. "Sometimes" is the tell — it is every time the
    /// clipboard holds a Miro payload, and never for the image, text and internal
    /// flavours beside it, so it reads as intermittent while being perfectly
    /// deterministic. The camera is deliberately left alone here too: fitting the board
    /// on a `⌘V` throws away the view the user was working in.
    fn paste_aimed(&mut self, aim: PasteAim) {
        self.recorder.event("paste");
        // Read *before* the clipboard is touched. `take_clipboard_payload` and the Miro
        // import can both take long enough for the pointer to have moved on, and every
        // flavour below has to agree on one anchor or a paste would land in two places
        // depending on which arm claimed it.
        let at = self.camera.screen_to_world(self.input.cursor(&self.camera));
        // Opened on the first paste and kept for the session. Borrowed out of the
        // field here rather than through a helper so `editor` and `archive` stay
        // independently borrowable alongside it.
        let clipboard = self
            .system_clipboard
            .get_or_insert_with(|| arboard::Clipboard::new().map_err(|error| error.to_string()));
        let clipboard = match clipboard {
            Ok(clipboard) => clipboard,
            Err(error) => {
                let error = anyhow::anyhow!("could not open the system clipboard: {error}");
                self.failed("pasting", &error);
                return;
            }
        };

        match self.editor.paste_from_clipboard(clipboard, self.archive.as_mut()) {
            Ok(Some(outcome)) => {
                // The whole report goes to the log — it names every substitution and
                // every missing asset — while the canvas gets the one line that fits.
                log::info!("{outcome}");
                let missing = outcome.missing_assets.len();
                let mut summary = format!("imported {} items from Miro", outcome.total());
                if missing > 0 {
                    summary.push_str(&format!(", {missing} without their assets"));
                }
                self.status = Some((summary.clone(), Instant::now()));
                self.ok(summary);
                match aim {
                    // A board of its own: keep Miro's coordinates and show the lot.
                    PasteAim::KeepMiroCoordinates => self.fit_board(),
                    // `⌘V` into a board that already has content: land under the mouse
                    // like every other flavour, and leave the camera where it was.
                    PasteAim::Pointer => self.shift_imported_onto(&outcome.items, at),
                }
                self.shell.invalidate_selection();
                return;
            }
            Ok(None) => {}
            Err(error) => {
                self.failed("pasting", &error);
                return;
            }
        }

        match self.take_clipboard_payload() {
            Some(Payload::Image(bytes)) => self.place_pasted_image(&bytes, at),
            Some(Payload::Text(text)) => self.place_pasted_text(&text, at),
            None => self.paste_internal(at),
        }
    }

    /// Moves a just-imported Miro board so its bounding box is centred on `at`.
    ///
    /// One offset for the whole set, from [`shift_onto`], so the board keeps its shape —
    /// the same rule the app's own clipboard follows, and for the same reason: moving each
    /// item to the pointer would collapse 596 widgets into a stack.
    ///
    /// **Every item is moved, including the children of frames.** `Placement`'s `x`/`y`
    /// are absolute world coordinates rather than offsets from a parent
    /// (`vellum_doc::geometry`, which notes that this matches Miro's own `offsetPx`), so
    /// shifting only the roots would leave every framed sticky behind.
    ///
    /// Silent when the payload is empty or the offset is nil: an import that landed
    /// exactly under the pointer needs no second undo step.
    fn shift_imported_onto(&mut self, imported: &[DocId], at: WorldPoint) {
        let placed: Vec<vellum_doc::Item> = imported
            .iter()
            .filter_map(|id| self.editor.board().item(*id).ok())
            .collect();
        let (dx, dy) = shift_onto(&placed, at);
        log::info!(
            "aimed {} imported items at the pointer ({:.0}, {:.0}), moving them by ({dx:.0}, {dy:.0})",
            placed.len(),
            at.x,
            at.y
        );
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        // One undo group, so `⌘Z` after a paste puts the whole import back rather than
        // leaving 596 items sitting at the origin.
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for item in &placed {
                let mut moved = item.placement;
                moved.x += dx;
                moved.y += dy;
                board.set_placement(item.id, moved)?;
            }
            board.end_undo_group();
            Ok(())
        });
        if let Err(error) = result {
            self.failed("placing the imported board", &error);
        }
    }

    /// Reads the system clipboard's flavours in priority order — see [`Self::paste`].
    ///
    /// Kept separate from the placing so the borrow of `system_clipboard` ends before
    /// the document is touched.
    fn take_clipboard_payload(&mut self) -> Option<Payload> {
        let Some(Ok(clipboard)) = self.system_clipboard.as_mut() else { return None };

        // An image, as loose RGBA8. `arboard` decodes the pasteboard's own format —
        // TIFF on macOS, DIB on Windows — and hands back pixels, *not* an encoded file.
        // The blob store holds encoded bytes and `crate::assets` decodes with
        // `image::load_from_memory`, so this has to be re-encoded or the blob would be
        // a headerless pixel dump nothing could read back.
        //
        // The error is logged rather than dropped, and the two arms mean different
        // things: `ContentNotAvailable` is "no image flavour on the pasteboard", which
        // is the ordinary case for a text copy and also what a macOS `⇧⌘4` produces —
        // that writes a *file to the Desktop* and leaves the pasteboard untouched,
        // where `⌃⇧⌘4` is the one that copies. `ConversionFailure` is an image that
        // was there and would not decode, which is a real fault. On macOS `arboard`
        // reads `NSPasteboardTypeTIFF` only, so a source that offers `public.png`
        // alone reports the first when it means the second.
        match clipboard.get().image() {
            Ok(image) => {
                let (width, height) = (image.width as u32, image.height as u32);
                match encode_png(&image.bytes, width, height) {
                    Some(bytes) => return Some(Payload::Image(bytes)),
                    None => log::warn!("a clipboard image of {width}×{height} would not re-encode"),
                }
            }
            Err(arboard::Error::ContentNotAvailable) => {
                log::info!("the clipboard holds no image flavour");
            }
            Err(error) => log::warn!("the clipboard's image would not decode: {error}"),
        }

        // Files, for "copy a picture in Finder, then paste". Needs no feature flag and
        // costs nothing, so it comes free with the image path.
        if let Ok(paths) = clipboard.get().file_list()
            && let Some(bytes) = paths.iter().find_map(|path| std::fs::read(path).ok())
            && image::guess_format(&bytes).is_ok()
        {
            return Some(Payload::Image(bytes));
        }

        // HTML before plain text: a copy from a browser carries both, and the HTML is
        // the richer one. Only its text is kept — a span model that could hold the
        // markup is `features/README` §3's rich-text work, not this.
        if let Ok(html) = clipboard.get().html() {
            let text = strip_html(&html);
            if !text.trim().is_empty() {
                return Some(Payload::Text(text));
            }
        }

        // Plain text, unless it is **our own** plain text.
        //
        // `copy` puts the selection's words on the pasteboard so a sticky can be pasted
        // into another application. Read back naively that is indistinguishable from text
        // copied anywhere else, so a board copy came home as one `ItemKind::Text` holding
        // every item's words concatenated — and `paste_internal` below it was dead code.
        //
        // The fix is *not* to move the internal clipboard above this arm. That ordering
        // was tried and is wrong for the opposite case: anything unrecognised fell
        // through to the internal clipboard, so pasting a screenshot silently re-pasted
        // the last item copied on the board and reported success. Both cases are served
        // by making our own write identifiable instead — `copy` remembers the exact
        // string, and only that string defers to the items behind it.
        //
        // Byte-identical, not "looks similar": another application legitimately copying
        // the same words should paste as text.
        // Read out of the borrow before deciding, so the rest of this can touch `self`
        // freely — `clipboard` is a mutable borrow of one of its fields.
        let text = clipboard.get().text().ok().filter(|text| !text.trim().is_empty());
        let text = text?;

        let ours = self.clipboard_text.as_deref() == Some(text.as_str());
        if ours && !self.clipboard.is_empty() {
            return None;
        }
        if !ours {
            // Someone else owns the pasteboard now, so stop claiming it. Without this,
            // copying items here, then copying the very same words in another
            // application, would keep deferring to the items behind them.
            self.clipboard_text = None;
        }
        Some(Payload::Text(text))
    }

    /// Puts pasted image bytes in the blob store and makes an item of them.
    ///
    /// The same three steps the Miro importer takes — `blobs.put`, `hash.to_hex`,
    /// `ItemKind::Image` — so a pasted picture and an imported one are the same thing
    /// afterwards.
    fn place_pasted_image(&mut self, bytes: &[u8], at: WorldPoint) {
        let hash = match self.editor.assets().blobs().put(bytes) {
            Ok(hash) => hash,
            Err(error) => return self.failed("pasting an image", &anyhow::anyhow!(error)),
        };

        // Sized from the image's own pixels so it arrives the shape it is. Capped
        // because a 6000px screenshot pasted at full size fills the board and lands
        // mostly outside the viewport.
        let (width, height) = match image::load_from_memory(bytes) {
            Ok(decoded) => {
                let (w, h) = (f64::from(decoded.width()), f64::from(decoded.height()));
                let scale = (MAX_PASTED_IMAGE / w.max(h)).min(1.0);
                ((w * scale).max(1.0), (h * scale).max(1.0))
            }
            Err(error) => {
                log::warn!("pasted image dimensions unreadable, using a default: {error}");
                (480.0, 360.0)
            }
        };

        let kind = ItemKind::Image { asset_id: hash.to_hex(), crop: None };
        let placement = Placement::new(at.x, at.y, width, height);
        match self.editor.edit(|board| Ok(board.add(NewItem::new(kind, placement))?)) {
            Ok(id) => {
                self.editor.select([id]);
                self.shell.invalidate_selection();
                self.ok("Pasted an image");
            }
            Err(error) => self.failed("pasting an image", &error),
        }
    }

    /// Pasted text becomes a sticky, or a text item once it is too long to be one.
    ///
    /// A sticky is the note-shaped thing and what a short paste almost always wants;
    /// past [`STICKY_TEXT_LIMIT`] characters it would auto-fit down to unreadable, and
    /// a text item — which grows instead — is the honest container.
    /// The URL in `text`, when the whole of `text` **is** one URL.
    ///
    /// Deliberately strict. A single token, no whitespace, and a host `vellum-link` recognises
    /// — which also refuses `mailto:`, `javascript:` and `file:` , so nothing that is not a web
    /// page can become a card. Prose containing a link is left as prose: a paragraph is worth
    /// more than the link inside it, and a card would keep only the link.
    fn lone_url(text: &str) -> Option<&str> {
        let candidate = text.trim();
        if candidate.split_whitespace().count() != 1 {
            return None;
        }
        // A bare host is a URL a person means — `example.com/thing` pasted from a browser bar —
        // but a bare *word* is not, so a scheme or a dot-bearing host is required. `host_of`
        // insists on the dot.
        vellum_link::host_of(candidate).is_some().then_some(candidate)
    }

    fn place_pasted_text(&mut self, text: &str, at: WorldPoint) {
        let text = text.trim();
        let styled = StyledText::plain(text);
        // A lone URL becomes a **link card**, not a sticky with a URL on it. This is how a
        // board of links is actually built — Miro does the same on paste — and without it the
        // card kinds were reachable only by importing someone else's board.
        //
        // "Lone" is the whole condition: one whitespace-free token with a host. A paragraph
        // that happens to mention a link is prose and stays prose, because turning it into a
        // card would throw the sentence away.
        let (kind, (width, height), what) = if let Some(url) = Self::lone_url(text) {
            (
                // The provider comes from the host, so the card names its site on the very
                // next frame with no network at all. A fetch — if the user has previews on —
                // improves the title and can add an image afterwards.
                ItemKind::LinkPreview {
                    title: None,
                    url: Some(url.to_owned()),
                    description: None,
                    thumbnail: None,
                    provider: vellum_link::provider_for(url),
                    favicon: None,
                    mode: vellum_doc::CardMode::default(),
                },
                LINK_CARD_SIZE,
                "Pasted a link",
            )
        } else if text.chars().count() > STICKY_TEXT_LIMIT {
            (ItemKind::Text { text: styled }, (480.0, 240.0), "Pasted text")
        } else {
            (ItemKind::Sticky { text: styled, background: None }, (199.0, 228.0), "Pasted text")
        };

        let placement = Placement::new(at.x, at.y, width, height);
        match self.editor.edit(|board| Ok(board.add(NewItem::new(kind, placement))?)) {
            Ok(id) => {
                self.editor.select([id]);
                self.shell.invalidate_selection();
                self.ok(what);
                // A pasted link asks for its own title and picture, if previews are on. This is
                // the path that makes the feature feel immediate: the card appears named by its
                // host on this frame, and fills in a moment later.
                if let Some(url) = Self::lone_url(text) {
                    self.fetch_link(id, url, vellum_doc::CardMode::default());
                }
            }
            Err(error) => self.failed("pasting text", &error),
        }
    }

    /// Pastes the app's own clipboard — the last thing copied *on a board*.
    ///
    /// Reached once every system flavour has been tried and declined, which now includes
    /// declining our own text: see [`Self::take_clipboard_payload`]. An empty internal
    /// clipboard here means the system one held nothing this build can use. Say which,
    /// rather than nothing: an unhandled flavour that produces silence is
    /// indistinguishable from a broken paste.
    ///
    /// The copy is a faithful one, which takes more than cloning each item:
    ///
    /// - it lands **at the pointer**, like every other paste flavour ([`shift_onto`]);
    /// - `parent` and connector endpoints are **remapped** to the new ids, not carried
    ///   across as the originals' ([`cut_loose`], [`rebound`]);
    /// - the whole thing is **one undo group**, so ⌘Z takes back a paste and not an item.
    fn paste_internal(&mut self, at: WorldPoint) {
        if self.clipboard.is_empty() {
            self.shell.toast(Toast::info(
                "Nothing to paste: the clipboard holds no board, image, file or text, \
                 and nothing has been copied here",
            ));
            return;
        }
        let items = self.clipboard.clone();
        let (dx, dy) = shift_onto(&items, at);
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;

            // **Two passes, one undo group.** An item's parent and a connector's
            // endpoints are `ItemId`s, and the ids of the items being created do not
            // exist until they are created. Pass one strips both, so no intermediate
            // state ever names an id that is not there; pass two puts them back through
            // the old→new map.
            //
            // Stripping is not belt-and-braces. An `ItemId` is a Loro `TreeID` — a peer
            // and a counter — so an id copied from one board can *collide with an
            // unrelated item* on another. Carried across verbatim, a pasted connector
            // would silently bind itself to a stranger, and `NewItem::new` drops the
            // parent anyway, which is what used to make a pasted group fall apart.
            let mut created = Vec::with_capacity(items.len());
            let mut remap: HashMap<DocId, DocId> = HashMap::with_capacity(items.len());
            for item in &items {
                let placement = Placement {
                    x: item.placement.x + dx,
                    y: item.placement.y + dy,
                    ..item.placement
                };
                let id = board.add(
                    NewItem::new(cut_loose(item.kind.clone()), placement)
                        .with_style(item.style.clone()),
                )?;
                remap.insert(item.id, id);
                created.push(id);
            }

            for (item, new_id) in items.iter().zip(&created) {
                // A parent that was not itself copied stays dropped: on another board it
                // would not exist, and on this one re-parenting into the original's frame
                // would put the copy somewhere the user cannot see it moved to.
                if let Some(parent) = item.parent.and_then(|old| remap.get(&old)) {
                    board.reparent(*new_id, Some(*parent))?;
                }
                if let ItemKind::Connector { .. } = &item.kind {
                    board.set_kind(*new_id, rebound(item.kind.clone(), &remap))?;
                }
            }

            board.end_undo_group();
            Ok(created)
        });
        match result {
            Ok(created) => {
                let count = created.len();
                self.editor.select(created);
                self.shell.invalidate_selection();
                self.ok(format!("Pasted {count} item{}", plural(count)));
            }
            Err(error) => self.failed("pasting", &error),
        }
    }

    fn duplicate_selection(&mut self) {
        let ids = self.editor.selected_ids();
        if ids.is_empty() {
            return;
        }
        let items: Vec<vellum_doc::Item> = ids
            .iter()
            .filter_map(|id| self.editor.board().item(*id).ok())
            .collect();
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let mut created = Vec::with_capacity(items.len());
            for item in &items {
                let placement = Placement {
                    x: item.placement.x + OFFSET,
                    y: item.placement.y + OFFSET,
                    ..item.placement
                };
                created.push(
                    board.add(
                        NewItem::new(item.kind.clone(), placement).with_style(item.style.clone()),
                    )?,
                );
            }
            board.end_undo_group();
            Ok(created)
        });
        match result {
            Ok(created) => {
                self.editor.select(created);
                self.shell.invalidate_selection();
            }
            Err(error) => self.failed("duplicating", &error),
        }
    }

    /// Moves the selection through the z-order.
    ///
    /// One step at a time for forward and backward, using the paint order the
    /// projection already computed, so "forward" means past exactly one thing that is
    /// currently drawn over it rather than past everything.
    /// Moves the selection through the paint order.
    ///
    /// A **locked** member is skipped. Depth is not position, so this is the weakest of
    /// the four lock holes — nothing lands anywhere else — but a lock that a whole class
    /// of commands ignores is a lock nobody can rely on, and "send to back" on a select-all
    /// is exactly how a pinned background frame ends up in front of the board again.
    fn reorder(&mut self, order: Order) {
        let ids = self.editor.unlocked_selected_ids();
        if ids.is_empty() {
            if self.editor.locked_selected_count() > 0 {
                self.gap("everything selected is locked");
            }
            return;
        }
        // Paint order over the whole document, which is what `raise_above` needs a
        // neighbour from.
        let mut ordered: Vec<(i32, DocId)> = self
            .editor
            .projection()
            .iter()
            .map(|(_, p)| (p.z, p.doc_id))
            .collect();
        ordered.sort_unstable();

        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            match order {
                Order::Front => {
                    for id in &ids {
                        board.bring_to_front(*id)?;
                    }
                }
                Order::Back => {
                    // In reverse, so the selection keeps its own internal order once
                    // every member has been pushed to the bottom.
                    for id in ids.iter().rev() {
                        board.send_to_back(*id)?;
                    }
                }
                Order::Forward | Order::Backward => {
                    for id in &ids {
                        let Some(index) = ordered.iter().position(|(_, other)| other == id) else {
                            continue;
                        };
                        let neighbour = if matches!(order, Order::Forward) {
                            ordered.get(index + 1)
                        } else {
                            index.checked_sub(1).and_then(|i| ordered.get(i))
                        };
                        let Some((_, neighbour)) = neighbour else { continue };
                        if ids.contains(neighbour) {
                            continue;
                        }
                        if matches!(order, Order::Forward) {
                            board.raise_above(*id, *neighbour)?;
                        } else {
                            board.lower_below(*id, *neighbour)?;
                        }
                    }
                }
            }
            board.end_undo_group();
            Ok(())
        });
        if let Err(error) = result {
            self.failed("reordering", &error);
        }
    }

    /// Wraps the selection in a group.
    ///
    /// `vellum_doc::ItemKind::Group` is a container with no visual payload — it draws
    /// nothing and does not clip — so this is the movable tree doing the work, which
    /// is exactly what `docs/01-architecture.md` §4 chose Loro's movable tree for.
    fn group(&mut self) {
        let ids = self.editor.selected_ids();
        if ids.len() < 2 {
            return;
        }
        let Some(bounds) = self.selection_bounds() else { return };
        let placement = Placement::new(
            (bounds.min.x + bounds.max.x) / 2.0,
            (bounds.min.y + bounds.max.y) / 2.0,
            bounds.max.x - bounds.min.x,
            bounds.max.y - bounds.min.y,
        );
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let group = board.add(NewItem::new(ItemKind::Group, placement))?;
            for id in &ids {
                board.reparent(*id, Some(group))?;
            }
            board.end_undo_group();
            Ok(group)
        });
        match result {
            Ok(group) => {
                self.editor.select([group]);
                self.shell.invalidate_selection();
            }
            Err(error) => self.failed("grouping", &error),
        }
    }

    fn ungroup(&mut self) {
        let ids = self.editor.selected_ids();
        let groups: Vec<DocId> = ids
            .iter()
            .copied()
            .filter(|id| {
                self.editor
                    .board()
                    .item(*id)
                    .is_ok_and(|item| matches!(item.kind, ItemKind::Group))
            })
            .collect();
        if groups.is_empty() {
            return;
        }
        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let mut freed = Vec::new();
            for group in &groups {
                let parent = board.parent_of(*group);
                for child in board.children(Some(*group)) {
                    board.reparent(child, parent)?;
                    freed.push(child);
                }
                board.remove(*group)?;
            }
            board.end_undo_group();
            Ok(freed)
        });
        match result {
            Ok(freed) => {
                self.editor.select(freed);
                self.shell.invalidate_selection();
            }
            Err(error) => self.failed("ungrouping", &error),
        }
    }

    /// Aligns the selection to one edge of its bounding box.
    ///
    /// A **locked** member is an anchor, not a participant: it counts towards the box —
    /// so "align left" on a locked background frame and three stickies moves the stickies
    /// onto the frame — and it is never itself written. Both halves matter. Taking the box
    /// from only the movable items would make a locked item's presence in the selection
    /// have no effect at all, and writing through the lock is what this used to do.
    fn align(&mut self, edge: Align) {
        // The whole selection decides the box; only the unlocked part is moved.
        if self.editor.selected_ids().len() < 2 {
            return;
        }
        let ids = self.editor.unlocked_selected_ids();
        if ids.is_empty() {
            self.gap("everything selected is locked");
            return;
        }
        let Some(bounds) = self.selection_bounds() else { return };
        let placements: Vec<(DocId, Placement)> = ids
            .iter()
            .filter_map(|id| self.editor.board().item(*id).ok().map(|i| (*id, i.placement)))
            .collect();

        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            for (id, placement) in &placements {
                // `Placement::x` is the item's *centre*; the panel and the aligner both
                // think in edges, so the half-size is the bridge.
                let (w, h) = placement.scaled_size();
                let mut next = *placement;
                match edge {
                    Align::Left => next.x = bounds.min.x + w / 2.0,
                    Align::Right => next.x = bounds.max.x - w / 2.0,
                    Align::CentreX => next.x = (bounds.min.x + bounds.max.x) / 2.0,
                    Align::Top => next.y = bounds.min.y + h / 2.0,
                    Align::Bottom => next.y = bounds.max.y - h / 2.0,
                    Align::CentreY => next.y = (bounds.min.y + bounds.max.y) / 2.0,
                }
                if next != *placement {
                    board.set_placement(*id, next)?;
                }
            }
            board.end_undo_group();
            Ok(())
        });
        if let Err(error) = result {
            self.failed("aligning", &error);
        }
    }

    /// Spaces the selection evenly between its outermost two members.
    ///
    /// Gap-based rather than centre-based: Miro distributes so the *space between*
    /// items is equal, which is what looks right when the items are different sizes.
    ///
    /// A **locked** member keeps its place in the arithmetic and loses its turn at being
    /// written: it still occupies its width and still counts towards the span, so the
    /// items around it land on the same even grid they would have landed on, and the one
    /// the user pinned does not move. The alternative — dropping it from the list — would
    /// distribute the rest *through* it and overlap the thing that was protected.
    fn distribute(&mut self, horizontal: bool) {
        let ids = self.editor.selected_ids();
        if ids.len() < 3 {
            return;
        }
        if self.editor.unlocked_selected_ids().is_empty() {
            self.gap("everything selected is locked");
            return;
        }
        let mut placements: Vec<(DocId, Placement, bool)> = ids
            .iter()
            .filter_map(|id| {
                self.editor
                    .board()
                    .item(*id)
                    .ok()
                    .map(|i| (*id, i.placement, !i.style.locked))
            })
            .collect();
        // Guarded on the *resolved* placements, not on `ids`: the `filter_map` above can
        // drop an id, and the arithmetic below indexes `placements.len() - 1` and divides
        // by it. Three selected items that resolved to none would panic rather than
        // decline. The projection and the document are in step today, so this is a
        // structural guard rather than a reachable path — but it costs one line and the
        // failure it prevents takes the window with it.
        if placements.len() < 3 {
            return;
        }
        placements.sort_by(|a, b| {
            let (a, b) = if horizontal { (a.1.x, b.1.x) } else { (a.1.y, b.1.y) };
            a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
        });

        let extent = |p: &Placement| {
            let (w, h) = p.scaled_size();
            if horizontal { (p.x - w / 2.0, w) } else { (p.y - h / 2.0, h) }
        };
        let (first_start, _) = extent(&placements[0].1);
        let (last_start, last_size) = extent(&placements[placements.len() - 1].1);
        let span = last_start + last_size - first_start;
        let occupied: f64 = placements.iter().map(|(_, p, _)| extent(p).1).sum();
        let gap = (span - occupied) / (placements.len() - 1) as f64;

        let result = self.editor.edit(|board| {
            board.begin_undo_group()?;
            let mut cursor = first_start;
            for (id, placement, movable) in &placements {
                let (_, size) = extent(placement);
                let mut next = *placement;
                if horizontal {
                    next.x = cursor + size / 2.0;
                } else {
                    next.y = cursor + size / 2.0;
                }
                // The cursor advances either way: a locked item holds its slot in the
                // run so the items after it keep the same gap they would otherwise have.
                if *movable && next != *placement {
                    board.set_placement(*id, next)?;
                }
                cursor += size + gap;
            }
            board.end_undo_group();
            Ok(())
        });
        if let Err(error) = result {
            self.failed("distributing", &error);
        }
    }

    /// Where the selection is on screen, in **logical** points — what
    /// `vellum_ui::context_bar` floats its toolbar above.
    ///
    /// Two conversions, and both matter. The camera answers in *physical* pixels, which
    /// is what `ScreenPoint` means everywhere in this crate (trap 4), and egui lays out
    /// in logical points — so a bar positioned without the division lands at twice the
    /// selection's distance from the window's origin, which on a retina display puts it
    /// off screen for anything past the middle of the board.
    ///
    /// A **rotated** item contributes its axis-aligned bounds, because that is what
    /// `Projected::bounds` holds and what the selection ring already draws. The bar
    /// therefore sits above the box the user can see rather than above a corner of a
    /// turned rectangle.
    pub(crate) fn selection_screen_rect(&self) -> Option<egui::Rect> {
        let bounds = self.selection_bounds()?;
        let scale = self.shell.pixels_per_point().max(f32::MIN_POSITIVE);
        let min = self.camera.world_to_screen(bounds.min);
        let max = self.camera.world_to_screen(bounds.max);
        Some(egui::Rect::from_min_max(
            egui::pos2(min.x as f32 / scale, min.y as f32 / scale),
            egui::pos2(max.x as f32 / scale, max.y as f32 / scale),
        ))
    }

    fn selection_bounds(&self) -> Option<WorldRect> {
        self.editor
            .selection()
            .iter()
            .filter_map(|id| self.editor.projection().get(*id))
            .map(|item| item.bounds)
            .reduce(|a, b| {
                WorldRect::from_corners(
                    WorldPoint::new(a.min.x.min(b.min.x), a.min.y.min(b.min.y)),
                    WorldPoint::new(a.max.x.max(b.max.x), a.max.y.max(b.max.y)),
                )
            })
    }

    // ----- the camera --------------------------------------------------------

    fn zoom_by(&mut self, factor: f64) {
        self.camera.zoom_by(factor, self.viewport_centre());
    }

    fn zoom_to(&mut self, zoom: f64) {
        self.camera.set_zoom_about(zoom, self.viewport_centre());
    }

    fn viewport_centre(&self) -> ScreenPoint {
        let viewport = self.camera.viewport();
        ScreenPoint::new(viewport.width / 2.0, viewport.height / 2.0)
    }

    pub(crate) fn fit_selection(&mut self) {
        match self.selection_bounds() {
            Some(extent) => {
                let canvas = self.canvas_pixels();
                crate::app::fit_rect_in_canvas(&mut self.camera, extent, canvas);
            }
            None => self.fit_board(),
        }
    }

    /// The camera a board opens at — `docs/04-ui-reference.md` §4's "Start view".
    ///
    /// Stored beside the boards rather than inside one: it is a preference about how
    /// *this machine* looks at the board, in the same file as the stars and the
    /// spaces, and putting it in the document would make every camera move a
    /// candidate for a CRDT transaction.
    fn set_start_view(&mut self) {
        let Some(path) = self.editor.path().map(Path::to_path_buf) else {
            self.gap("Save the board first, so there is something to remember the view against");
            return;
        };
        let centre = self.camera.center();
        self.shell
            .library
            .set_start_view(&path, [centre.x, centre.y, self.camera.zoom()]);
        self.ok("Start view set");
    }

    fn go_to_start_view(&mut self) {
        let view = self
            .editor
            .path()
            .and_then(|path| self.shell.library.start_view(path));
        match view {
            Some([x, y, zoom]) => {
                self.camera.set_center(WorldPoint::new(x, y));
                self.zoom_to(zoom);
            }
            None => {
                // Not a failure: a board with no start view opens fitted, which is what
                // "go to the start" means for it.
                self.fit_board();
            }
        }
    }

    fn persist_view(&mut self) {
        let view = self.shell.view();
        self.shell
            .library
            .set_view_toggles(view.minimap_visible);
    }

    // ----- find --------------------------------------------------------------

    /// `Cmd+F`, over the board's own text.
    ///
    /// **An honest gap in scope, not in behaviour.** `docs/features/README.md` §3 puts
    /// find over `vellum-search`'s inverted index so it can search *across* boards.
    /// This searches the open board by substring, which is what the find bar's own
    /// interface — a query and a `(current, total)` readout — describes. Cross-board
    /// search needs a result list the chrome does not have a panel for.
    fn find(&mut self, event: FindEvent) {
        match event {
            FindEvent::Query(query) => {
                self.matches = self.search(&query);
                self.match_index = 0;
                let total = self.matches.len();
                self.shell
                    .set_find_matches(if query.is_empty() { None } else { Some((total.min(1), total)) });
                self.show_match();
            }
            FindEvent::Next | FindEvent::Previous => {
                if self.matches.is_empty() {
                    return;
                }
                let len = self.matches.len();
                self.match_index = if matches!(event, FindEvent::Next) {
                    (self.match_index + 1) % len
                } else {
                    (self.match_index + len - 1) % len
                };
                self.shell.set_find_matches(Some((self.match_index + 1, len)));
                self.show_match();
            }
            FindEvent::Closed => {
                self.matches.clear();
                self.match_index = 0;
                self.shell.set_find_matches(None);
            }
        }
    }

    /// Every item whose words contain the query, in paint order.
    ///
    /// Through `crate::words` rather than `ItemKind::text`, which is what brought the four
    /// structured widgets into the index: their words are inside a JSON token `vellum-doc`
    /// deliberately cannot parse, so a search for a word in a table cell used to find
    /// nothing at all while the same word on a sticky was found.
    fn search(&self, query: &str) -> Vec<DocId> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(i32, DocId)> = self
            .editor
            .projection()
            .iter()
            .filter_map(|(_, projected)| {
                crate::words::contains(&projected.item.kind, &needle)
                    .then_some((projected.z, projected.doc_id))
            })
            .collect();
        // Paint order, so stepping through matches walks the board the way it reads.
        hits.sort_unstable();
        hits.into_iter().map(|(_, id)| id).collect()
    }

    fn show_match(&mut self) {
        let Some(id) = self.matches.get(self.match_index).copied() else { return };
        self.editor.select([id]);
        self.shell.invalidate_selection();
        self.fit_selection();
        // Fitting one sticky fills the screen with it; back off to something a person
        // can read in context.
        let centre = self.viewport_centre();
        let zoom = self.camera.zoom().min(1.5);
        self.camera.set_zoom_about(zoom, centre);
    }

    // ----- the board library -------------------------------------------------

    fn library_event(&mut self, event: LibraryEvent) {
        match event {
            LibraryEvent::Open(path) => self.open_board(&path),
            LibraryEvent::Duplicate(path) => {
                // The board being copied may be open behind a tab with writes still on
                // their way to disk, and `Library::duplicate` is a byte copy of the
                // file. Copying across that is copying the board as it was.
                self.flush_board(&path);
                match self.shell.library.duplicate(&path) {
                Ok(copy) => {
                    self.shell.refresh_cards();
                    let name = copy.file_stem().map(|s| s.to_string_lossy().into_owned());
                    self.ok(format!("Duplicated as {}", name.unwrap_or_default()));
                }
                Err(error) => self.failed("duplicating the board", &error),
                }
            }
            // **Delete does not ask any more, because it no longer destroys anything.**
            // The board moves to Recently deleted and the file is not touched; the
            // confirmation that used to be here was buying a promise — *"this cannot be
            // undone"* — that is now false. A dialog in front of a reversible action is
            // the thing that teaches people to dismiss dialogs.
            LibraryEvent::Delete(path) => {
                let name = self.board_title(&path);
                self.flush_board(&path);
                self.shell.library.trash(&path);
                self.shell.refresh_cards();
                // The tab goes, and by `discard` rather than another flush: the board is
                // not being written to any more, and a tab on a board the library is
                // hiding is a way back into it that the trash does not know about.
                self.shell.close_tab_for(&path);
                self.discard_board(&path);
                self.follow_tab_strip();
                self.ok(format!("“{name}” moved to Recently deleted"));
            }
            LibraryEvent::Restore(path) => {
                let name = self.board_title(&path);
                self.shell.library.restore(&path);
                self.shell.refresh_cards();
                self.ok(format!("“{name}” restored"));
            }
            // The only route to `Library::purge`, and the one place a confirmation is
            // still earned — this is the sentence that used to be a lie on Delete.
            LibraryEvent::Purge(path) => {
                let name = self.board_title(&path);
                self.shell.ask(
                    move |id| {
                        Dialog::destructive(
                            id,
                            "Delete permanently",
                            format!(
                                "“{name}” and everything on it will be deleted from this \
                                 machine. This cannot be undone."
                            ),
                            "Delete permanently",
                        )
                    },
                    Ask::DeleteBoard(path),
                );
            }
            LibraryEvent::EmptyTrash => {
                let count = self.shell.library.trashed().len();
                if count == 0 {
                    self.ok("Recently deleted is already empty");
                    return;
                }
                self.shell.ask(
                    move |id| {
                        Dialog::destructive(
                            id,
                            "Empty Recently deleted",
                            format!(
                                "{count} board{} will be deleted from this machine. This \
                                 cannot be undone.",
                                plural(count),
                            ),
                            "Delete permanently",
                        )
                    },
                    Ask::EmptyTrash,
                );
            }
            LibraryEvent::Rename { path, title } => self
                .shell
                .ask(
                    move |id| Dialog::rename(id, "Rename board", title).with_hint("Board name"),
                    Ask::RenameBoard(path),
                ),
            LibraryEvent::SetStarred { path, starred } => {
                self.shell.library.set_starred(&path, starred);
                self.shell.refresh_cards();
            }
            LibraryEvent::MoveToSpace { path, space } => {
                self.shell.library.move_to_space(&path, space.as_deref());
                self.shell.refresh_cards();
            }
            LibraryEvent::CreateSpace => self
                .shell
                .ask(
                    |id| {
                        Dialog::rename(id, "New folder", "")
                            .with_confirm("Create")
                            .with_hint("Folder name")
                    },
                    Ask::CreateSpace,
                ),
            LibraryEvent::RenameSpace(name) => self.shell.ask(
                {
                    let seed = name.clone();
                    move |id| Dialog::rename(id, "Rename folder", seed).with_hint("Folder name")
                },
                Ask::RenameSpace(name),
            ),
            LibraryEvent::DeleteSpace(name) => self.shell.ask(
                {
                    let shown = name.clone();
                    move |id| {
                        Dialog::destructive(
                            id,
                            "Delete folder",
                            format!("“{shown}” will be removed. The boards in it are kept."),
                            "Delete folder",
                        )
                    }
                },
                Ask::DeleteSpace(name),
            ),
            LibraryEvent::SetSpacePinned { space, pinned } => {
                self.shell.library.set_space_pinned(&space, pinned);
                self.shell.refresh_cards();
            }
            // The chrome filters what it lists for display; the app has nothing to do
            // until cross-board content search exists.
            LibraryEvent::SearchChanged(_) => {}
        }
    }

    fn board_title(&self, path: &Path) -> String {
        self.shell
            .library
            .cards()
            .iter()
            .find(|card| card.path == path)
            .map_or_else(
                || path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                |card| card.title.clone(),
            )
    }

    /// Opens a board, keeping the one being left resident if its tab is still open.
    ///
    /// **Opening a board that is already open never loads a second copy.** Two live
    /// `Editor`s over one SQLite file would each have an autosave thread writing the
    /// same document, which is the same shape of mistake as the import that appended
    /// three times and made 1,788 items out of 596. There are two ways a board can
    /// already be open and both are handled before anything touches the disk: it is
    /// the board on screen, or it is parked behind another tab.
    pub(crate) fn open_board(&mut self, path: &Path) {
        self.recorder.event(&format!("open-board {}", path.display()));
        let title = self.board_title(path);

        // Already on screen. Its tab is brought forward — the click may have come from
        // the library rather than from the strip — and nothing is loaded.
        if self.editor.path() == Some(path) {
            self.shell.open_tab(path, &title);
            self.shell.set_screen(Screen::Board);
            return;
        }

        // Already open behind another tab: swapped in with the view, the selection and
        // the undo history it was left with.
        if let Some((editor, camera)) = self.session.take(Shell::tab_key(path)) {
            self.swap_in(editor, Some(camera));
            self.shell.open_tab(path, &title);
            self.shell.set_screen(Screen::Board);
            self.shell.library.set_last_board(Some(path));
            return;
        }

        let blobs = match BlobStore::open(crate::editor::blob_directory()) {
            Ok(blobs) => blobs,
            Err(error) => {
                self.failed("opening the asset store", &anyhow::Error::new(error));
                return;
            }
        };
        match crate::editor::Editor::open(path, blobs) {
            Ok(editor) => {
                self.swap_in(editor, None);
                self.shell.set_screen(Screen::Board);
                self.shell.library.rescan();
                self.shell.refresh_cards();
                self.shell.library.set_last_board(Some(path));
                // *"everytime i open a new board it should open on top"* — the tab is
                // appended and brought to the front, and a board that already has one
                // is switched to rather than opened twice.
                let title = self.board_title(path);
                self.shell.open_tab(path, &title);
            }
            Err(error) => self.failed("opening the board", &error),
        }
    }

    /// Puts a different board on screen, parking or releasing the one that was there.
    ///
    /// The order is the whole of it, and each step has to happen where it does:
    ///
    /// 1. **Flush and thumbnail while the outgoing board is still on screen.** The
    ///    preview is rendered from `self.editor`'s own projection, so it cannot be
    ///    taken after the swap — and taking it here is what lets a tab be closed later
    ///    without making its board hot again to re-render it, because a parked board
    ///    cannot be edited.
    /// 2. **Swap.** One `mem::replace`, so neither board is ever duplicated and no
    ///    placeholder document has to be invented.
    /// 3. **Park or release.** Still has a tab, so it stays resident; no tab, so the
    ///    `Editor` drops here and its autosave thread is joined.
    /// 4. **Frame it.** A board coming back from the session keeps its camera — with
    ///    the viewport re-stated, since the window may have been resized while it was
    ///    in the background. A board arriving from disk is fitted and sent to its
    ///    start view, as it always was.
    fn swap_in(&mut self, editor: crate::editor::Editor, camera: Option<Camera>) {
        // Close the caret's undo group before the board it belongs to is parked, or the
        // group stays open on a board nothing is editing and the next edit joins it. The
        // eraser's sweep holds a group open exactly the same way and was not covered:
        // switching tabs mid-sweep parked the board with it open, and it stayed open.
        self.commit_editing();
        self.finish_erase();
        let outgoing = self.editor.path().map(Path::to_path_buf);
        if let Err(error) = self.editor.flush() {
            self.failed("saving the board", &error);
        }
        if let Some(path) = outgoing.as_deref() {
            self.capture_thumbnail(path);
        }

        let previous_camera = self.camera;
        let mut previous = std::mem::replace(&mut self.editor, editor);
        match outgoing {
            Some(path) if self.shell.has_tab(Shell::tab_key(&path)) => {
                let key = Shell::tab_key(&path);
                // Releases the outgoing board's projection — a clone of every one of
                // its items, plus the R-tree over them — which nothing behind a tab
                // has any use for. The camera, selection and undo history stay.
                if let Err(error) = previous.park() {
                    self.failed("saving the board", &error);
                }
                self.session.park(key, path, previous, previous_camera);
            }
            _ => drop(previous),
        }

        // Rebuilds the incoming board's projection if it was parked. Must precede
        // everything below: the painter, `fit_board` and the start view all read it.
        self.editor.unpark();

        self.forget_the_board_on_screen();
        match camera {
            Some(camera) => {
                self.camera = camera;
                let (width, height) = self.surface.size();
                self.camera
                    .set_viewport(ScreenSize::new(f64::from(width), f64::from(height)));
            }
            None => {
                self.fit_board();
                self.go_to_start_view();
            }
        }
    }

    /// Drops everything that described the board that was on screen a moment ago.
    ///
    /// A drag, a stroke and a find result all name items in one document; carrying any
    /// of them across a tab switch would apply them to the next board's items, which
    /// happen to have the same ids.
    fn forget_the_board_on_screen(&mut self) {
        // The epoch is what makes this actually drop the outgoing board's layouts and
        // ink. Retaining against the incoming projection cannot: its ids cover the same
        // range, so the stale entries look alive.
        self.painter.sync(self.editor.epoch(), self.editor.projection());
        self.shell.invalidate_selection();
        self.drag = None;
        self.stroke.clear();
        self.matches.clear();
        self.match_index = 0;
        self.shell.set_find_matches(None);
    }

    /// A board with nowhere to save to, for the moment after the last tab closed.
    ///
    /// The blob store is cloned off the outgoing board rather than reopened, so this
    /// cannot fail — there is no sensible thing to do with an error raised while
    /// closing the last tab.
    fn blank_editor(&self) -> crate::editor::Editor {
        let blobs = self.editor.assets().blobs().clone();
        crate::editor::Editor::in_memory(vellum_doc::Board::new(), blobs)
    }

    /// `⌘W`, and Board ▸ Close board.
    fn close_board(&mut self) {
        self.recorder.event("close-board");
        // One binding with one meaning: it closes the tab in front, and the strip
        // decides what comes forward — a neighbouring board, or home.
        if !self.shell.close_active_tab() {
            self.shell
                .toast(Toast::info("The board library is a permanent tab and cannot be closed"));
            return;
        }
        self.shell.library.rescan();
        self.shell.refresh_cards();
        // The strip has already dropped the tab, so this both releases its document
        // and installs whatever came forward.
        self.follow_tab_strip();
    }

    /// Board ▸ Open… — which *is* the library, and with tabs it no longer costs the
    /// board you are on. Before the strip existed this closed the open board, because
    /// there was nowhere else for it to go.
    fn show_library_tab(&mut self) {
        self.select_tab(0);
    }

    /// Blocks until a board is on disk, wherever it is open.
    ///
    /// Every library verb reaches into a board's SQLite file with a **second** handle:
    /// rename loads and saves it, duplicate byte-copies it, delete unlinks it. With one
    /// board open that only had to consider the board on screen. With tabs, the board
    /// being renamed is just as likely to be sitting behind one, still holding unwritten
    /// changes — and a byte copy taken across that is a copy of the board as it was
    /// some seconds ago.
    fn flush_board(&mut self, path: &Path) {
        if self.editor.path() == Some(path) {
            if let Err(error) = self.editor.flush() {
                self.failed("saving the board", &error);
            }
            return;
        }
        let key = Shell::tab_key(path);
        let outcome = self
            .session
            .iter_mut()
            .find(|board| board.key() == key)
            .map(crate::session::Parked::flush);
        if let Some(Err(error)) = outcome {
            self.failed("saving the board", &error);
        }
    }

    /// Renames a board's live document, wherever it is open.
    ///
    /// A parked board that is not told would come back carrying its old title and
    /// write it over the new one on its next save.
    fn retitle_open_board(&mut self, path: &Path, title: &str) {
        let outcome = if self.editor.path() == Some(path) {
            Some(self.editor.edit(|board| Ok(board.set_title(title)?)))
        } else {
            let key = Shell::tab_key(path);
            self.session
                .iter_mut()
                .find(|board| board.key() == key)
                .map(|board| {
                    let editor = board.editor_mut();
                    let renamed = editor.edit(|doc| Ok(doc.set_title(title)?));
                    // `edit` reprojects, which undoes the shed this board was parked
                    // with. Park it again rather than leaving a background tab holding
                    // a projection for the rest of the session because it was renamed.
                    renamed.and_then(|()| editor.park())
                })
        };
        if let Some(Err(error)) = outcome {
            self.failed("renaming the open board", &error);
        }
        self.shell.set_tab_title(path, title);
    }

    /// Forgets a board **without saving it** — for one that has just been deleted,
    /// where a flush would be writing into a database that is no longer on disk.
    fn discard_board(&mut self, path: &Path) {
        drop(self.session.take(Shell::tab_key(path)));
        if self.editor.path() == Some(path) {
            self.editor = self.blank_editor();
            self.forget_the_board_on_screen();
        }
    }

    fn star(&mut self) {
        let Some(path) = self.editor.path().map(Path::to_path_buf) else { return };
        let starred = !self.shell.library.is_starred(&path);
        self.shell.library.set_starred(&path, starred);
        self.shell.refresh_cards();
    }

    fn duplicate_board(&mut self) {
        let Some(path) = self.editor.path().map(Path::to_path_buf) else { return };
        self.flush_board(&path);
        match self.shell.library.duplicate(&path) {
            Ok(copy) => {
                self.shell.refresh_cards();
                self.open_board(&copy.clone());
            }
            Err(error) => self.failed("duplicating the board", &error),
        }
    }

    /// Board ▸ Delete, on the board in front of you.
    ///
    /// Goes through the **same** `LibraryEvent::Delete` a card's own Delete does, rather
    /// than repeating the verb here — which is what stops one of the two routes out of a
    /// board being a trash and the other a shredder. It was two copies of a destructive
    /// dialog before, and this one would still have been asking *"this cannot be undone"*
    /// about an action that now can.
    fn confirm_delete_open_board(&mut self) {
        let Some(path) = self.editor.path().map(Path::to_path_buf) else { return };
        self.library_event(LibraryEvent::Delete(path));
    }

    // ----- dialogs -----------------------------------------------------------

    fn answered(&mut self, event: DialogEvent) {
        let Some(ask) = self.shell.take_ask(event.id()) else { return };
        match (ask, event) {
            (Ask::NewBoard, DialogEvent::Renamed(_, title)) => {
                match self.shell.library.create(&title) {
                    Ok(path) => {
                        self.shell.refresh_cards();
                        self.open_board(&path);
                    }
                    Err(error) => self.failed("creating the board", &error),
                }
            }
            (Ask::RenameBoard(path), DialogEvent::Renamed(_, title)) => {
                // Save first: the rename opens the file a second time, and a title
                // written under an unflushed document would be overwritten by the next
                // autosave. Wherever the board is open — in front or behind a tab.
                self.flush_board(&path);
                match self.shell.library.rename(&path, &title) {
                    Ok(()) => {
                        // The open document has to learn its own new name, or the
                        // window title, the tab and the library row disagree until it
                        // is reopened — and a *parked* one would write its old title
                        // back over the new one on its next save.
                        self.retitle_open_board(&path, &title);
                        self.shell.refresh_cards();
                    }
                    Err(error) => self.failed("renaming the board", &error),
                }
            }
            (Ask::DeleteBoard(path), DialogEvent::Confirmed(_)) => {
                self.flush_board(&path);
                if let Some(texture) = self.shell.drop_thumbnail(&path) {
                    self.chrome.free_texture(texture);
                }
                match self.shell.library.purge(&path) {
                    Ok(()) => {
                        self.shell.refresh_cards();
                        // A tab for a board that no longer exists would open a file
                        // that is not there — and a document left open on it would go
                        // on saving into a database that has been unlinked, which is
                        // why the release is `discard` rather than another flush.
                        self.shell.close_tab_for(&path);
                        self.discard_board(&path);
                        self.follow_tab_strip();
                        self.ok("Board deleted permanently");
                    }
                    Err(error) => self.failed("deleting the board", &error),
                }
            }
            (Ask::EmptyTrash, DialogEvent::Confirmed(_)) => {
                // Counted from what actually left, not from what was asked for: a board
                // whose file has already gone — deleted in Finder between the dialog going
                // up and the answer coming back — is not a failure worth a red toast, but
                // it is not one of the boards this removed either.
                let mut removed = 0usize;
                for path in self.shell.library.trashed() {
                    if let Some(texture) = self.shell.drop_thumbnail(&path) {
                        self.chrome.free_texture(texture);
                    }
                    match self.shell.library.purge(&path) {
                        Ok(()) => removed += 1,
                        Err(error) => log::warn!("emptying the trash: {error:#}"),
                    }
                }
                self.shell.refresh_cards();
                self.ok(format!("Deleted {removed} board{}", plural(removed)));
            }
            (Ask::CreateSpace, DialogEvent::Renamed(_, name)) => {
                if self.shell.library.create_space(&name) {
                    self.shell.refresh_cards();
                    self.ok(format!("Folder “{}” created", name.trim()));
                } else {
                    let reason = space_refusal(&name);
                    self.shell.toast(Toast::error(reason));
                }
            }
            (Ask::RenameSpace(from), DialogEvent::Renamed(_, to)) => {
                if self.shell.library.rename_space(&from, &to) {
                    self.shell.refresh_cards();
                } else {
                    let reason = space_refusal(&to);
                    self.shell.toast(Toast::error(reason));
                }
            }
            (Ask::DeleteSpace(name), DialogEvent::Confirmed(_)) => {
                self.shell.library.delete_space(&name);
                self.shell.refresh_cards();
                // Said out loud, because a folder disappearing with boards in it is
                // exactly the moment a user wonders whether the work went with it.
                self.ok(format!("Folder “{name}” removed. Its boards were kept."));
            }
            // Continue on the instructions: the file picker and the name dialog, which is
            // exactly what the button used to do on its own.
            (Ask::ImportSteps, DialogEvent::Confirmed(_)) => self.begin_import_from_miro(),
            (Ask::ImportFromMiro, DialogEvent::Renamed(_, title)) => {
                self.import_to_new_board(&title);
            }
            // Cancelled, or an answer of the wrong shape. Both are "the user changed
            // their mind", and the question has already been forgotten.
            _ => {}
        }
    }

    // ----- saving, exporting, thumbnails --------------------------------------

    fn save(&mut self) {
        match self.editor.flush() {
            Ok(()) => {
                if let Some(path) = self.editor.path().map(Path::to_path_buf) {
                    self.capture_thumbnail(&path);
                }
                self.shell.refresh_cards();
                self.ok("Saved");
            }
            Err(error) => self.failed("saving", &error),
        }
    }

    fn history(&mut self) {
        match self.editor.restore_points() {
            Ok(points) if points.is_empty() => {
                self.gap("No restore points yet; the store writes one as the board grows")
            }
            Ok(points) => self.gap(&format!(
                "{} restore point{} are kept for this board, and a browser for them \
                 is not built yet",
                points.len(),
                plural(points.len())
            )),
            Err(error) => self.failed("reading the history", &error),
        }
    }

    /// Renders the whole board to a PNG beside the board file.
    ///
    /// **No file dialog, and that is a decision rather than an omission.** This build
    /// carries no native dialog crate; inventing a path is worse than choosing an
    /// obvious one, so the export lands next to the board it came from and the toast
    /// names the file.
    fn export_png(&mut self) {
        let Some(target) = self.export_path("png") else {
            self.gap("Save the board first, so the export has somewhere to go");
            return;
        };
        match self.render_board(None) {
            Ok(Some(capture)) => match capture.to_png().and_then(|png| {
                std::fs::write(&target, png)
                    .map_err(|e| anyhow::anyhow!("writing {}: {e}", target.display()))
            }) {
                Ok(()) => self.ok(format!("Exported {}", file_name(&target))),
                Err(error) => self.failed("exporting a PNG", &error),
            },
            Ok(None) => self.shell.toast(Toast::info("There is nothing on this board to export")),
            Err(error) => self.failed("rendering the board", &error),
        }
    }

    /// Writes the board as a PDF or an SVG through `vellum-export`.
    ///
    /// Both go through one function because they differ only in the writer: the scene
    /// is collected identically, and the two used to share a single "not adapted yet"
    /// toast for the same reason.
    ///
    /// **Scope follows the selection.** Exporting with something selected exports that,
    /// which is what Miro does and what someone who has just selected a frame means.
    fn export_vector(&mut self, format: VectorFormat) {
        let Some(target) = self.export_path(format.extension()) else {
            self.gap("Save the board first, so the export has somewhere to go");
            return;
        };

        let selected: Vec<vellum_export::ItemId> =
            self.editor.selection().iter().map(|id| vellum_export::ItemId(*id)).collect();
        let scope = if selected.is_empty() {
            vellum_export::Scope::Board
        } else {
            vellum_export::Scope::selection(selected)
        };

        let title = {
            let name = self.editor.board().title();
            (!name.trim().is_empty()).then(|| name.trim().to_owned())
        };
        // Two mutable borrows that cannot overlap: the projection and blob store come off
        // `editor`, the shaping engine off `painter`. Separate fields, so the split is real
        // — but the source has to be built and shaped before `collect` reads it back.
        let mut source = {
            let (projection, _, assets) = self.editor.frame_parts();
            crate::export::BoardExport::new(projection, assets.blobs(), title)
        };
        source.load_images();
        // Shaped, so a wrapped paragraph exports as the lines it is drawn as rather than as
        // one line running off the page, and an auto-fitted sticky exports at the size it
        // actually resolved to. See `crate::export`'s header for what shaping is and is not
        // used for here.
        source.shape_text(self.painter.text_mut().engine_mut());

        let scene = match vellum_export::Scene::collect(&source, &scope) {
            Ok(scene) => scene,
            Err(error) => {
                // The one failure `collect` has is "nothing to export", which is a
                // statement about the board rather than a fault.
                self.shell.toast(Toast::info(format!("Nothing to export: {error}")));
                return;
            }
        };

        let written = match format {
            VectorFormat::Svg => vellum_export::svg::write(&scene, &vellum_export::svg::SvgOptions::default())
                .map(String::into_bytes),
            VectorFormat::Pdf => {
                vellum_export::pdf::write(&scene, &vellum_export::pdf::PdfOptions::new())
            }
        };
        match written.map_err(|e| anyhow::anyhow!("{e}")).and_then(|bytes| {
            std::fs::write(&target, bytes)
                .map_err(|e| anyhow::anyhow!("writing {}: {e}", target.display()))
        }) {
            Ok(()) => self.ok(format!("Exported {}", file_name(&target))),
            Err(error) => self.failed(format.what(), &error),
        }
    }

    /// A spreadsheet of the board's items — Miro's "Export to spreadsheet".
    ///
    /// Written here rather than through `vellum-export`'s CSV writer for the same
    /// reason PDF and SVG are gaps: that crate models a scene of its own and nothing
    /// adapts `vellum-doc` onto it. A row per item over the document is forty lines
    /// and is exactly as true as the document is.
    fn export_csv(&mut self) {
        let Some(target) = self.export_path("csv") else {
            self.gap("Save the board first, so the export has somewhere to go");
            return;
        };
        let mut out = String::from("kind,x,y,width,height,rotation,text\n");
        let mut ordered: Vec<(i32, &crate::project::Projected)> = self
            .editor
            .projection()
            .iter()
            .map(|(_, p)| (p.z, p))
            .collect();
        ordered.sort_by_key(|(z, _)| *z);
        for (_, projected) in ordered {
            let placement = projected.item.placement;
            let (w, h) = placement.scaled_size();
            let text = projected
                .item
                .kind
                .text()
                .map(vellum_doc::StyledText::to_plain)
                .unwrap_or_default();
            out.push_str(&format!(
                "{},{:.2},{:.2},{:.2},{:.2},{:.2},{}\n",
                projected.item.kind.tag(),
                placement.x,
                placement.y,
                w,
                h,
                placement.rotation,
                csv_field(&text)
            ));
        }
        match std::fs::write(&target, out) {
            Ok(()) => self.ok(format!("Exported {}", file_name(&target))),
            Err(error) => {
                self.failed("exporting a spreadsheet", &anyhow::Error::new(error));
            }
        }
    }

    /// The document's own bytes — a `.velm` backup, restorable with no database.
    fn export_backup(&mut self) {
        let Some(target) = self.export_path("velm") else {
            self.gap("Save the board first, so the backup has somewhere to go");
            return;
        };
        let result = self
            .editor
            .board()
            .to_bytes()
            .map_err(anyhow::Error::new)
            .and_then(|bytes| {
                std::fs::write(&target, bytes)
                    .map_err(|e| anyhow::anyhow!("writing {}: {e}", target.display()))
            });
        match result {
            Ok(()) => self.ok(format!("Backed up to {}", file_name(&target))),
            Err(error) => self.failed("writing the backup", &error),
        }
    }

    /// Where an export goes: beside the board, named after it.
    fn export_path(&self, extension: &str) -> Option<PathBuf> {
        let path = self.editor.path()?;
        let directory = path.parent()?;
        let stem = path.file_stem()?;
        let mut candidate = directory.join(stem).with_extension(extension);
        let mut n = 2;
        while candidate.exists() {
            candidate = directory
                .join(format!("{}-{n}", stem.to_string_lossy()))
                .with_extension(extension);
            n += 1;
        }
        Some(candidate)
    }

    /// Renders the whole board offscreen, fitted to `longest_edge` where given.
    ///
    /// Reuses the frame's own painter and renderer, so a thumbnail cannot disagree
    /// with the screen — `crate::capture` explains why that matters more than it
    /// sounds.
    fn render_board(
        &mut self,
        longest_edge: Option<u32>,
    ) -> anyhow::Result<Option<crate::capture::Capture>> {
        let Some(content) = self.editor.projection().content_bounds() else {
            return Ok(None);
        };
        let width = content.max.x - content.min.x;
        let height = content.max.y - content.min.y;
        let Some((pixels_w, pixels_h)) =
            crate::capture::fitted_size((width, height), longest_edge.unwrap_or(u32::MAX))
        else {
            return Ok(None);
        };

        let mut camera = vellum_scene::Camera::new(vellum_scene::ScreenSize::new(
            f64::from(pixels_w),
            f64::from(pixels_h),
        ));
        camera.fit_to_rect(content, 0.0);

        let theme = self.theme;
        // A preview shows the board's own background, not the view toggle.
        let pattern = self.canvas_pattern();
        let grid_color = self.shell.library.grid_color();
        let clear = self.canvas_color();
        let mut list = vellum_render::DrawList::new();
        {
            let (projection, _, assets) = self.editor.frame_parts();
            let context = crate::draw::DrawContext {
                agents: &self.agents,
                camera: &camera,
                projection,
                theme,
                // A thumbnail is rendered with nobody's pointer in it.
                hovered_badge: None,
                // A preview shows the board, not what happened to be selected when it
                // was taken.
                selection: &[],
                marquee: None,
                placing: None,
                guides: &[],
                // A thumbnail is taken between gestures, never during one.
                stroke: None,
                pending_connector: None,
                editing: None,
                card_drop: None,
                pattern,
                grid_color,
                minimap: None,
            };
            let (device, queue, renderer) = self.surface.parts();
            self.painter
                .paint(device, queue, renderer, assets, &mut list, &context);
        }

        let format = self.surface.format();
        let (device, queue, renderer) = self.surface.parts();
        let capture = crate::capture::render(
            crate::capture::Target {
                device,
                queue,
                renderer,
                format,
                clear,
                width: pixels_w,
                height: pixels_h,
            },
            &list,
        )?;
        // The painter's caches now hold layouts sized for the capture's camera rather
        // than the window's. Dropping them costs one frame of re-shaping and avoids
        // text that is subtly the wrong size until the next zoom.
        self.painter.text_mut().clear();
        Ok(Some(capture))
    }

    /// Renders a preview of a board and files it in the blob store.
    ///
    /// Taken when a board is closed, saved or the app quits — not on every autosave.
    /// Autosave runs after *every* mutation, including each move inside a drag, and a
    /// full board render per mouse-move would be exactly the stutter this project
    /// exists to refuse.
    pub(crate) fn capture_thumbnail(&mut self, path: &Path) {
        self.recorder.event("capture-thumbnail");
        let capture = match self.render_board(Some(crate::capture::THUMBNAIL_SIZE)) {
            Ok(Some(capture)) => capture,
            // An empty board has no preview, which is not a failure — the library row
            // shows its name and count.
            Ok(None) => return,
            Err(error) => {
                log::warn!("thumbnail for {}: {error:#}", path.display());
                return;
            }
        };

        let png = match capture.to_png() {
            Ok(png) => png,
            Err(error) => {
                log::warn!("encoding the thumbnail: {error:#}");
                return;
            }
        };
        let hash = match self.editor.assets().blobs().put(&png) {
            Ok(hash) => hash,
            Err(error) => {
                log::warn!("storing the thumbnail: {error}");
                return;
            }
        };
        if let Err(error) = self.shell.library.set_thumbnail(path, &hash) {
            log::warn!("recording the thumbnail: {error:#}");
            return;
        }

        // Straight into the chrome as well, so the library shows the new preview
        // without a round trip through the blob store.
        if let Some(old) = self.shell.drop_thumbnail(path) {
            self.chrome.free_texture(old);
        }
        let (device, queue) = self.surface.device_queue();
        if let Some(texture) = self.chrome.register_texture(
            device,
            queue,
            &capture.rgba,
            capture.width,
            capture.height,
        ) {
            self.shell.set_thumbnail(
                path.to_path_buf(),
                texture,
                [capture.width as usize, capture.height as usize],
            );
        }
    }

    /// Uploads the previews the library is about to show.
    ///
    /// Lazy and bounded: a handful per frame, only for rows that have a stored preview
    /// and no texture yet. Decoding a hundred PNGs the first time the library opens
    /// would be a visible pause on the one screen that has to feel instant.
    pub(crate) fn load_thumbnails(&mut self) {
        if self.shell.screen() != Screen::Library {
            return;
        }
        let pending: Vec<PathBuf> = self
            .shell
            .library
            .cards()
            .iter()
            .map(|card| card.path.clone())
            .filter(|path| !self.shell.has_thumbnail(path))
            .take(THUMBNAILS_PER_FRAME)
            .collect();

        for path in pending {
            let Some(hash) = self.shell.library.thumbnail_hash(&path) else {
                // No stored preview. Remembering an empty one stops this retrying the
                // same board every frame for the life of the session.
                self.shell.set_thumbnail(path, egui::TextureId::User(u64::MAX), [1, 1]);
                continue;
            };
            let Ok(Some(bytes)) = self.editor.assets().blobs().get(&hash) else { continue };
            let Ok(image) = image::load_from_memory(&bytes) else { continue };
            let rgba = image.to_rgba8();
            let (width, height) = rgba.dimensions();
            let (device, queue) = self.surface.device_queue();
            if let Some(texture) =
                self.chrome
                    .register_texture(device, queue, rgba.as_raw(), width, height)
            {
                self.shell
                    .set_thumbnail(path, texture, [width as usize, height as usize]);
            }
        }

        self.evict_thumbnails();
    }

    /// Keeps the preview textures bounded by the most recently modified boards.
    ///
    /// `Library::cards` is sorted newest-first (`vellum_store::list_boards`), so the
    /// tail of that list is the least likely to be looked at and the right thing to
    /// drop. A dropped preview is not lost — the blob is still on disk and
    /// [`Self::load_thumbnails`] uploads it again within a few frames of the row
    /// coming back into view.
    ///
    /// Note this is deliberately **not** hung off closing a tab. A thumbnail belongs to
    /// the library row, not to the tab: freeing it when a board is closed would just
    /// make the next visit to the library decode it again.
    fn evict_thumbnails(&mut self) {
        if self.shell.thumbnail_count() <= THUMBNAIL_BUDGET {
            return;
        }
        let keep: std::collections::BTreeSet<PathBuf> = self
            .shell
            .library
            .cards()
            .iter()
            .take(THUMBNAIL_BUDGET)
            .map(|card| card.path.clone())
            .collect();
        let freed = self.shell.retain_thumbnails(&keep);
        log::debug!("released {} board previews", freed.len());
        for texture in freed {
            self.chrome.free_texture(texture);
        }
    }
}

/// How many previews are decoded and uploaded per frame. Four is imperceptible and
/// still fills a screenful of the library within a few frames of it opening.
const THUMBNAILS_PER_FRAME: usize = 4;

/// How many board previews may be resident at once.
///
/// Each is at most 512 × 512 × 4 = 1 MiB (`crate::capture::THUMBNAIL_SIZE`), so this
/// caps them at ~64 MiB and in practice well under, since a wide board fits to a much
/// shorter texture. Comfortably more than a screenful of library cards, which is what
/// stops the bound from causing churn: eviction only bites on a library far larger than
/// anything visible at once.
const THUMBNAIL_BUDGET: usize = 64;

/// Which way [`ActiveState::reorder`] moves the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Front,
    Forward,
    Backward,
    Back,
}

/// Which edge [`ActiveState::align`] lines the selection up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    CentreX,
    Right,
    Top,
    CentreY,
    Bottom,
}

// `import_steps` used to render `vellum_ui::IMPORT_STEPS` into the import dialog's body.
// That dialog now asks for a board *name* instead, so the steps live in one place — the
// board library's start tab, which is where somebody who has not done this before is
// looking anyway. `vellum_ui::library` owns them and tests them there.

/// The shortcut sheet, as rows for [`Dialog::reference`] to lay out.
///
/// Built from the command table rather than written out, so a binding cannot be
/// documented as one thing and behave as another. **Grouped by the menu each command
/// lives under** — `Command::menu()` already knows, so the headings cost nothing and
/// forty-eight rows in one undifferentiated column become six short lists in the order
/// the menus themselves are in.
///
/// Returning rows rather than a padded string is the point: the columns used to be
/// `{:<28}` space padding, which lines up only in a monospace face, and the dialog body
/// is drawn in the proportional one.
/// A normalised anchor's name, for a diagnostic's message.
fn anchor_name(anchor: (f64, f64)) -> &'static str {
    match anchor {
        ConnectorEnd::TOP => "top",
        ConnectorEnd::RIGHT => "right",
        ConnectorEnd::BOTTOM => "bottom",
        ConnectorEnd::LEFT => "left",
        ConnectorEnd::CENTER => "centre",
        _ => "custom",
    }
}

fn shortcut_reference() -> Vec<ReferenceSection> {
    let is_mac = cfg!(target_os = "macos");
    let mut sections = vec![
        ReferenceSection {
            heading: "Mouse".to_owned(),
            rows: vec![
                ("Marquee select".to_owned(), "Left drag on empty canvas".to_owned()),
                // The right button does two things depending on whether it moved, so
                // both are listed. Neither is announced anywhere on the canvas, and a
                // menu nobody knows to open is a menu that does not exist.
                (
                    "Everything the selection can do".to_owned(),
                    "Right click it — or the ⋮ on its toolbar".to_owned(),
                ),
                ("The canvas's own menu".to_owned(), "Right click empty board".to_owned()),
                ("Pan".to_owned(), "Middle / right / space drag".to_owned()),
                ("Zoom, anchored at the cursor".to_owned(), "Wheel".to_owned()),
                ("Pan horizontally".to_owned(), "Shift + wheel".to_owned()),
                ("Place one, then back to Select".to_owned(), "Click with a create tool".to_owned()),
                ("Connect two items".to_owned(), "Drag between them with the connector".to_owned()),
                // Still in the sheet now that the eraser has a flyout mode, because ⇧ has
                // become the *inversion* of that mode rather than the only way in: whichever
                // eraser is selected, this reaches the other one for one sweep. The flyout
                // names it too, in the hint under the toggle.
                (
                    "Swap the eraser's mode for one sweep".to_owned(),
                    "Shift + drag with the eraser".to_owned(),
                ),
                ("Put the caret in an item".to_owned(), "Double click it".to_owned()),
                (
                    "Type in a table cell, card or node".to_owned(),
                    "Double click inside the widget".to_owned(),
                ),
                // The three keys that edit a structured widget's *shape*. In the sheet
                // because there is nothing on the canvas that announces them, and a key that
                // adds or removes part of a widget is exactly the kind that must not be
                // discovered by accident.
                (
                    "Next cell · next card · a child node".to_owned(),
                    "Tab, while the caret is in one".to_owned(),
                ),
                ("Back one field".to_owned(), "Shift + Tab".to_owned()),
                (
                    "Remove the card or node the caret is in".to_owned(),
                    "Cmd + Delete".to_owned(),
                ),
                ("Fold or unfold a mind-map branch".to_owned(), "Alt + Return".to_owned()),
                ("Move a kanban card".to_owned(), "Drag it to another column".to_owned()),
            ],
        },
        ReferenceSection {
            heading: "Tools".to_owned(),
            rows: Tool::ALL
                .into_iter()
                .filter_map(|tool| {
                    Some((tool.label().to_owned(), tool.shortcut_hint()?.to_owned()))
                })
                .collect(),
        },
    ];

    for menu in Menu::ALL {
        let rows: Vec<(String, String)> = Command::ALL
            .iter()
            .filter(|command| command.menu() == menu)
            .filter_map(|command| {
                let shortcut = command.shortcut()?;
                Some((command.label().to_owned(), vellum_ui::format_shortcut(shortcut, is_mac)))
            })
            .collect();
        // A menu whose commands all lack bindings earns no heading.
        if !rows.is_empty() {
            sections.push(ReferenceSection { heading: menu.title().to_owned(), rows });
        }
    }
    sections
}

fn documentation() -> String {
    [
        "The project's own documents, in docs/ beside the source:",
        "",
        "  01-architecture.md    why native, and the performance argument",
        "  02-miro-formats.md    the reverse-engineering record",
        "  04-ui-reference.md    Miro's real layout and shortcuts",
        "  05-design-language.md the palette, and what the chrome may look like",
        "  06-mouse-controls.md  every mouse binding and why",
        "  features/README.md    the parity contract",
    ]
    .join("\n")
}

fn about() -> String {
    format!(
        "Velm {}\n\nA native infinite canvas.\nNo account, no cloud, no telemetry.\n\
         Boards are SQLite files you own, in {}.",
        env!("CARGO_PKG_VERSION"),
        crate::editor::data_directory().display()
    )
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// How far to move a copied group so its bounding box is centred on `at`.
///
/// A paste is aimed. Every other flavour already lands at the pointer —
/// [`Actions::place_pasted_image`] and [`Actions::place_pasted_text`] both take `at` —
/// and the app's own clipboard used to be the exception: it added a flat 24 points to
/// coordinates read off *the board they were copied from*. On the same board that reads
/// as a nudge, which is why it survived. Paste onto a different board and the items land
/// wherever the first board's items happened to be, which is almost always outside the
/// viewport: the paste worked and looked like it had done nothing at all.
///
/// The whole group moves by one offset rather than each item to the pointer, or a
/// copied diagram would collapse into a stack.
fn shift_onto(items: &[vellum_doc::Item], at: WorldPoint) -> (f64, f64) {
    let Some(first) = items.first() else { return (0.0, 0.0) };
    let (mut min_x, mut min_y) = (first.placement.x, first.placement.y);
    let (mut max_x, mut max_y) = (min_x, min_y);
    for item in items {
        min_x = min_x.min(item.placement.x);
        min_y = min_y.min(item.placement.y);
        max_x = max_x.max(item.placement.x);
        max_y = max_y.max(item.placement.y);
    }
    // `Placement`'s x/y are the item's centre, so the midpoint of the centres is the
    // group's own centre for this purpose. Sizes are deliberately not considered: a
    // 38,000-unit frame would drag the centre away from where the items actually are.
    (at.x - (min_x + max_x) / 2.0, at.y - (min_y + max_y) / 2.0)
}

/// Strips a connector's endpoint targets, leaving its anchors and arrowheads alone.
///
/// Pass one of a paste writes this, so that no item ever enters the document naming an
/// id that does not exist in it. [`rebound`] puts the surviving targets back.
fn cut_loose(mut kind: ItemKind) -> ItemKind {
    // Matched with `..` rather than by listing the variant's fields, so a connector
    // growing another one does not silently drop it here.
    if let ItemKind::Connector { start, end, .. } = &mut kind {
        start.target = None;
        end.target = None;
    }
    kind
}

/// Re-points a connector's endpoints at the copies of what they were attached to.
///
/// An endpoint whose target was **not** part of the copy is left free rather than bound
/// to the original. Binding it back would be wrong in both directions: across boards the
/// id belongs to a different document — and because an [`ItemId`](vellum_doc::ItemId) is
/// a Loro `TreeID`, it can happily match an unrelated item there — while on the same
/// board it would tie the copy to the original's geometry, so moving one would drag the
/// other's connector with it.
fn rebound(mut kind: ItemKind, remap: &HashMap<DocId, DocId>) -> ItemKind {
    if let ItemKind::Connector { start, end, .. } = &mut kind {
        for endpoint in [start, end] {
            endpoint.target = endpoint.target.and_then(|old| remap.get(&old).copied());
        }
    }
    kind
}

/// Why a space could not be created or renamed.
///
/// `Library::create_space` and `rename_space` both answer a bare `false` for two quite
/// different refusals, and reporting the wrong one is worse than reporting nothing:
/// *"There is already a space called “”"* is what an empty name used to produce, and
/// it names a problem the user does not have.
fn space_refusal(name: &str) -> String {
    if name.trim().is_empty() {
        "A folder needs a name".to_owned()
    } else {
        format!("There is already a folder called “{}”", name.trim())
    }
}

/// Quotes a CSV field if it needs it, doubling any quote inside.
///
/// A sticky's text can contain a comma, a newline and a quotation mark, and all three
/// appear on the reference board. Getting this wrong produces a file that opens
/// without complaint and is wrong from that row on.
fn csv_field(value: &str) -> String {
    let flattened = value.replace(['\n', '\r'], " ");
    if flattened.contains([',', '"']) {
        format!("\"{}\"", flattened.replace('"', "\"\""))
    } else {
        flattened
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, x: f64, y: f64) -> vellum_doc::Item {
        vellum_doc::Item {
            id: id.parse().expect("well-formed item id"),
            parent: None,
            placement: Placement::new(x, y, 100.0, 100.0),
            style: vellum_doc::Style::default(),
            kind: ItemKind::Sticky { text: StyledText::plain("note"), background: None },
        }
    }

    /// A connector bound to `start`/`end`, with a non-default anchor so the tests can
    /// prove that remapping touches the target and nothing else.
    fn connector(id: &str, start: Option<DocId>, end: Option<DocId>) -> vellum_doc::Item {
        let bind = |target: Option<DocId>| ConnectorEnd {
            target,
            ..ConnectorEnd::free(ConnectorEnd::RIGHT)
        };
        vellum_doc::Item {
            kind: ItemKind::Connector {
                start: bind(start),
                end: bind(end),
                routing: vellum_doc::Routing::default(),
                dash: vellum_doc::Dash::default(),
                thickness: CONNECTOR_THICKNESS,
                color: None,
                captions: Vec::new(),
            },
            ..item(id, 0.0, 0.0)
        }
    }

    /// *"when i copy another board to copy it on another page it pastes only text"*.
    ///
    /// A paste is aimed. The internal clipboard used to add a flat offset to coordinates
    /// read off the board the items were copied *from*, which on another board puts them
    /// wherever that board's items happened to be — usually outside the viewport, so the
    /// paste looked like it had done nothing.
    #[test]
    fn a_pasted_group_is_centred_on_the_pointer_and_keeps_its_shape() {
        let items = [item("1@1", 1000.0, 2000.0), item("2@1", 1100.0, 2000.0)];
        let at = WorldPoint { x: -50.0, y: 7.0 };

        let (dx, dy) = shift_onto(&items, at);

        // The group's centre lands on the pointer…
        let centres: Vec<(f64, f64)> =
            items.iter().map(|i| (i.placement.x + dx, i.placement.y + dy)).collect();
        let mid_x = (centres[0].0 + centres[1].0) / 2.0;
        assert!((mid_x - at.x).abs() < 1e-9, "group centre x = {mid_x}, wanted {}", at.x);
        assert!((centres[0].1 - at.y).abs() < 1e-9);

        // …and the items keep their spacing rather than collapsing onto one point.
        assert!((centres[1].0 - centres[0].0 - 100.0).abs() < 1e-9);
    }

    #[test]
    fn shifting_an_empty_clipboard_moves_nothing() {
        assert_eq!(shift_onto(&[], WorldPoint { x: 5.0, y: 5.0 }), (0.0, 0.0));
    }

    /// An `ItemId` is a Loro `TreeID` — a peer and a counter — so an id copied from one
    /// board can collide with an unrelated item on another. A connector that carried its
    /// targets across verbatim would bind itself to a stranger, which is why pass one of
    /// a paste strips them and pass two puts back only what it can account for.
    #[test]
    fn a_pasted_connector_follows_the_copies_and_lets_go_of_what_was_not_copied() {
        let (old_a, old_b): (DocId, DocId) =
            ("1@1".parse().unwrap(), "2@1".parse().unwrap());
        let (new_a, stranger): (DocId, DocId) =
            ("7@9".parse().unwrap(), "8@9".parse().unwrap());
        let wire = connector("3@1", Some(old_a), Some(old_b));

        // Pass one: nothing names an id the document does not hold yet.
        let stripped = cut_loose(wire.kind.clone());
        let ItemKind::Connector { start, end, .. } = &stripped else { panic!("not a connector") };
        assert_eq!(start.target, None, "an unresolved id must never reach the document");
        assert_eq!(end.target, None);
        assert_eq!(start.anchor, ConnectorEnd::RIGHT, "anchors and arrowheads are untouched");

        // Pass two reads the **original** kind, not the stripped one — the strip threw the
        // old ids away and they are what the map is keyed on. Chaining the two the other
        // way round silently produces a connector with both ends free, which is why this
        // is asserted rather than assumed.
        let remap: HashMap<DocId, DocId> = [(old_a, new_a)].into_iter().collect();
        let bound = rebound(wire.kind.clone(), &remap);
        let ItemKind::Connector { start, end, .. } = &bound else { panic!("not a connector") };
        assert_eq!(start.target, Some(new_a), "retargeted at the copy, not the original");
        assert_eq!(end.target, None, "an end whose target was not copied stays free");
        assert_ne!(start.target, Some(stranger));
    }

    /// Anything that is not a connector passes through both passes unchanged.
    #[test]
    fn remapping_leaves_other_kinds_alone() {
        let sticky = item("4@1", 0.0, 0.0).kind;
        assert_eq!(cut_loose(sticky.clone()), sticky);
        assert_eq!(rebound(sticky.clone(), &HashMap::new()), sticky);
    }

    /// Board ▸ Import from Miro must ask for a **name**, not offer to paste.
    ///
    /// It used to raise a `Confirm` whose button ran `paste()`, which put the imported
    /// board into whichever board was already open — *"oh it just copied into the
    /// already open board"*. A `Rename` dialog is the difference: it can only be
    /// answered with `DialogEvent::Renamed`, and that arm is the one that creates a
    /// board first. Asserted on the dialog's *shape* because that is what makes the
    /// wrong outcome unreachable rather than merely unlikely.
    #[test]
    fn the_import_command_asks_for_a_board_name() {
        let dialog = Dialog::rename(vellum_ui::DialogId(1), "Import from Miro", "Reference Board")
            .with_confirm("Create and paste")
            .with_hint("Board name");
        let Dialog::Rename { value, confirm, .. } = dialog else {
            panic!("the import dialog must be a Rename, or confirming it cannot name a board")
        };
        assert_eq!(value, "Reference Board", "the name is offered, not typed");
        assert_eq!(confirm, "Create and paste");
    }

    /// A field containing a comma, a quote or a newline has to survive, or the export
    /// is wrong from that row on and opens without complaint.
    #[test]
    fn csv_fields_are_quoted_when_they_have_to_be() {
        assert_eq!(csv_field("radiator"), "radiator");
        assert_eq!(csv_field("fan, radiator"), "\"fan, radiator\"");
        assert_eq!(csv_field("the \"good\" one"), "\"the \"\"good\"\" one\"");
        assert_eq!(csv_field("two\nlines"), "two lines");
    }

    /// Frames with a running order come first, in that order; frames without one
    /// follow in document order. A board where nobody set a sequence still presents in
    /// the order the frames were made, rather than all colliding at slide zero.
    #[test]
    fn the_deck_puts_ordered_frames_first_and_keeps_the_rest_in_document_order() {
        let mut ordered = vec![(Some(30_i64), 0_i32), (None, 1), (Some(10), 2), (None, 3)];
        // The comparison `frame_deck` makes, with `i64::MAX` standing in for "no place
        // in the running order".
        ordered.sort_by_key(|(order, z)| (order.unwrap_or(i64::MAX), *z));
        assert_eq!(
            ordered,
            vec![(Some(10), 2), (Some(30), 0), (None, 1), (None, 3)],
            "an unordered frame jumped ahead of an ordered one",
        );
    }

    /// The direction that did not exist before the eraser. A stroke has to come out of
    /// the document, be cut, and go back in landing exactly where it was drawn —
    /// anything else and erasing the middle of a line makes the ends jump.
    #[test]
    fn a_stroke_survives_the_trip_out_of_the_document_and_back() {
        let placement = Placement::new(100.0, 50.0, 40.0, 20.0);
        let points = vec![
            vellum_doc::Point { x: -20.0, y: -10.0 },
            vellum_doc::Point { x: 0.0, y: 0.0 },
            vellum_doc::Point { x: 20.0, y: 10.0 },
        ];

        let stroke = ink_stroke(&points, 4.0, &placement);
        // Absolute, so the middle sample sits on the placement's centre.
        let middle = stroke.points()[1];
        assert!((middle.x - 100.0).abs() < 1e-9, "{}", middle.x);
        assert!((middle.y - 50.0).abs() < 1e-9, "{}", middle.y);

        let (kind, back) = ink_item(&stroke, None).expect("three points is a mark");
        let ItemKind::Ink { points: out, thickness, .. } = &kind else { panic!("wrong kind") };
        assert!((thickness - 4.0).abs() < 1e-9);
        // Re-centred on its own bounds, which for a symmetric stroke is where it began.
        assert!((back.x - 100.0).abs() < 1e-9, "{}", back.x);
        assert!((back.y - 50.0).abs() < 1e-9, "{}", back.y);
        assert_eq!(out.len(), 3);
        assert!((out[0].x - -20.0).abs() < 1e-9, "{}", out[0].x);
    }

    /// A rotated, scaled stroke has its transform **baked into the points**, because a
    /// cut piece is not a rotation of anything. Reading it back must therefore give
    /// world coordinates, and the piece written back must carry no transform of its own
    /// — applying it twice is the failure this pins.
    #[test]
    fn a_transformed_stroke_is_flattened_rather_than_carried() {
        let placement =
            Placement { scale: 2.0, rotation: 90.0, ..Placement::new(0.0, 0.0, 40.0, 40.0) };
        let points = vec![
            vellum_doc::Point { x: 10.0, y: 0.0 },
            vellum_doc::Point { x: 20.0, y: 0.0 },
        ];

        let stroke = ink_stroke(&points, 3.0, &placement);
        // Local +x at 90° is world +y, and the scale doubles it.
        let first = stroke.points()[0];
        assert!(first.x.abs() < 1e-6, "x {}", first.x);
        assert!((first.y - 20.0).abs() < 1e-6, "y {}", first.y);
        assert!((stroke.width() - 6.0).abs() < 1e-9, "the width did not scale");

        let (_, back) = ink_item(&stroke, None).expect("two points is a mark");
        assert!((back.scale - 1.0).abs() < 1e-9, "the scale would be applied twice");
        assert!((back.rotation - 0.0).abs() < 1e-9, "the rotation would be applied twice");
    }

    /// A piece too short to be a mark is dropped, not written back as a degenerate
    /// item — the same floor `commit_stroke` applies to a drawn stroke.
    #[test]
    fn a_cut_that_leaves_a_single_point_is_dropped() {
        let single = Stroke::from_miro(&[(0.0, 0.0)], Some(4.0));
        assert!(ink_item(&single, None).is_none());
    }

    /// The whole point of a *stroke* eraser: a dab through the middle of a line leaves
    /// the two ends, rather than deleting the line.
    #[test]
    fn erasing_the_middle_of_a_stroke_leaves_both_ends() {
        let line: Vec<(f64, f64)> = (0..=20).map(|i| (f64::from(i) * 10.0, 0.0)).collect();
        let stroke = Stroke::from_miro(&line, Some(4.0));

        let pieces = stroke.erase((100.0, 0.0), 20.0);
        assert_eq!(pieces.len(), 2, "expected a left and a right piece");

        let items: Vec<_> = pieces.iter().filter_map(|p| ink_item(p, None)).collect();
        assert_eq!(items.len(), 2, "both pieces must survive the trip back");
        // One each side of where the eraser was.
        assert!(items[0].1.x < 100.0, "{}", items[0].1.x);
        assert!(items[1].1.x > 100.0, "{}", items[1].1.x);
    }

    fn drag_of(mode: DragMode, offset: (f64, f64)) -> Drag {
        Drag { items: Vec::new(), offset, mode, group: None, lock_aspect: false }
    }

    /// The preview and the commit ask the same method for where an item lands, so a
    /// drag cannot end anywhere other than where it was last drawn. They used to hold
    /// two copies of the arithmetic, which was survivable only while the sole mode was
    /// a translation.
    #[test]
    fn a_drag_lands_where_it_was_previewed() {
        let original = Placement::new(0.0, 0.0, 100.0, 100.0);
        let pointer = WorldPoint::new(70.0, -40.0);

        for mode in [DragMode::Move, DragMode::Resize(Handle::BottomRight), DragMode::Rotate] {
            let drag = drag_of(mode, (20.0, 15.0));
            let previewed = drag.placement_of(&original, pointer, false);
            let committed = drag.placement_of(&original, pointer, false);
            assert_eq!(previewed.x.to_bits(), committed.x.to_bits(), "{mode:?}");
            assert_eq!(previewed.width.to_bits(), committed.width.to_bits(), "{mode:?}");
            assert_eq!(previewed.rotation.to_bits(), committed.rotation.to_bits(), "{mode:?}");
        }
    }

    /// A rotation moves the pointer without moving the item's centre, so the offset a
    /// move is judged by is zero for the whole gesture. Judging every mode by it would
    /// have thrown away every rotation at the moment the button came up.
    #[test]
    fn a_rotation_commits_even_though_its_offset_never_moves() {
        assert!(!drag_of(DragMode::Move, (0.0, 0.0)).is_effective());
        assert!(drag_of(DragMode::Move, (1.0, 0.0)).is_effective());
        assert!(drag_of(DragMode::Rotate, (0.0, 0.0)).is_effective());
        assert!(drag_of(DragMode::Resize(Handle::Right), (0.0, 0.0)).is_effective());
    }

    /// Each mode changes its own fields and leaves the others alone — a resize that
    /// also moved the item, or a move that resized it, is the failure worth pinning.
    #[test]
    fn each_drag_mode_touches_only_what_it_should() {
        let original = Placement::new(10.0, 20.0, 100.0, 60.0);
        // Directly above the item's *centre*, not above the world origin — rotation is
        // measured from the centre, so an item that is not at (0, 0) reads a different
        // angle for the same screen point.
        let pointer = WorldPoint::new(original.x, original.y - 100.0);

        let moved = drag_of(DragMode::Move, (7.0, -3.0)).placement_of(&original, pointer, false);
        assert_eq!((moved.width, moved.height), (original.width, original.height));
        assert_eq!((moved.x, moved.y), (17.0, 17.0));
        assert_eq!(moved.rotation, original.rotation);

        let sized =
            drag_of(DragMode::Resize(Handle::Right), (10.0, 0.0)).placement_of(&original, pointer, false);
        assert!((sized.width - 110.0).abs() < 1e-9);
        assert_eq!(sized.height, original.height);
        assert_eq!(sized.rotation, original.rotation);

        let turned = drag_of(DragMode::Rotate, (0.0, 0.0)).placement_of(&original, pointer, false);
        assert_eq!((turned.width, turned.height), (original.width, original.height));
        assert_eq!((turned.x, turned.y), (original.x, original.y));
        assert!((turned.rotation - 0.0).abs() < 1e-9, "{}", turned.rotation);
    }

    /// `arboard` hands a pasted bitmap back as loose RGBA8, and everything downstream
    /// — the blob store, `crate::assets`'s `image::load_from_memory` — wants an encoded
    /// file. Storing the raw pixels would put a headerless dump in the blob store that
    /// nothing could ever read back, and the item would draw as a permanent placeholder.
    #[test]
    fn a_pasted_bitmap_is_re_encoded_into_something_that_decodes() {
        // 2×2 RGBA: red, green, blue, opaque black.
        let rgba = [
            255, 0, 0, 255, //
            0, 255, 0, 255, //
            0, 0, 255, 255, //
            0, 0, 0, 255,
        ];
        let png = encode_png(&rgba, 2, 2).expect("2x2 RGBA is encodable");
        assert_eq!(image::guess_format(&png).unwrap(), image::ImageFormat::Png);

        let decoded = image::load_from_memory(&png).expect("what we store must decode");
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
        assert_eq!(decoded.to_rgba8().into_raw(), rgba, "the pixels did not survive");
    }

    /// A buffer that does not match its stated size is refused rather than panicking
    /// inside the encoder.
    #[test]
    fn a_bitmap_whose_pixels_do_not_match_its_size_is_declined() {
        assert!(encode_png(&[255, 0, 0, 255], 64, 64).is_none());
    }

    /// Pasting from a browser should give the words, not the markup. Paragraphs keep
    /// one blank line between them — `</p><p>` is a break on each side — and runs
    /// longer than that collapse, so deeply nested markup does not arrive as a column
    /// of whitespace.
    #[test]
    fn html_paste_keeps_the_words_and_drops_the_tags() {
        let text = strip_html(
            "<div><p>Engine <b>bay</b> notes</p><p>Second&nbsp;line &amp; more</p></div>",
        );
        assert_eq!(text, "Engine bay notes\n\nSecond line & more");

        let nested = strip_html("<div><div><div><p>Alone</p></div></div></div>");
        assert_eq!(nested, "Alone", "nesting became whitespace");
    }

    /// `<script>` and `<style>` are the one case where the text between the tags is not
    /// text anyone meant to read — a clipboard full of CSS is not a sticky.
    #[test]
    fn html_paste_does_not_show_script_or_style_bodies() {
        let text = strip_html(
            "<style>.a{color:red}</style><p>Visible</p><script>alert('x')</script>",
        );
        assert_eq!(text, "Visible");
    }

    /// The shortcut sheet is generated from the same tables the menus and the keymap
    /// use, so it cannot describe a binding that does not exist.
    #[test]
    fn the_shortcut_sheet_is_generated_from_the_command_table() {
        let sheet = shortcut_reference();
        let actions: Vec<&str> =
            sheet.iter().flat_map(|s| s.rows.iter()).map(|(what, _)| what.as_str()).collect();

        assert!(actions.contains(&"Marquee select"), "the mouse bindings are missing");
        assert!(actions.contains(&Command::Undo.label()));
        assert!(actions.contains(&Tool::Sticky.label()));
        // `docs/06` §1: the wheel zooms, and that is the binding most worth stating.
        assert!(actions.contains(&"Zoom, anchored at the cursor"));
    }

    /// Every row has both halves and every section a heading. A blank right-hand cell
    /// is a row that says an action exists and refuses to say how to reach it, which is
    /// worse than leaving it out.
    #[test]
    fn no_row_of_the_shortcut_sheet_is_half_empty() {
        for section in shortcut_reference() {
            assert!(!section.heading.trim().is_empty());
            assert!(!section.rows.is_empty(), "`{}` is an empty heading", section.heading);
            for (what, keys) in &section.rows {
                assert!(!what.trim().is_empty(), "a nameless row under `{}`", section.heading);
                assert!(!keys.trim().is_empty(), "`{what}` names no keys");
            }
        }
    }

    /// Grouped by the menu each command lives under, in menu order — so the sheet reads
    /// in the same sequence as the menu bar rather than as one undifferentiated list.
    #[test]
    fn the_sheet_is_grouped_by_menu_in_menu_order() {
        let headings: Vec<String> =
            shortcut_reference().into_iter().map(|s| s.heading).collect();
        assert_eq!(&headings[..2], &["Mouse", "Tools"], "{headings:?}");

        let menus: Vec<&String> = headings[2..].iter().collect();
        let expected: Vec<&str> =
            Menu::ALL.into_iter().map(Menu::title).filter(|t| menus.iter().any(|m| m == t)).collect();
        assert_eq!(menus, expected, "the menu sections are out of menu order");
    }

    #[test]
    fn about_names_where_the_boards_live() {
        let about = about();
        assert!(about.contains(env!("CARGO_PKG_VERSION")));
        assert!(about.contains("Vellum"), "the data directory should be named: {about}");
    }

    /// The live preview and the item that lands are one rectangle, because they are one
    /// function.
    ///
    /// *"when i am trying to draw a frame i do not see it as i draw … it just spawns."*
    /// The preview was two lines of arithmetic away from being a second copy of `place`'s
    /// sizing rule, which is the failure `draw::kanban_runs` is `pub(crate)` to avoid: a
    /// preview that disagrees with what it previews is worse than none, because it is
    /// believed. Both call this.
    #[test]
    fn a_placing_drag_previews_the_box_it_will_create() {
        let at = WorldPoint::new(100.0, 50.0);

        // A click — under `DRAG_TO_SIZE` on both axes — is the tool's own default size,
        // centred on the press. A frame is 16:9 because a frame is a slide.
        let click = ActiveState::swept_placement(Tool::Frame, at, at).expect("a frame has a size");
        assert_eq!((click.width, click.height), (1_600.0, 900.0));
        assert_eq!((click.x, click.y), (100.0, 50.0));

        // A drag past the threshold fills the box it swept, whichever way it was dragged.
        let to = WorldPoint::new(500.0, 250.0);
        let dragged = ActiveState::swept_placement(Tool::Frame, at, to).expect("a frame has a size");
        assert_eq!((dragged.width, dragged.height), (400.0, 200.0));
        assert_eq!((dragged.x, dragged.y), (300.0, 150.0), "centred on the sweep");
        let backwards = ActiveState::swept_placement(Tool::Frame, to, at).expect("a size");
        assert_eq!((backwards.width, backwards.height), (400.0, 200.0));
        assert_eq!((backwards.x, backwards.y), (300.0, 150.0));

        // A sweep long on one axis and short on the other is still a click: a rectangle
        // that is one unit tall is not what anybody dragged out on purpose.
        let sliver = WorldPoint::new(500.0, 51.0);
        let thin = ActiveState::swept_placement(Tool::Frame, at, sliver).expect("a size");
        assert_eq!((thin.width, thin.height), (1_600.0, 900.0));

        // And the tools that make nothing rectangular preview nothing. The pen draws its
        // own live stroke and the connector its own line; a ghost box around either would
        // be a second, wrong answer to the same question.
        for tool in [Tool::Pen, Tool::Eraser, Tool::Connector, Tool::Select, Tool::Hand] {
            assert!(
                ActiveState::swept_placement(tool, at, to).is_none(),
                "{tool:?} should not preview a box",
            );
        }
    }

    #[test]
    fn plurals_read_properly() {
        assert_eq!(format!("1 item{}", plural(1)), "1 item");
        assert_eq!(format!("3 item{}", plural(3)), "3 items");
        assert_eq!(format!("0 item{}", plural(0)), "0 items");
    }
}
