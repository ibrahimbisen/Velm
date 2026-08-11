//! The tab strip along the very top of the window.
//!
//! The user's words are the specification:
//!
//! > *"on the top i want there to [be] pages constantly and i want there to be sticky
//! > home page that i can just go back to okay and also everytime i open a new board
//! > it should open on top as well like google tabs does it makes sense but alot
//! > smaller footprint"*
//!
//! Four things follow from that, and each of them is load-bearing:
//!
//! 1. **Constant.** The strip is part of the window, not something that appears once a
//!    second board is open. With nothing open at all there is still a strip, carrying
//!    the home tab.
//! 2. **A sticky home tab.** The board library is tab zero: always first, never
//!    closable, never draggable, never dragged past. It wears the mark rather than a
//!    label, which is what keeps it narrow and what makes it read as *home* rather
//!    than as another board.
//! 3. **Chrome-like opening.** A board opens as a tab at the end of the strip and
//!    becomes the active one, exactly as a browser opens a page. Familiar beats
//!    clever.
//! 4. **A much smaller footprint than a browser's.**
//!    [`TAB_STRIP_HEIGHT`](crate::theme::TAB_STRIP_HEIGHT) is 28 points against
//!    Chrome's ~34, tabs run [`TAB_MIN_WIDTH`](crate::theme::TAB_MIN_WIDTH) to
//!    [`TAB_MAX_WIDTH`](crate::theme::TAB_MAX_WIDTH), and the type is the 11-point
//!    label size. This is a canvas app: every point of chrome is a point of board the
//!    user cannot see. Squared-off tabs, hairline separation, 4px on the top corners
//!    only — not a browser's rounded sheet.
//!
//! # Who owns what
//!
//! **The app owns which boards are open; the strip owns the order they sit in and
//! which one is in front.** That split is the same one the board library already
//! makes with [`Scope`](crate::LibraryScope): membership is data the app supplies,
//! arrangement is something the user does to the interface.
//!
//! So [`Chrome::set_tabs`](crate::Chrome::set_tabs) is a *membership* update — it adds
//! the tabs that are new, drops the ones that are gone, refreshes every title and
//! dirty flag, and leaves the order and the selection alone. Tabs are matched by
//! [`TabKey`], an id the app assigns, because a path cannot identify a board that has
//! never been saved and two of those would otherwise be the same tab.
//!
//! Every gesture still leaves as a [`UiEvent`] — [`UiEvent::SelectTab`],
//! [`UiEvent::CloseTab`], [`UiEvent::ReorderTabs`], [`UiEvent::NewBoardTab`] — and the
//! strip applies it to itself as well, so the interface responds on the frame the user
//! acted rather than on the frame after the app agreed. An app that ignores the events
//! gets a strip that still switches, closes and reorders; it just will not have
//! swapped the document underneath.
//!
//! # Indices
//!
//! Every index in this module and in the events is a **strip index**: `0` is always
//! the home tab and `1..` are the board tabs in the order they are drawn.
//! [`TabStrip::HOME`] names the constant and [`TabStrip::board_index`] converts.
//! [`UiEvent::CloseTab`] and [`UiEvent::ReorderTabs`] never carry `0` — home cannot be
//! closed and nothing can be dropped in front of it.

use crate::event::{EventSink, UiEvent};
use crate::icon::Icon;
use crate::theme::{
    Palette, TAB_HOME_WIDTH, TAB_MAX_WIDTH, TAB_MIN_WIDTH, TAB_STRIP_HEIGHT, space, text,
};
use crate::widgets::icon_button;
use egui::{
    Align, CornerRadius, Id, Layout, Rangef, Rect, Response, Sense, Stroke, Ui, UiBuilder, Vec2,
    pos2, vec2,
};
use std::path::PathBuf;

/// The app's own name for an open board.
///
/// A tab has to be identifiable across frames so the strip can keep the order the user
/// dragged it into while the app goes on replacing the list. A path cannot do that job
/// — a board that has never been saved has no path, and two of them would collide — so
/// the app assigns an id, exactly as it does for
/// [`CustomShapeId`](crate::CustomShapeId) and [`DialogId`](crate::DialogId).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabKey(pub u64);

/// One open board, as the strip shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardTab {
    pub key: TabKey,
    /// Shown on the tab, truncated to fit; the whole string is the tooltip.
    pub title: String,
    /// The file behind it, if it has one. Carried so the app can match a tab to a
    /// library row without a second lookup; the strip only shows it in the tooltip.
    pub path: Option<PathBuf>,
    /// Whether anything is still on its way to disk.
    ///
    /// *"i want to see which tabs i have open"*, and the user asked to be able to
    /// *see* that autosave works — the dot this raises is how that is visible at all.
    /// Its slot is reserved whether or not it is showing, so a tab does not change
    /// width the moment the board is typed into.
    pub dirty: bool,
}

impl BoardTab {
    pub fn new(key: TabKey, title: impl Into<String>) -> Self {
        Self { key, title: title.into(), path: None, dirty: false }
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub const fn dirty(mut self, dirty: bool) -> Self {
        self.dirty = dirty;
        self
    }

    /// What the tab says when it has no title of its own, so a tab is never blank.
    fn label(&self) -> &str {
        if self.title.trim().is_empty() { "Untitled board" } else { &self.title }
    }

    /// The whole title, and the file under it when there is one, so a truncated tab
    /// still answers which board it is.
    fn tooltip(&self) -> String {
        match &self.path {
            Some(path) => format!("{}\n{}", self.label(), path.display()),
            None => self.label().to_owned(),
        }
    }
}

/// A drag in progress.
#[derive(Debug, Clone, Copy)]
struct Drag {
    /// Where the tab was when the drag started, so the event reports the whole move
    /// rather than the last slot it crossed.
    origin: usize,
    /// Where it is now. The strip reorders live under the pointer, which is what makes
    /// the gesture legible without a floating ghost drawn over the board.
    current: usize,
}

/// The strip's own memory between frames.
#[derive(Debug, Default)]
pub struct TabStrip {
    tabs: Vec<BoardTab>,
    /// Strip index of the tab in front. `0` is home.
    active: usize,
    drag: Option<Drag>,
    /// Where the drag wants the tab, and which tab the user asked to close. Both are
    /// applied after the tabs have been laid out rather than during: mutating the list
    /// while iterating it draws a tab twice, skips one, or — for a close — indexes
    /// past the end of a list that has just got shorter.
    pending: Option<usize>,
    pending_close: Option<usize>,
    /// Set when the selection moved, so the next frame scrolls that tab into view. A
    /// frame late on purpose — the rectangle is not known until it has been laid out.
    reveal: bool,
}

impl TabStrip {
    /// The home tab's index. Always zero, always present, never closable.
    pub const HOME: usize = 0;

    pub fn tabs(&self) -> &[BoardTab] {
        &self.tabs
    }

    /// How many tabs the strip is showing, home included.
    pub const fn len(&self) -> usize {
        self.tabs.len() + 1
    }

    /// Never true: the home tab is always there. Present because [`TabStrip::len`] is.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Strip index of the tab in front.
    pub const fn active(&self) -> usize {
        self.active
    }

    /// Whether home is the tab in front — which is to say, whether the window is
    /// showing the board library.
    pub const fn is_home(&self) -> bool {
        self.active == Self::HOME
    }

    /// The board behind a strip index, or `None` for home and for anything past the
    /// end.
    pub fn board_index(&self, strip: usize) -> Option<usize> {
        strip.checked_sub(1).filter(|i| *i < self.tabs.len())
    }

    /// The tab at a strip index, or `None` for home.
    pub fn tab(&self, strip: usize) -> Option<&BoardTab> {
        self.board_index(strip).map(|i| &self.tabs[i])
    }

    /// The tab in front, or `None` when that is home.
    pub fn active_tab(&self) -> Option<&BoardTab> {
        self.tab(self.active)
    }

    fn index_of(&self, key: TabKey) -> Option<usize> {
        self.tabs.iter().position(|t| t.key == key).map(|i| i + 1)
    }

    /// Brings a tab to the front.
    ///
    /// An index past the end is ignored rather than clamped: a stale index is a bug in
    /// the caller, and quietly selecting a neighbour would hide it.
    pub fn set_active(&mut self, strip: usize) {
        if strip < self.len() && strip != self.active {
            self.active = strip;
            self.reveal = true;
        }
    }

    /// Replaces which boards are open, keeping the order the user arranged them in and
    /// the tab that is in front.
    ///
    /// Tabs already on the strip stay where they are and take their new title and
    /// dirty flag; tabs the app has added arrive at the end, in the order it supplied
    /// them, which is what makes a newly opened board appear *on top* the way a
    /// browser opens a page. If the tab in front is gone, the selection falls back
    /// exactly as closing it would.
    ///
    /// **Ignored while a tab is being dragged.** The app replaces this list every
    /// frame, and a replacement mid-gesture would snap the tab out from under the
    /// pointer.
    pub fn set_tabs(&mut self, incoming: Vec<BoardTab>) {
        if self.drag.is_some() {
            return;
        }
        let active_key = self.active_tab().map(|t| t.key);
        let mut kept: Vec<BoardTab> = Vec::with_capacity(incoming.len());
        for existing in &self.tabs {
            if let Some(fresh) = incoming.iter().find(|t| t.key == existing.key) {
                kept.push(fresh.clone());
            }
        }
        for fresh in incoming {
            if !kept.iter().any(|t| t.key == fresh.key) {
                kept.push(fresh);
            }
        }
        self.tabs = kept;
        self.active = match active_key {
            None => Self::HOME,
            Some(key) => self.index_of(key).unwrap_or_else(|| self.active.min(self.tabs.len())),
        };
    }

    /// Opens a board as a tab and brings it to the front, appending it when it is not
    /// already there. Returns its strip index.
    ///
    /// *"everytime i open a new board it should open on top as well like google tabs"*:
    /// a board already on the strip is switched to rather than opened twice, which is
    /// also what stops the same board being loaded into two documents.
    pub fn open(&mut self, tab: BoardTab) -> usize {
        let index = match self.index_of(tab.key) {
            Some(index) => {
                self.tabs[index - 1] = tab;
                index
            }
            None => {
                self.tabs.push(tab);
                self.tabs.len()
            }
        };
        self.set_active(index);
        index
    }

    /// Updates one tab's unsaved-work flag, leaving everything else alone.
    ///
    /// Cheap enough to call every frame, which is what the dot needs in order to
    /// follow an autosave the user asked to be able to *see*.
    pub fn set_dirty(&mut self, key: TabKey, dirty: bool) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.key == key) {
            tab.dirty = dirty;
        }
    }

    /// Closes a tab and reports whether it was there to close. Home never is.
    pub fn close(&mut self, strip: usize) -> bool {
        let Some(index) = self.board_index(strip) else { return false };
        self.active = active_after_close(self.active, strip, self.tabs.len());
        self.tabs.remove(index);
        self.reveal = true;
        true
    }

    /// Moves a tab, clamping the destination into the board tabs so nothing can be
    /// dropped in front of home. Reports whether anything moved.
    pub fn reorder(&mut self, from: usize, to: usize) -> bool {
        let Some(source) = self.board_index(from) else { return false };
        let target = to.clamp(1, self.tabs.len()) - 1;
        if source == target {
            return false;
        }
        let tab = self.tabs.remove(source);
        self.tabs.insert(target, tab);
        // The tab in front is a tab, not a slot: the selection follows the board it
        // names rather than staying on the index that board used to occupy.
        self.active = if self.active == source + 1 {
            target + 1
        } else if self.active == Self::HOME {
            Self::HOME
        } else {
            let mut index = self.active - 1;
            if index > source {
                index -= 1;
            }
            if index >= target {
                index += 1;
            }
            index + 1
        };
        true
    }
}

/// Which tab comes to the front when one is closed.
///
/// The browser rule, because it is the one every user already has: closing the tab in
/// front hands over to its **right-hand** neighbour, and to its left-hand one when
/// there is nothing to the right. Closing the last board tab therefore lands on home,
/// which is the whole reason home is sticky. Closing a tab that is not in front never
/// moves the selection — it only shifts its index.
///
/// `tabs` is the number of board tabs *before* the close.
pub const fn active_after_close(active: usize, closed: usize, tabs: usize) -> usize {
    if closed == TabStrip::HOME || closed > tabs {
        return active;
    }
    if active < closed {
        active
    } else if active > closed {
        active - 1
    } else if closed < tabs {
        // Something is to the right; it slides into this slot.
        closed
    } else {
        // It was the last one — go left, which is home when it was also the only one.
        closed - 1
    }
}

/// Edge length of a tab's close button.
const CLOSE_BUTTON: f32 = space::of(4);

/// The slot the unsaved-work dot sits in, reserved whether or not it is showing.
const DOT_SLOT: f32 = space::of(2);

/// The column the `+` at the end of the strip takes.
const NEW_TAB_SLOT: f32 = space::of(6);

/// The mark's size in the home tab.
///
/// Four points under [`crate::mark::MINIMUM_SIZE`], and the arithmetic is why that is
/// not a violation: `mark`'s cap rule holds the square caps back so the negative space
/// between two brackets cannot drop under its floor, and from 24 points to about 35
/// that clamp is what binds — so the mark at 24 has *exactly* the same open gap, in
/// points, as the mark at its documented minimum. The test below asserts that rather
/// than trusting this paragraph. Twenty-eight would leave no room at all in a
/// 28-point strip, and the strip's height is the supplied constraint.
const HOME_MARK: f32 = space::of(6);

/// The width board tabs take, given how much room the strip has for them.
///
/// Shrink toward the floor first and scroll only after that: a strip that scrolls
/// while its tabs are still 160 wide makes the user work for something that would
/// have fitted.
pub fn tab_width(count: usize, available: f32) -> f32 {
    if count == 0 {
        return TAB_MAX_WIDTH;
    }
    (available / count as f32).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// Draws the strip and returns the rectangle it took.
pub(crate) fn show(
    ui: &mut Ui,
    palette: Palette,
    strip: &mut TabStrip,
    events: &mut EventSink,
) -> Rect {
    let frame = egui::Frame::new()
        .fill(palette.backdrop)
        .inner_margin(egui::Margin::ZERO)
        .stroke(Stroke::NONE);

    // Where the tab in front sits, so the rule under the strip can be cut there.
    let mut active_span: Option<Rangef> = None;
    let panel = egui::Panel::top("vellum-tab-strip")
        .frame(frame)
        .exact_size(TAB_STRIP_HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            let full = ui.available_rect_before_wrap();
            let home_rect = Rect::from_min_size(full.min, vec2(TAB_HOME_WIDTH, full.height()));
            let plus_rect = Rect::from_min_size(
                pos2(full.right() - NEW_TAB_SLOT, full.top()),
                vec2(NEW_TAB_SLOT, full.height()),
            );
            let track = Rect::from_min_max(home_rect.right_top(), plus_rect.left_bottom());

            home_tab(ui, palette, strip, home_rect, &mut active_span, events);
            board_tabs(ui, palette, strip, track, &mut active_span, events);
            new_tab_button(ui, palette, plus_rect, events);
        });

    // The rule under the strip, drawn by hand for the reason `crate::menu` gives: one
    // point of the border colour, whatever the platform's scale factor rounds a panel
    // edge to. The tab in front cuts it, so the tab and what is under it read as one
    // surface rather than as two.
    let rect = panel.response.rect;
    let painter = ui
        .ctx()
        .layer_painter(egui::LayerId::new(egui::Order::Middle, Id::new("vellum-tab-rule")));
    painter.hline(rect.x_range(), rect.max.y, palette.hairline_stroke());
    if let Some(span) = active_span {
        painter.hline(span, rect.max.y, Stroke::new(palette.hairline_width(), palette.surface));
    }
    rect
}

/// The sticky home tab: the mark, and nothing else.
fn home_tab(
    ui: &mut Ui,
    palette: Palette,
    strip: &mut TabStrip,
    rect: Rect,
    active_span: &mut Option<Rangef>,
    events: &mut EventSink,
) {
    let response = ui.interact(rect, Id::new("vellum-tab-home"), Sense::click());
    let active = strip.is_home();
    if ui.is_rect_visible(rect) {
        tab_background(ui, palette, rect, active, response.hovered());
        let glyph = Rect::from_center_size(rect.center(), Vec2::splat(HOME_MARK));
        // The full mark when it is the tab in front, the single-colour cut when it is
        // not: `assets/logo/README.md` scopes the mono cut to exactly this — a menu
        // row, a disabled state, any ground the red does not sit on cleanly.
        if active {
            crate::mark::paint(&ui.painter().clone(), glyph, palette);
        } else {
            crate::mark::paint_mono(&ui.painter().clone(), glyph, palette.muted);
        }
        *active_span = active.then(|| rect.x_range());
    }
    if response.on_hover_text("Board library").clicked() {
        select(strip, TabStrip::HOME, events);
    }
}

/// The board tabs, scrolling under the pointer once they have shrunk as far as they
/// go.
fn board_tabs(
    ui: &mut Ui,
    palette: Palette,
    strip: &mut TabStrip,
    track: Rect,
    active_span: &mut Option<Rangef>,
    events: &mut EventSink,
) {
    if strip.tabs.is_empty() || track.width() <= 0.0 {
        return;
    }
    let width = tab_width(strip.tabs.len(), track.width());
    let mut child = ui.new_child(UiBuilder::new().id_salt("vellum-tabs").max_rect(track));
    child.spacing_mut().item_spacing = Vec2::ZERO;

    egui::ScrollArea::horizontal()
        .id_salt("vellum-tab-scroll")
        .auto_shrink([false, false])
        // No scrollbar: there is no room for one in a 28-point strip, and taking the
        // height for it would cost more board than the bar is worth. The wheel
        // scrolls the strip, and the tab in front is always scrolled into view.
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut child, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let mut front: Option<Rect> = None;
                // The strip's own height, not the row's: inside a horizontal layout
                // `available_height` is whatever egui's `interact_size` left, which
                // drew tabs four points short of the rule under them.
                let height = track.height();
                for index in 0..strip.tabs.len() {
                    let rect = board_tab(ui, palette, strip, index, width, height, events);
                    if index + 1 == strip.active {
                        front = Some(rect);
                    }
                }
                if let Some(rect) = front {
                    *active_span = Some(rect.x_range());
                    if std::mem::take(&mut strip.reveal) {
                        ui.scroll_to_rect(rect, None);
                    }
                }
            });
        });

    // Applied after the tabs are laid out rather than during: mutating the list while
    // iterating it draws a tab twice, skips one, or indexes past the end.
    if let Some(index) = strip.pending_close.take() {
        strip.pending = None;
        if strip.close(index) {
            events.push(UiEvent::CloseTab(index));
        }
    } else if let Some(target) = strip.pending.take()
        && let Some(mut drag) = strip.drag
        && strip.reorder(drag.current, target)
    {
        drag.current = target;
        strip.drag = Some(drag);
    }
}

/// One board tab. Returns the rectangle it took.
fn board_tab(
    ui: &mut Ui,
    palette: Palette,
    strip: &mut TabStrip,
    index: usize,
    width: f32,
    height: f32,
    events: &mut EventSink,
) -> Rect {
    let strip_index = index + 1;
    let active = strip.active == strip_index;
    let (key, label, dirty, hint) = {
        let tab = &strip.tabs[index];
        (tab.key, tab.label().to_owned(), tab.dirty, tab.tooltip())
    };

    // The tab's id follows its **board**, not its slot. egui tracks a drag by the id
    // that was pressed, and the whole point of this gesture is that the tab changes
    // slot half way through it — with a positional id the drag would be left behind on
    // the first swap and the tab would never reach the third position.
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let response = ui.interact(rect, Id::new(("vellum-tab", key)), Sense::click_and_drag());

    if ui.is_rect_visible(rect) {
        tab_background(ui, palette, rect, active, response.hovered());
        let inner = rect.shrink2(vec2(space::of(2), 0.0));

        // Left: the unsaved-work dot, in a slot reserved whether or not it is showing,
        // so the label never shifts under the user's eye as the board is saved.
        if dirty {
            ui.painter().circle_filled(
                pos2(inner.left() + DOT_SLOT / 2.0, inner.center().y),
                space::UNIT / 2.0,
                palette.accent,
            );
        }

        // Right: the close button's slot, reserved for the same reason.
        let text_rect = Rect::from_min_max(
            pos2(inner.left() + DOT_SLOT + space::UNIT, inner.top()),
            pos2(inner.right() - CLOSE_BUTTON, inner.bottom()),
        );
        if text_rect.width() > 0.0 {
            let color = if active { palette.text } else { palette.muted };
            let galley = truncated(ui, &label, text_rect.width(), color);
            let at = pos2(text_rect.left(), text_rect.center().y - galley.size().y / 2.0);
            ui.painter().galley(at, galley, color);
        }

        // A hairline between two inactive tabs. None beside the tab in front: its own
        // edge already separates it, and two lines a point apart read as a seam.
        if !active && strip.active != strip_index + 1 {
            ui.painter().vline(
                rect.right(),
                rect.y_range().shrink(space::UNIT),
                palette.hairline_stroke(),
            );
        }
    }

    // Shown on hover and on the tab in front, as a browser does: a column of close
    // buttons down an idle strip is noise, and one that is never there is a hunt.
    let closing = (response.hovered() || active) && {
        let at = Rect::from_center_size(
            pos2(rect.right() - space::of(2) - CLOSE_BUTTON / 2.0, rect.center().y),
            Vec2::splat(CLOSE_BUTTON),
        );
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("vellum-tab-close", key))
                .max_rect(at)
                .layout(Layout::left_to_right(Align::Center)),
        );
        icon_button(&mut child, palette, Icon::Close, CLOSE_BUTTON, false)
            .on_hover_text("Close tab")
            .clicked()
    };

    // Middle-click closes wherever on the tab it lands — the browser binding, and the
    // one that does not need the button to be visible first.
    if closing || response.middle_clicked() {
        strip.pending_close = Some(strip_index);
        return rect;
    }
    if response.clicked() {
        select(strip, strip_index, events);
    }
    drag(ui, strip, strip_index, &response, rect, width);
    if response.drag_stopped() {
        finish_drag(strip, events);
    }
    // Last, because the tooltip consumes the response it is attached to.
    let _ = response.on_hover_text(hint);
    rect
}

/// Drag to reorder, with the strip rearranging live under the pointer.
///
/// Rearranging as the pointer crosses each boundary, rather than drawing a ghost and
/// committing at the end, is both less code and more legible: what the user sees
/// during the drag is what they will have when they let go.
fn drag(ui: &Ui, strip: &mut TabStrip, strip_index: usize, response: &Response, rect: Rect, width: f32) {
    if response.drag_started() {
        strip.drag = Some(Drag { origin: strip_index, current: strip_index });
    }
    let Some(state) = strip.drag else { return };
    if state.current != strip_index || !response.dragged() {
        return;
    }
    let Some(pointer) = ui.ctx().pointer_interact_pos() else { return };
    let slots = ((pointer.x - rect.center().x) / width) as i32;
    if slots != 0 {
        // Clamped into the board tabs: nothing goes in front of home.
        let last = strip.tabs.len() as i32;
        let target = (strip_index as i32 + slots).clamp(1, last) as usize;
        if target != strip_index {
            strip.pending = Some(target);
        }
    }
}

fn finish_drag(strip: &mut TabStrip, events: &mut EventSink) {
    let Some(state) = strip.drag.take() else { return };
    strip.pending = None;
    if state.current != state.origin {
        events.push(UiEvent::ReorderTabs { from: state.origin, to: state.current });
    }
}

/// The `+` at the end of the strip. A new tab shows the library, which is what the
/// user goes there for — pick a board, or start one.
fn new_tab_button(ui: &mut Ui, palette: Palette, rect: Rect, events: &mut EventSink) {
    let size = space::of(5);
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("vellum-tab-new")
            .max_rect(Rect::from_center_size(rect.center(), Vec2::splat(size)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    if icon_button(&mut child, palette, Icon::Plus, size, false)
        .on_hover_text("New tab")
        .clicked()
    {
        events.push(UiEvent::NewBoardTab);
    }
}

fn select(strip: &mut TabStrip, index: usize, events: &mut EventSink) {
    if strip.active != index {
        strip.set_active(index);
        events.push(UiEvent::SelectTab(index));
    }
}

/// The tab's own surface.
///
/// The tab in front is a step lighter with a 2-point accent along its top edge, and
/// its corners are rounded on the top only: a tab is attached to what is under it, and
/// a rounded bottom would float it. Inactive tabs sit on the recessed ground with no
/// fill at all until the pointer reaches them.
fn tab_background(ui: &Ui, palette: Palette, rect: Rect, active: bool, hovered: bool) {
    // **Square.** The top corners were rounded, and the accent bar along the active
    // tab's top edge could not follow them — egui clamps a corner radius to half the
    // height, so a 6-point radius on a 2-point bar became 1 and the bar overhung the
    // tab's shoulders. That mismatch is what *"the tabs are slightly misaligned"* was
    // looking at. Clipping the accent to the tab's own curve fixed the overhang and the
    // user's answer to seeing it was *"you dont need to do that"* — so the rounding
    // goes instead, which removes the mismatch rather than reconciling it.
    let corner = CornerRadius::ZERO;
    if active {
        ui.painter().rect_filled(rect, corner, palette.surface);
        // **The accent is the whole tab's shape, clipped to its top two points** — not a
        // 2-point rect of its own. Passing `corner` to a rect that short does nothing:
        // egui clamps a corner radius to half the height, so a 6-point radius on a
        // 2-point bar becomes 1, and the bar drew with square ends sitting on the tab's
        // rounded shoulders. It overhung them on both sides by a couple of pixels, which
        // is what *"the tabs are slightly misaligned"* was looking at — measured off a
        // screenshot: at the tab's top row the teal began abruptly at full opacity where
        // the body two rows below was still antialiasing its curve.
        //
        // Clipping a full-height rounded rect instead means the accent follows exactly
        // the curve the body has, because it *is* the same curve.
        let edge = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 2.0));
        ui.painter().with_clip_rect(edge).rect_filled(rect, corner, palette.accent);
    } else if hovered {
        ui.painter().rect_filled(rect, corner, palette.hover);
    }
}

/// The tab's title, cut to the room it has with an ellipsis rather than clipped
/// mid-letter.
fn truncated(
    ui: &Ui,
    label: &str,
    width: f32,
    color: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(
        label.to_owned(),
        egui::FontId::proportional(text::LABEL),
        color,
    );
    job.wrap = egui::text::TextWrapping {
        max_width: width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Context;

    fn strip_of(n: u64) -> TabStrip {
        let mut strip = TabStrip::default();
        strip.set_tabs((0..n).map(|i| BoardTab::new(TabKey(i), format!("Board {i}"))).collect());
        strip
    }

    fn keys(strip: &TabStrip) -> Vec<u64> {
        strip.tabs().iter().map(|t| t.key.0).collect()
    }

    #[test]
    fn a_fresh_strip_is_the_home_tab_and_nothing_else() {
        let strip = TabStrip::default();
        assert_eq!(strip.len(), 1);
        assert!(strip.is_home());
        assert_eq!(strip.active(), TabStrip::HOME);
        assert_eq!(strip.active_tab(), None);
        assert_eq!(strip.board_index(TabStrip::HOME), None);
        assert!(!strip.is_empty(), "home is always there");
    }

    /// *"everytime i open a new board it should open on top as well like google
    /// tabs"*: a board the app has added arrives at the end.
    #[test]
    fn a_new_board_arrives_at_the_end_and_the_others_keep_their_places() {
        let mut strip = strip_of(2);
        assert_eq!(keys(&strip), vec![0, 1]);
        strip.set_active(1);

        strip.set_tabs(vec![
            BoardTab::new(TabKey(7), "Late"),
            BoardTab::new(TabKey(0), "Board 0"),
            BoardTab::new(TabKey(1), "Board 1"),
        ]);
        assert_eq!(keys(&strip), vec![0, 1, 7], "the app's order does not re-sort the strip");
        assert_eq!(strip.active(), 1, "the tab in front is still the same board");
    }

    /// The strip owns the order; the app owns the membership. A replacement that is a
    /// permutation of what is already there must not undo a drag.
    #[test]
    fn replacing_the_tabs_keeps_the_order_the_user_dragged_them_into() {
        let mut strip = strip_of(3);
        assert!(strip.reorder(3, 1));
        assert_eq!(keys(&strip), vec![2, 0, 1]);

        strip.set_tabs((0..3).map(|i| BoardTab::new(TabKey(i), format!("Board {i}"))).collect());
        assert_eq!(keys(&strip), vec![2, 0, 1]);
    }

    /// Opening the same board twice switches to the tab it already has rather than
    /// making a second one — which is also what stops one board being loaded into two
    /// documents at once.
    #[test]
    fn opening_a_board_appends_it_and_opening_it_again_only_switches() {
        let mut strip = TabStrip::default();
        assert_eq!(strip.open(BoardTab::new(TabKey(1), "Site plan")), 1);
        assert_eq!(strip.active(), 1);
        assert_eq!(strip.open(BoardTab::new(TabKey(2), "Roadmap")), 2);
        assert_eq!(strip.active(), 2, "a new board opens on top");

        assert_eq!(strip.open(BoardTab::new(TabKey(1), "Site plan")), 1);
        assert_eq!(strip.len(), 3, "and the second open did not add a tab");
        assert_eq!(strip.active(), 1);

        strip.set_dirty(TabKey(1), true);
        assert!(strip.tabs()[0].dirty);
        strip.set_dirty(TabKey(9), true);
        assert_eq!(strip.tabs().iter().filter(|t| t.dirty).count(), 1, "a key that is not there");
    }

    #[test]
    fn titles_and_dirty_flags_are_refreshed_in_place() {
        let mut strip = strip_of(1);
        assert!(!strip.tabs()[0].dirty);
        strip.set_tabs(vec![BoardTab::new(TabKey(0), "Renamed").dirty(true)]);
        assert_eq!(strip.tabs()[0].title, "Renamed");
        assert!(strip.tabs()[0].dirty);
    }

    /// The rule a browser taught everyone: the tab to the right takes over, and the
    /// one to the left when there is nothing to the right.
    #[test]
    fn closing_the_tab_in_front_hands_over_to_a_sensible_neighbour() {
        let mut strip = strip_of(3);
        strip.set_active(2);
        assert!(strip.close(2));
        assert_eq!(keys(&strip), vec![0, 2]);
        assert_eq!(strip.active(), 2, "the right-hand neighbour came forward");

        assert!(strip.close(2));
        assert_eq!(strip.active(), 1, "nothing to the right, so the left-hand one");

        assert!(strip.close(1));
        assert!(strip.is_home(), "the last board tab hands over to home");
    }

    #[test]
    fn closing_a_tab_that_is_not_in_front_only_shifts_the_index() {
        let mut strip = strip_of(3);
        strip.set_active(3);
        assert!(strip.close(1));
        assert_eq!(strip.active(), 2, "the same board, one slot to the left");

        let mut strip = strip_of(3);
        strip.set_active(1);
        assert!(strip.close(3));
        assert_eq!(strip.active(), 1);
    }

    /// The whole rule in one place, over every arrangement it can be asked about.
    #[test]
    fn the_close_rule_never_selects_a_tab_that_is_not_there() {
        for tabs in 1..=4_usize {
            for closed in 1..=tabs {
                for active in 0..=tabs {
                    let next = active_after_close(active, closed, tabs);
                    assert!(
                        next < tabs,
                        "closing {closed} of {tabs} with {active} in front chose {next}"
                    );
                }
            }
        }
    }

    /// *"a sticky home page that i can just go back to"*: not a tab that happens to be
    /// first, but one that cannot be got rid of.
    #[test]
    fn the_home_tab_can_never_be_closed_or_dragged_past() {
        let mut strip = strip_of(2);
        assert!(!strip.close(TabStrip::HOME), "home is not closable");
        assert_eq!(strip.len(), 3);
        assert!(!strip.close(9), "and neither is a tab that is not there");

        // Dropped in front of home, a tab lands in the first board slot instead.
        assert!(strip.reorder(2, TabStrip::HOME));
        assert_eq!(keys(&strip), vec![1, 0]);
    }

    #[test]
    fn reordering_carries_the_tab_in_front_with_it() {
        let mut strip = strip_of(4);
        strip.set_active(4);
        assert!(strip.reorder(4, 1));
        assert_eq!(keys(&strip), vec![3, 0, 1, 2]);
        assert_eq!(strip.active(), 1, "the dragged tab was the one in front");

        let mut strip = strip_of(4);
        strip.set_active(1);
        assert!(strip.reorder(4, 1));
        assert_eq!(strip.active(), 2, "a tab that did not move still names its own board");

        let mut strip = strip_of(4);
        strip.set_active(2);
        assert!(strip.reorder(1, 3));
        assert_eq!(keys(&strip), vec![1, 2, 0, 3]);
        assert_eq!(strip.active(), 1);

        let mut strip = strip_of(3);
        strip.set_active(TabStrip::HOME);
        assert!(strip.reorder(1, 3));
        assert!(strip.is_home(), "home is not carried anywhere");
    }

    #[test]
    fn a_reorder_that_moves_nothing_reports_nothing() {
        let mut strip = strip_of(3);
        assert!(!strip.reorder(2, 2));
        assert!(!strip.reorder(TabStrip::HOME, 2), "home does not move");
        assert!(!strip.reorder(9, 1));
        assert_eq!(keys(&strip), vec![0, 1, 2]);
        assert!(!TabStrip::default().reorder(1, 1), "and nothing moves on an empty strip");
    }

    /// If the app closes a board behind the strip's back, the selection has to land
    /// somewhere that exists.
    #[test]
    fn losing_the_tab_in_front_falls_back_the_way_closing_it_would() {
        let mut strip = strip_of(3);
        strip.set_active(2);
        strip.set_tabs(vec![
            BoardTab::new(TabKey(0), "Board 0"),
            BoardTab::new(TabKey(2), "Board 2"),
        ]);
        assert_eq!(strip.active(), 2);

        let mut strip = strip_of(2);
        strip.set_active(2);
        strip.set_tabs(vec![]);
        assert!(strip.is_home());
    }

    /// Shrink to the floor first, then scroll. A strip that scrolls while its tabs are
    /// still full width makes the user work for something that would have fitted.
    #[test]
    fn tabs_shrink_to_the_floor_before_the_strip_scrolls() {
        assert_eq!(tab_width(2, 1000.0), TAB_MAX_WIDTH, "room to spare");
        assert_eq!(tab_width(8, 1000.0), 125.0, "sharing what there is");
        assert_eq!(tab_width(40, 1000.0), TAB_MIN_WIDTH, "at the floor, so it scrolls");
        assert_eq!(tab_width(0, 1000.0), TAB_MAX_WIDTH);
        const { assert!(TAB_MIN_WIDTH < TAB_MAX_WIDTH) };
    }

    /// The footprint the user asked for, held to by a test rather than by a comment.
    #[test]
    fn the_strip_is_smaller_than_a_browsers() {
        assert!((26.0..=28.0).contains(&TAB_STRIP_HEIGHT), "{TAB_STRIP_HEIGHT}");
        assert_eq!(TAB_STRIP_HEIGHT % space::UNIT, 0.0, "on the grid");
        assert_eq!(TAB_HOME_WIDTH % space::UNIT, 0.0);
        const { assert!(TAB_HOME_WIDTH < TAB_MIN_WIDTH, "the home tab stays narrow") };
        assert_eq!(text::LABEL, 11.0, "the strip is set at the label size");
        const { assert!(TAB_STRIP_HEIGHT < crate::theme::MENU_BAR_HEIGHT) };
    }

    /// The mark is four points under its documented minimum, and this is the
    /// measurement that says it still reads: `mark`'s cap rule clamps the square caps
    /// to protect the negative space, and from 24 points to about 35 that clamp is
    /// what binds — so the gap is identical at both sizes.
    #[test]
    fn the_home_tabs_mark_has_as_much_negative_space_as_the_full_size_one() {
        const { assert!(HOME_MARK < crate::mark::MINIMUM_SIZE) };
        assert!(
            crate::mark::open_gap(HOME_MARK) >= crate::mark::open_gap(crate::mark::MINIMUM_SIZE),
            "{} against {}",
            crate::mark::open_gap(HOME_MARK),
            crate::mark::open_gap(crate::mark::MINIMUM_SIZE)
        );
        // …and it fits the strip with room on every side.
        const { assert!(HOME_MARK < TAB_STRIP_HEIGHT) };
        const { assert!(HOME_MARK < TAB_HOME_WIDTH) };
    }

    fn run(strip: &mut TabStrip) -> Vec<UiEvent> {
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        let mut events = EventSink::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = show(ui, Palette::LIGHT, strip, &mut events);
        });
        events.take()
    }

    /// Every arrangement has to draw, and an untouched frame has to emit nothing.
    #[test]
    fn the_strip_draws_empty_full_and_overflowing_without_emitting_anything() {
        for count in [0, 1, 3, 40] {
            let mut strip = strip_of(count);
            assert!(run(&mut strip).is_empty(), "{count} tabs emitted something");
        }

        let mut strip = strip_of(3);
        strip.set_active(2);
        strip.tabs[1].dirty = true;
        assert!(run(&mut strip).is_empty(), "a dirty tab in front emitted something");

        // A tab with no title of its own still says something.
        let mut strip = TabStrip::default();
        strip.set_tabs(vec![BoardTab::new(TabKey(1), "  ").with_path("/boards/site-plan.vellum")]);
        assert!(run(&mut strip).is_empty());
        assert_eq!(strip.tabs()[0].label(), "Untitled board");
        assert!(strip.tabs()[0].tooltip().contains("site-plan.vellum"));
    }
}
