//! End-to-end tests: real pointer clicks in, [`UiEvent`]s out.
//!
//! The unit tests inside the crate check that each panel *draws* and that the state
//! machines behind it are right. These check the part neither can: that a click at a
//! screen position actually reaches the widget under it and produces the event the
//! app is waiting for. Nothing here reaches into the crate — it is the same API
//! `vellum-app` will use.
//!
//! Widgets are located through [`egui::Context::interactive_rects_last_pass`] rather
//! than by reconstructing the layout arithmetic, because a test that hard-codes
//! "the sticky tool is 132 points down" fails on every spacing change without ever
//! being wrong about behaviour.

use egui::{Context, Event, Order, PointerButton, Pos2, RawInput, Rect, Vec2, vec2};
use std::path::PathBuf;
use std::time::SystemTime;
use vellum_ui::{
    BoardCard, BoardState, BoardTab, Chrome, ChromeOutput, ChromeState, Command, CustomShape,
    CustomShapeId, LibraryEvent, Screen, Space, TabKey, Tool, UiEvent,
};

const SCREEN: Vec2 = vec2(1440.0, 900.0);

/// The band along the top of every screen that the tab strip takes.
const STRIP: f32 = vellum_ui::theme::TAB_STRIP_HEIGHT;

/// The top of the menu bar, which now starts under the strip.
/// Translucent chrome · Align objects · Fetch link previews · Transparency ▸ ·
/// Accent colour ▸ · Keyboard shortcuts · Documentation · About, plus the Agent Canvas's
/// four — Agent output ▸ · Providers ▸ · Browser nodes · Worktree isolation — the last
/// group in the ☰ menu.
///
/// The count is pinned rather than derived because it is what makes the *index* arithmetic
/// below trustworthy: several tests reach a specific row by position, and a row silently
/// appearing above one of them would move every click after it without failing anything.
const PREFERENCES_ROWS: usize = 12;

const MENU_TOP: f32 = STRIP;

/// The bottom of the menu bar.
const MENU_BOTTOM: f32 = STRIP + vellum_ui::theme::MENU_BAR_HEIGHT;

fn input() -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
        ..RawInput::default()
    }
}

/// A press and release at `pos`, which is what egui needs to see to report a click.
fn click_at(pos: Pos2) -> RawInput {
    let mut raw = input();
    raw.events.push(Event::PointerMoved(pos));
    raw.events.push(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    raw.events.push(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    raw
}

/// The pointer moved somewhere and left there. egui keeps the position across frames,
/// so a following idle frame still counts as hovering.
fn hover_at(pos: Pos2) -> RawInput {
    let mut raw = input();
    raw.events.push(Event::PointerMoved(pos));
    raw
}

/// A middle press and release — the browser binding for closing a tab.
fn middle_click_at(pos: Pos2) -> RawInput {
    let mut raw = input();
    raw.events.push(Event::PointerMoved(pos));
    for pressed in [true, false] {
        raw.events.push(Event::PointerButton {
            pos,
            button: PointerButton::Middle,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
    }
    raw
}

/// The press that starts a drag. egui needs the press and the move on separate
/// frames, so a drag is three inputs rather than one.
fn press_at(pos: Pos2) -> RawInput {
    let mut raw = input();
    raw.events.push(Event::PointerMoved(pos));
    raw.events.push(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    raw
}

/// The move in the middle of a drag, with the button still down.
fn drag_to(pos: Pos2) -> RawInput {
    let mut raw = input();
    raw.events.push(Event::PointerMoved(pos));
    raw
}

/// The release that ends one.
fn release_at(pos: Pos2) -> RawInput {
    let mut raw = input();
    raw.events.push(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    raw
}

/// A whole drag: press, move, release. Returns what the release frame emitted, which
/// is where a completed gesture reports itself.
fn drag(
    ctx: &Context,
    chrome: &mut Chrome,
    state: &ChromeState<'_>,
    from: Pos2,
    to: Pos2,
) -> ChromeOutput {
    let _ = frame(ctx, chrome, state, press_at(from));
    let _ = frame(ctx, chrome, state, drag_to(to));
    frame(ctx, chrome, state, release_at(to))
}

/// A secondary press and release, which is what opens a context menu.
fn right_click_at(pos: Pos2) -> RawInput {
    let mut raw = input();
    raw.events.push(Event::PointerMoved(pos));
    for pressed in [true, false] {
        raw.events.push(Event::PointerButton {
            pos,
            button: PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
    }
    raw
}

fn frame(
    ctx: &Context,
    chrome: &mut Chrome,
    state: &ChromeState<'_>,
    raw: RawInput,
) -> ChromeOutput {
    let mut output = None;
    let _ = ctx.run_ui(raw, |ui| {
        output = Some(chrome.show(ui, state));
    });
    output.expect("the chrome ran a pass")
}

/// Two idle passes, which is what egui needs before every widget has a rectangle:
/// the first sizes the panels, the second lays out what is inside them.
fn settle(ctx: &Context, chrome: &mut Chrome, state: &ChromeState<'_>) -> ChromeOutput {
    let _ = frame(ctx, chrome, state, input());
    frame(ctx, chrome, state, input())
}

/// Every interactive rectangle from the previous pass matching `predicate`, top to
/// bottom then left to right — the order a reader would call "first".
fn all_widgets(ctx: &Context, predicate: impl Fn(Rect) -> bool) -> Vec<Rect> {
    let mut found: Vec<Rect> = ctx
        .interactive_rects_last_pass()
        .into_iter()
        .filter(|rect| rect.is_positive() && predicate(*rect))
        .collect();
    found.sort_by(|a, b| {
        a.top().total_cmp(&b.top()).then_with(|| a.left().total_cmp(&b.left()))
    });
    found
}

/// The same, below the tab strip.
///
/// The strip spans the top of every screen and its buttons are the same sizes the
/// panels use, so without this every search for "the 20-point squares in the sidebar"
/// would also find the `+` at the end of the strip.
fn widgets(ctx: &Context, predicate: impl Fn(Rect) -> bool) -> Vec<Rect> {
    all_widgets(ctx, |rect| rect.top() >= STRIP && predicate(rect))
}

/// Everything inside the tab strip.
fn strip_widgets(ctx: &Context) -> Vec<Rect> {
    all_widgets(ctx, |rect| rect.bottom() <= STRIP + 1.0)
}

/// The board tabs: the only things in the strip wide enough to hold a title.
fn board_tabs(ctx: &Context) -> Vec<Rect> {
    strip_widgets(ctx)
        .into_iter()
        .filter(|rect| rect.width() >= vellum_ui::theme::TAB_MIN_WIDTH)
        .collect()
}

/// The interactive controls inside the top-left pill: ☰, the board name, ⋮.
///
/// The pill floats over the canvas now rather than being a docked full-width bar, so
/// this is a band *and* a left-edge test — the status cluster and the tool palette are
/// also floating, and only this one starts at the window's left margin.
fn pill_controls(ctx: &Context) -> Vec<Rect> {
    // Width-bounded so the pill's own container rectangle does not count as a control:
    // it is also interactive, it is also in the band, and being widest it would sort
    // first — so `first()` would click the middle of the pill, which is the board name.
    let mut found: Vec<Rect> = widgets(ctx, |rect| {
        rect.top() >= MENU_TOP
            && rect.bottom() <= MENU_BOTTOM + 8.0
            && rect.left() < 400.0
            && rect.width() < 40.0
    });
    found.sort_by(|a, b| a.left().total_cmp(&b.left()));
    found
}

/// Opens the ☰ menu and returns its **four group rows** — Board, Edit, View,
/// Preferences — top to bottom.
///
/// The four used to be top-level buttons on a full-width bar, which is a menu *bar*;
/// Miro has none and `docs/04-ui-reference.md` §4 files the whole tree under one opener.
/// They were briefly one flat list of all 58 commands, which overflowed a 900pt window by
/// most of its own height and hid Preferences below a fold nothing announced. They nest
/// again: use [`group_rows`] to reach the commands inside one.
fn main_menu(ctx: &Context, chrome: &mut Chrome, state: &ChromeState<'_>) -> Vec<Rect> {
    // **Idempotent, because the ☰ is a toggle.** A caller that opens the menu and then
    // reaches for a group would otherwise click the ☰ a second time and shut it, which is
    // the same trap `picking_an_accent_in_preferences` fell into by calling this twice.
    // `Menu::ALL` is four, so four visible rows means it is already open.
    if menu_rows(ctx).len() < MENU_GROUPS {
        let hamburger = *pill_controls(ctx).first().expect("the pill draws a ☰");
        let _ = frame(ctx, chrome, state, click_at(hamburger.center()));
        settle(ctx, chrome, state);
    }
    let mut rows = menu_rows(ctx);
    rows.sort_by(|a, b| a.top().total_cmp(&b.top()));
    rows
}

/// Board · Edit · View · Preferences — the length of `Menu::ALL`, which is not public.
const MENU_GROUPS: usize = 4;

/// Opens one of the four groups in the ☰ menu and returns **that group's** rows.
///
/// `group` indexes `Menu::ALL`: 0 Board, 1 Edit, 2 View, 3 Preferences.
///
/// Filtered by `left()` against the parent row's right edge, because a submenu opens
/// *beside* the popup that spawned it and both are on screen at once — which is what a
/// submenu looks like, and which `menu_rows` reports in full.
fn group_rows(
    ctx: &Context,
    chrome: &mut Chrome,
    state: &ChromeState<'_>,
    group: usize,
) -> Vec<Rect> {
    let groups = main_menu(ctx, chrome, state);
    let parent = *groups.get(group).unwrap_or_else(|| {
        panic!("the ☰ menu drew {} group rows, wanted at least {}", groups.len(), group + 1)
    });
    let _ = frame(ctx, chrome, state, click_at(parent.center()));
    settle(ctx, chrome, state);
    let mut rows: Vec<Rect> =
        menu_rows(ctx).into_iter().filter(|r| r.left() > parent.right() - 4.0).collect();
    rows.sort_by(|a, b| a.top().total_cmp(&b.top()));
    rows
}

fn board_state<'a>(selection: &'a [vellum_ui::SelectionItem]) -> ChromeState<'a> {
    ChromeState {
        screen: Screen::Board,
        board: BoardState { title: "Site plan", dirty: true, ..BoardState::default() },
        selection,
        ..ChromeState::default()
    }
}

fn approx(value: f32, target: f32) -> bool {
    (value - target).abs() < 0.75
}

/// Interactive rectangles that are *not* in the background layer — menus, popovers,
/// flyouts and the two overlays.
///
/// The discriminator is the layer rather than the geometry, because a menu row and a
/// sidebar row are the same size by design and telling them apart by height would be a
/// test that passes for the wrong reason.
fn overlay_widgets(ctx: &Context) -> Vec<Rect> {
    widgets(ctx, |rect| {
        ctx.layer_id_at(rect.center()).is_some_and(|layer| layer.order != Order::Background)
    })
}

/// The rows of whatever menu or context menu is open.
///
/// A menu row is one `interact_size` tall, which excludes the popup's own container
/// rectangle, the 40-point tool buttons and the 26-point status cluster — all of which
/// are also outside the background layer.
fn menu_rows(ctx: &Context) -> Vec<Rect> {
    overlay_widgets(ctx).into_iter().filter(|rect| approx(rect.height(), 24.0)).collect()
}

fn board(path: &str, title: &str) -> BoardCard {
    BoardCard {
        path: path.into(),
        title: title.to_owned(),
        item_count: 596,
        modified: SystemTime::UNIX_EPOCH,
        starred: false,
        deleted: None,
        thumbnail: None,
    }
}

fn library_state(boards: &[BoardCard]) -> ChromeState<'_> {
    ChromeState {
        screen: Screen::Library,
        library: boards,
        now: SystemTime::UNIX_EPOCH,
        ..ChromeState::default()
    }
}

/// The cards in the grid — the only things in the library large enough to be one.
fn cards(ctx: &Context) -> Vec<Rect> {
    widgets(ctx, |rect| rect.width() > 200.0 && rect.height() > 150.0)
}

/// The 24-point squares in the library header: the grid/list toggle, and nothing else.
fn layout_toggles(ctx: &Context) -> Vec<Rect> {
    widgets(ctx, |rect| approx(rect.width(), 24.0) && approx(rect.height(), 24.0))
}

#[test]
fn clicking_a_tool_in_the_palette_switches_the_tool() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    // The palette is the only cluster of 40-point squares, and it is on the left.
    let buttons = widgets(&ctx, |rect| {
        approx(rect.width(), 40.0) && approx(rect.height(), 40.0) && rect.left() < 100.0
    });
    // The everyday tools, plus one **More** button standing for the six folded behind
    // it. Derived rather than a literal 9, so moving a tool between the two lists moves
    // this with it instead of failing here.
    let on_palette = Tool::ALL.len() - Tool::OCCASIONAL.len() + 1;
    assert_eq!(buttons.len(), on_palette, "one button per palette tool, plus More: {buttons:?}");

    // Third from the top, in palette order: Select, Hand, Sticky.
    let output = frame(&ctx, &mut chrome, &state, click_at(buttons[2].center()));
    assert_eq!(output.events, vec![UiEvent::ToolChanged(Tool::Sticky)]);
    assert!(output.pointer_over_ui, "the palette must swallow the click");
}

#[test]
fn clicking_the_shape_tool_opens_its_flyout_and_picking_a_shape_chooses_it() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    let buttons = widgets(&ctx, |rect| {
        approx(rect.width(), 40.0) && approx(rect.height(), 40.0) && rect.left() < 100.0
    });
    // Select, Hand, Sticky, Text, Shape.
    let shape_button = buttons[4];
    let output = frame(&ctx, &mut chrome, &state, click_at(shape_button.center()));
    assert_eq!(output.events, vec![UiEvent::ToolChanged(Tool::Shape)]);

    // The flyout is laid out on the following pass; its tiles are 30-point squares
    // to the right of the palette.
    settle(&ctx, &mut chrome, &state);
    let tiles = widgets(&ctx, |rect| {
        approx(rect.width(), 30.0)
            && approx(rect.height(), 30.0)
            && rect.left() > shape_button.right()
    });
    assert_eq!(
        tiles.len(),
        vellum_ui::SHAPE_CATALOGUE.len(),
        "every catalogue shape gets a tile"
    );

    let output = frame(&ctx, &mut chrome, &state, click_at(tiles[0].center()));
    assert_eq!(
        output.events,
        vec![
            UiEvent::ShapeChosen(vellum_ui::Shape::Rectangle),
            UiEvent::ToolChanged(Tool::Shape),
        ]
    );
    assert_eq!(chrome.shape(), vellum_ui::Shape::Rectangle);
}

#[test]
fn clicking_the_minimap_button_in_the_status_cluster_emits_its_command() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    // The cluster is anchored to the bottom-right corner; nothing else lives there.
    let cluster = widgets(&ctx, |rect| {
        approx(rect.height(), 26.0) && rect.bottom() > SCREEN.y - 60.0
    });
    assert!(!cluster.is_empty(), "the status cluster drew no buttons");

    let output = frame(&ctx, &mut chrome, &state, click_at(cluster[0].center()));
    assert_eq!(output.events, vec![UiEvent::Command(Command::ToggleMinimap)]);
}

#[test]
fn clicking_a_board_card_asks_the_app_to_open_it() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [
        BoardCard {
            path: "/boards/site-plan.vellum".into(),
            title: "Site plan".to_owned(),
            item_count: 596,
            modified: SystemTime::UNIX_EPOCH,
            starred: false,
            deleted: None,
            thumbnail: None,
        },
        BoardCard {
            path: "/boards/roadmap.vellum".into(),
            title: "Roadmap".to_owned(),
            item_count: 12,
            modified: SystemTime::UNIX_EPOCH,
            starred: false,
            deleted: None,
            thumbnail: None,
        },
    ];
    let state = ChromeState {
        screen: Screen::Library,
        library: &boards,
        now: SystemTime::UNIX_EPOCH,
        ..ChromeState::default()
    };
    settle(&ctx, &mut chrome, &state);

    // A card is the only thing in the library large enough to be one.
    let cards = widgets(&ctx, |rect| rect.width() > 200.0 && rect.height() > 150.0);
    assert_eq!(cards.len(), boards.len(), "one card per board: {cards:?}");

    // Near the bottom of the card, clear of the preview and the labels.
    let target = Pos2::new(cards[1].center().x, cards[1].bottom() - 6.0);
    let output = frame(&ctx, &mut chrome, &state, click_at(target));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::Open("/boards/roadmap.vellum".into()))]
    );
    assert_eq!(output.canvas_rect, Rect::NOTHING, "the library has no canvas");
}

/// *"dont put that much space for folders, it should look the same … i just want it to look
/// organized"*.
///
/// The folder used to be a row of its own under the title, so a filed card was ~18 points taller
/// than an unfiled one — and the card frame wrapped its content, so the white boxes in a row
/// ended at different heights. On top of that the grid's 16-point gutter was inherited as the
/// card's *internal* leading, which spent 24 points of every card on air and floated the title
/// into the middle of it.
///
/// Two things are asserted here and a third is asserted by `card` itself. The grid is even —
/// same rectangle everywhere, and the gap between rows is the gap between columns. That the
/// **painted** card fills its slot is `card` painting its frame at the slot rather than wrapping
/// one around its content, and that the content still fits inside it is the `debug_assert`
/// beside that, which this test runs: a filed board is in the fixture precisely so the folder is
/// laid out while it does.
#[test]
fn the_board_grid_is_even_whether_or_not_a_board_is_filed() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    // Twelve, so the tail band wraps onto a second row of its own: with fewer, every row
    // on the page is the first row of some band and the assertion below has nothing to
    // compare.
    let boards: Vec<BoardCard> = (0..12)
        .map(|n| board(&format!("/boards/b{n}.vellum"), &format!("Board {n}")))
        .collect();
    // One filed, and one filed under a name long enough to want the whole row.
    chrome.set_spaces(vec![
        Space::new("Cars", [PathBuf::from("/boards/b0.vellum")]),
        Space::new("Somewhere with a very long name indeed", [PathBuf::from("/boards/b3.vellum")]),
    ]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let cards = cards(&ctx);
    assert_eq!(cards.len(), boards.len(), "one card per board: {cards:?}");

    let first = cards[0];
    for card in &cards {
        assert!(
            approx(card.width(), first.width()) && approx(card.height(), first.height()),
            "every card is the same rectangle: {first:?} vs {card:?}",
        );
    }

    // The gap between two rows **of the same band**. The Recent page is banded now —
    // Recently opened, Pinned, All boards — so consecutive rows are not all neighbours: a
    // pair either side of a section heading is legitimately further apart. The *smallest*
    // gap is therefore the one inside a band, and that is the one that has to match the
    // gutter between columns.
    let mut tops: Vec<f32> = cards.iter().map(|card| card.top()).collect();
    tops.dedup_by(|a, b| (*a - *b).abs() < 1.0);
    assert!(tops.len() >= 2, "twelve boards should occupy more than one row: {cards:?}");
    let down = tops
        .windows(2)
        .map(|pair| pair[1] - pair[0] - first.height())
        .fold(f32::INFINITY, f32::min);
    let across = cards[1].left() - first.left() - first.width();
    assert!(
        approx(down, across),
        "the gutter between rows of one band ({down}) is the gutter between columns \
         ({across}); rows at {tops:?}",
    );
}

/// The Recent page is three bands, and a pinned folder is one of the things in them.
///
/// *"on the top there would be one row that is the most recent boards that i opened … and
/// then the next row will be pinned boards if i have any pinned boards or folders."*
///
/// Two things here that the pure `recent_bands` test cannot reach: that the headings are
/// actually drawn, and that a **folder** appears in the grid at all — a folder tile is a new
/// kind of thing in a list that had only ever held boards, and clicking one has to change
/// the scope rather than open a board.
#[test]
fn the_recent_page_is_banded_and_a_pinned_folder_is_in_it() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let mut boards: Vec<BoardCard> = (0..8)
        .map(|n| board(&format!("/boards/b{n}.vellum"), &format!("Board {n}")))
        .collect();
    // Starred, and far enough down the list to fall past the top row.
    boards[7].starred = true;
    chrome.set_spaces(vec![
        Space::new("Cars", [PathBuf::from("/boards/b0.vellum")]).pinned(),
        Space::new("Home", [PathBuf::from("/boards/b1.vellum")]),
    ]);
    let state = library_state(&boards);
    let full = frame_painted(&ctx, &mut chrome, &state, input());
    settle(&ctx, &mut chrome, &state);

    let words: Vec<String> = painted_text(&full).into_iter().map(|(_, text, _)| text).collect();
    for heading in ["Recently opened", "Pinned", "All boards"] {
        assert!(
            words.iter().any(|w| w.eq_ignore_ascii_case(heading)),
            "no `{heading}` heading: {words:?}",
        );
    }
    // One tile per **pinned** folder, in the grid beside the cards — so exactly one
    // card-sized thing exists that is not one of the eight boards.
    //
    // This is also what proves the unpinned folder is not promoted, and it is the only
    // assertion that can: searching the painted text for "Home" finds it twice on a correct
    // screen, once in the sidebar's own folder list and once as the folder chip on the card
    // of the board filed there.
    let tiles = cards(&ctx);
    assert_eq!(tiles.len(), boards.len() + 1, "eight boards and one folder tile: {tiles:?}");

    // And it opens the folder rather than a board: no event leaves the chrome, because the
    // scope is the library's own state, and the page retitles itself.
    //
    // Found by its **count**, not by its name: "Cars" is painted on the card of the board
    // filed there as well, as its folder chip, so searching for the name finds b0's card
    // and clicking that opens a board — which is exactly what this test caught the first
    // time it was written. A folder counts boards and a board counts items, and nothing on
    // this screen but a folder tile says "1 board".
    let cars = tiles
        .iter()
        .find(|rect| {
            painted_text(&full)
                .iter()
                .any(|(_, text, at)| text == "1 board" && rect.contains(at.center()))
        })
        .copied()
        .expect("the Cars tile");
    let output = frame(&ctx, &mut chrome, &state, click_at(cars.center()));
    assert!(output.events.is_empty(), "a folder tile is not a board: {:?}", output.events);
    settle(&ctx, &mut chrome, &state);
    let after: Vec<String> =
        painted_text(&frame_painted(&ctx, &mut chrome, &state, input()))
            .into_iter()
            .map(|(_, text, _)| text)
            .collect();
    assert!(after.iter().any(|w| w == "Cars"), "the page did not open the folder: {after:?}");
}

/// *"in the all boards page i want you to give me an option to group them by folders or
/// free for all."*
///
/// Two assertions and the second is the one worth having: the control exists **only** on
/// All boards. Every other scope either has its own arrangement — Recent is banded — or *is*
/// one folder already, so a grouping switch there would be a control that changes nothing,
/// which is worse than no control because it invites the question of why it did not work.
#[test]
fn all_boards_can_be_grouped_by_folder_or_left_flat() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards: Vec<BoardCard> = (0..4)
        .map(|n| board(&format!("/boards/b{n}.vellum"), &format!("Board {n}")))
        .collect();
    chrome.set_spaces(vec![
        Space::new("Cars", [PathBuf::from("/boards/b0.vellum")]),
        Space::new("Home", [PathBuf::from("/boards/b1.vellum")]),
    ]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    // The 24-point squares in the header. On Recent there are two — the list and grid
    // toggles — and the grouping pair must not be among them.
    let on_recent = layout_toggles(&ctx).len();
    assert_eq!(on_recent, 2, "Recent should carry the layout toggle and nothing else");

    // Into All boards, through the sidebar row rather than by setting state, so the
    // control is being asked for on the screen a user would be looking at.
    // Sidebar rows only — `rect.right()` inside the rail. Without it the header's own
    // "Import from Miro" button is the same 24 points tall and sorts first.
    let rows = widgets(&ctx, |rect| {
        rect.right() < vellum_ui::theme::SIDEBAR_WIDTH && approx(rect.height(), 24.0)
    });
    let all_boards =
        *rows.get(1).unwrap_or_else(|| panic!("Recent · All boards · Starred: {rows:?}"));
    let _ = frame(&ctx, &mut chrome, &state, click_at(all_boards.center()));
    settle(&ctx, &mut chrome, &state);

    assert_eq!(
        layout_toggles(&ctx).len(),
        on_recent + 2,
        "All boards should add the two grouping buttons",
    );

    // Grouped is the default, so the folder names head their own sections. Compared
    // case-insensitively: `section_label` letterspaces and uppercases, so the heading is
    // painted as "NOT IN A FOLDER" and asserting the cased string tests the label style
    // rather than the grouping.
    let full = frame_painted(&ctx, &mut chrome, &state, input());
    let headings: Vec<String> = painted_text(&full).into_iter().map(|(_, t, _)| t).collect();
    assert!(
        headings.iter().any(|w| w.eq_ignore_ascii_case("Not in a folder")),
        "the two unfiled boards need a heading of their own: {headings:?}",
    );
}

/// *"when a person deletes their board make it so that it puts it in the recently deleted
/// folder, and there should be a recently deleted folder."*
///
/// Three properties, and the third is the one a screenshot would not show. A deleted board
/// is in **exactly one** scope — it must not still be sitting in Recent, in All boards, in
/// Starred or in the folder it was filed under, and it stays filed under that folder on
/// disk the whole time so a restore puts it back where it was. Clicking it **restores**
/// rather than opens, because the only thing anyone wants from a board they deleted is to
/// have it back, and opening one would leave it deleted while it was on screen.
#[test]
fn a_deleted_board_is_only_in_the_trash_and_a_click_restores_it() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let mut boards = [
        board("/boards/kept.vellum", "Kept"),
        board("/boards/gone.vellum", "Gone"),
    ];
    // Starred *and* filed, so the two scopes that could leak it are both exercised.
    boards[1].starred = true;
    boards[1].deleted = Some(SystemTime::UNIX_EPOCH);
    chrome.set_spaces(vec![Space::new("Cars", [PathBuf::from("/boards/gone.vellum")])]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    // Recent is the opening scope: one card, and it is not the deleted one.
    assert_eq!(cards(&ctx).len(), 1, "the deleted board is still in Recent");

    // The trash is the last of the four standing rows in the sidebar.
    let rows = widgets(&ctx, |rect| {
        rect.right() < vellum_ui::theme::SIDEBAR_WIDTH && approx(rect.height(), 24.0)
    });
    let trash = *rows.get(3).unwrap_or_else(|| panic!("Recent · All · Starred · Trash: {rows:?}"));
    let _ = frame(&ctx, &mut chrome, &state, click_at(trash.center()));
    settle(&ctx, &mut chrome, &state);

    let trashed = cards(&ctx);
    assert_eq!(trashed.len(), 1, "the trash should hold exactly the deleted board");

    // And clicking it asks for a restore, not an open.
    let output = frame(&ctx, &mut chrome, &state, click_at(trashed[0].center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::Restore("/boards/gone.vellum".into()))],
    );
}

/// A starred board is drawn **twice** on the Recent page, and both cards work.
///
/// *"the boards that are starred, even if they are under recently opened, if they are
/// starred they should still be under pinned."* Pinned is where you go to find what you
/// pinned; one missing because it happens to be recent makes the section untrustworthy.
///
/// The half that needs a real frame rather than the pure `recent_bands` test is the **id**.
/// Two cards for one board are two widgets, and `card_id` used to be `("vellum-board",
/// path)` with a comment saying a board appears once per frame so there is nothing to
/// collide with — true when it was written, false two sections later. On a clash egui gives
/// the interaction to whichever registered last, so the ring would light on one card while
/// the click landed on the other. This clicks the **second** copy and expects it to answer.
#[test]
fn both_copies_of_a_starred_board_are_live() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let mut boards = [
        board("/boards/a.vellum", "Alpha"),
        board("/boards/b.vellum", "Beta"),
    ];
    boards[0].starred = true;
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    // Two boards, three cards: Alpha is in the top row and under Pinned.
    let grid = cards(&ctx);
    assert_eq!(grid.len(), 3, "the starred board should appear in both bands: {grid:?}");

    // Sorted top-then-left, so the last one is the Pinned copy on its own row below.
    let pinned = grid[2];
    assert!(pinned.top() > grid[0].bottom(), "the third card is not on a later row: {grid:?}");

    let output = frame(&ctx, &mut chrome, &state, click_at(pinned.center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::Open("/boards/a.vellum".into()))],
        "the Pinned copy is dead — the two cards are sharing an Id",
    );
}

#[test]
fn a_click_on_the_canvas_is_left_for_the_app() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    let first = settle(&ctx, &mut chrome, &state);

    let output = frame(&ctx, &mut chrome, &state, click_at(first.canvas_rect.center()));
    assert!(output.events.is_empty());
    assert!(!output.pointer_over_ui, "the middle of the canvas is not chrome");
}

/// A control that overruns the properties panel is clipped at the window edge, and
/// the clipping looks like a rendering bug rather than a layout one. The panel is a
/// fixed width, so every row in it has a fixed budget and this is checkable.
#[test]
fn no_control_overruns_the_properties_panel() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [everything_selected()];
    let families = vec!["Noto Sans".to_owned(), "Inter".to_owned()];
    let state = ChromeState {
        font_families: &families,
        ..board_state(&selection)
    };

    // A tall window so the whole panel lays out without scrolling, and so the
    // status cluster — which is anchored to the bottom corner and legitimately
    // overlaps the panel's column — is far below the rows being checked.
    let raw = || RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1440.0, 1600.0))),
        ..RawInput::default()
    };
    let _ = frame(&ctx, &mut chrome, &state, raw());
    let output = frame(&ctx, &mut chrome, &state, raw());

    let panel_left = output.canvas_rect.right();
    // The panel's own inner margin, plus room for the overlay scrollbar.
    let limit = 1440.0 - 8.0;
    let overrunning: Vec<Rect> = ctx
        .interactive_rects_last_pass()
        .into_iter()
        .filter(|rect| rect.left() >= panel_left && rect.top() < 1200.0)
        // The tab strip and the menu bar both span the full width and neither is the
        // panel's business.
        .filter(|rect| rect.top() > MENU_BOTTOM)
        .filter(|rect| rect.right() > limit)
        .collect();
    assert!(overrunning.is_empty(), "controls past {limit}: {overrunning:#?}");
}

/// A sticky with every property the panel can edit filled in.
fn everything_selected() -> vellum_ui::SelectionItem {
    use vellum_ui::{
        Align, Border, Color, FontWeight, ItemFacet, LineStyle, Placement, SelectionItem,
        TextSummary, VerticalAlign,
    };
    SelectionItem {
        fill: Some(Some(Color::rgb(0xFF, 0xF7, 0x9E))),
        opacity: Some(1.0),
        border: Some(Border {
            color: Color::rgb(0x33, 0x33, 0x33),
            width: 1.0,
            style: LineStyle::Solid,
        }),
        text: Some(TextSummary {
                content: "radiator fan".to_owned(),
            family: Some("Noto Sans".to_owned()),
            size: Some(14.0),
            weight: FontWeight::Regular,
            color: Color::rgb(0, 0, 0),
            align: Align::Center,
            vertical_align: VerticalAlign::Middle,
            line_height: 1.2,
        }),
        ..SelectionItem::new(
            "1@1".parse().expect("well-formed item id"),
            ItemFacet::Sticky,
            Placement::new(1234.0, 5678.0, 200.0, 200.0),
        )
    }
}

/// *"boards should not go off screen"*.
///
/// The grid asked egui for `with_main_wrap(true)` and did not get it, because the cards
/// were placed with `scope_builder`, which never allocates, and the wrap is only ever
/// tested inside `allocate_space`. Twelve boards laid out as one endless row running off
/// the right of the window, clipped by a vertical-only `ScrollArea` with no way to reach
/// them.
///
/// This has to live out here rather than in the crate's own tests: `library.rs`'s test
/// helper builds its `RawInput` with no `screen_rect` at all, so there is no window for
/// a card to fall outside of, and every one of its assertions is about emitted events.
/// The bug was pure geometry and needed a screen to be visible.
#[test]
fn every_board_card_stays_inside_the_window_and_the_grid_wraps_into_rows() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards: Vec<_> =
        (0..12).map(|i| board(&format!("/boards/b{i}.vellum"), &format!("Board {i}"))).collect();
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let grid = cards(&ctx);
    assert_eq!(grid.len(), boards.len(), "one card per board: {grid:?}");

    // The window, not the panel: the sidebar is a left panel, so anything past the
    // window's right edge is unreachable by any means.
    for (i, card) in grid.iter().enumerate() {
        assert!(
            card.right() <= SCREEN.x,
            "card {i} runs off the right edge: right={} window={}",
            card.right(),
            SCREEN.x
        );
        assert!(card.left() >= 0.0, "card {i} starts off the left edge: {card:?}");
    }

    // Wrapped, not merely clipped. Twelve 236-point cards cannot fit across 1440 points,
    // so a correct layout puts them on several rows — and each row must hold more than
    // one card, or "wrapping" would just be a column.
    let mut tops: Vec<i32> = grid.iter().map(|rect| rect.top() as i32).collect();
    tops.sort_unstable();
    tops.dedup();
    assert!(tops.len() > 1, "all 12 cards share one row — the grid did not wrap: {grid:?}");
    assert!(
        tops.len() < grid.len(),
        "every card got its own row — that is a column, not a grid: {grid:?}"
    );

    // Cards in a row hang from a common top edge. This is why the layout is spelled out
    // instead of using `horizontal_wrapped`, which centres on the cross axis and lets a
    // two-line title float its neighbours out of alignment.
    let first_row: Vec<_> = grid.iter().filter(|r| r.top() as i32 == tops[0]).collect();
    assert!(first_row.len() > 1, "the first row holds one card: {grid:?}");
    for card in &first_row {
        assert!(approx(card.height(), first_row[0].height()), "ragged row: {first_row:?}");
    }
}

/// The toggle `docs/04-ui-reference.md` §5 shows beside Filter and Sort. It was drawn
/// before this and it has to *do* something: the whole point of the list is that it
/// scales past a screenful, and a toggle that only highlights itself is decoration.
#[test]
fn the_grid_list_toggle_actually_switches_the_layout() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [board("/boards/site-plan.vellum", "Site plan"), board("/boards/roadmap.vellum", "Roadmap")];
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    assert_eq!(cards(&ctx).len(), 2, "the library opens in a grid");
    let toggles = layout_toggles(&ctx);
    assert_eq!(toggles.len(), 2, "one button per layout: {toggles:?}");

    // Sorted left to right, and the list button is the right-hand one: the row is laid
    // out right-to-left, so the first entry added sits furthest right.
    let output = frame(&ctx, &mut chrome, &state, click_at(toggles[1].center()));
    assert!(output.events.is_empty(), "the layout is the chrome's own state");
    settle(&ctx, &mut chrome, &state);
    assert!(cards(&ctx).is_empty(), "the cards should have become rows");

    // A row still opens its board, which is the property that makes the list usable.
    let rows = widgets(&ctx, |rect| rect.width() > 400.0 && approx(rect.height(), 32.0));
    assert_eq!(rows.len(), 2, "one row per board: {rows:?}");
    let output = frame(&ctx, &mut chrome, &state, click_at(rows[0].center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::Open("/boards/site-plan.vellum".into()))]
    );

    settle(&ctx, &mut chrome, &state);
    let toggles = layout_toggles(&ctx);
    let _ = frame(&ctx, &mut chrome, &state, click_at(toggles[0].center()));
    settle(&ctx, &mut chrome, &state);
    assert_eq!(cards(&ctx).len(), 2, "and back to the grid");
}

/// Starring is the cheapest organisation the library offers, and it was missing.
#[test]
fn clicking_a_star_asks_the_app_to_star_that_board() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let mut boards =
        [board("/boards/site-plan.vellum", "Site plan"), board("/boards/roadmap.vellum", "Roadmap")];
    boards[1].starred = true;
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    // 20-point squares to the right of the sidebar: one star per card, and nothing else
    // that size lives in the main pane.
    //
    // **Three, for two boards.** The starred one is drawn twice on the Recent page — once
    // in the top row and once under Pinned, which is what the user asked for — so a star
    // per card is one more than a star per board. This is also the assertion that would
    // fail if the two cards shared an egui `Id`: a clash gives the interaction to whichever
    // registered last, and only one of them would be a widget at all.
    let stars = widgets(&ctx, |rect| {
        approx(rect.width(), 20.0) && approx(rect.height(), 20.0) && rect.left() > 200.0
    });
    assert_eq!(stars.len(), boards.len() + 1, "one star per card: {stars:?}");

    let output = frame(&ctx, &mut chrome, &state, click_at(stars[0].center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::SetStarred {
            path: "/boards/site-plan.vellum".into(),
            starred: true,
        })]
    );

    // …and the already-starred one asks to be unstarred, rather than asking again.
    settle(&ctx, &mut chrome, &state);
    let stars = widgets(&ctx, |rect| {
        approx(rect.width(), 20.0) && approx(rect.height(), 20.0) && rect.left() > 200.0
    });
    let output = frame(&ctx, &mut chrome, &state, click_at(stars[1].center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::SetStarred {
            path: "/boards/roadmap.vellum".into(),
            starred: false,
        })]
    );
}

/// The 20-point squares in the board library's rail.
fn sidebar_buttons(ctx: &Context) -> Vec<Rect> {
    widgets(ctx, |rect| {
        approx(rect.width(), 20.0) && approx(rect.height(), 20.0) && rect.right() < 192.0
    })
}

/// The full-width rows in the rail, top to bottom: the standing scopes, then one per
/// space. Spaces are the last `count` of them, which is stabler than counting from the
/// top past a search field whose height is egui's to decide.
fn space_rows(ctx: &Context, count: usize) -> Vec<Rect> {
    let rows: Vec<Rect> = widgets(ctx, |rect| {
        approx(rect.height(), 24.0) && rect.right() < 192.0 && rect.width() > 120.0
    });
    assert!(rows.len() >= count + 3, "the rail drew {} rows", rows.len());
    rows[rows.len() - count..].to_vec()
}

/// The user has six real spaces. Creating, pinning, renaming and deleting them all
/// have to leave the sidebar as events, or Spaces is a picture of a feature.
#[test]
fn spaces_can_be_created_and_acted_on_from_the_sidebar() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [board("/boards/site-plan.vellum", "Site plan")];
    chrome.set_spaces(vec![
        Space::new("Cars", ["/boards/site-plan.vellum".into()]).pinned(),
        Space::new("Books", []),
    ]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    // With the pointer off the rail, the only 20-point square in it is the `+` beside
    // the Spaces heading: a space row shows its count until it is pointed at.
    let buttons = sidebar_buttons(&ctx);
    assert_eq!(buttons.len(), 1, "a row's buttons are revealed, not standing: {buttons:?}");

    let output = frame(&ctx, &mut chrome, &state, click_at(buttons[0].center()));
    assert_eq!(output.events, vec![UiEvent::Library(LibraryEvent::CreateSpace)]);

    // The pinned space sorts to the top, so the first space row is Cars'. Hovering it
    // reveals the pin and the overflow menu, as Miro does.
    settle(&ctx, &mut chrome, &state);
    let cars = space_rows(&ctx, 2)[0];
    let output = frame(&ctx, &mut chrome, &state, hover_at(cars.center()));
    assert!(output.events.is_empty(), "hovering a row is not an action");
    settle(&ctx, &mut chrome, &state);

    let buttons = sidebar_buttons(&ctx);
    assert_eq!(buttons.len(), 3, "the plus, and the row's pin and menu: {buttons:?}");
    let (pin, more) = (buttons[1], buttons[2]);
    assert!(pin.left() < more.left(), "the pin comes before the overflow");

    // The pin acts from the row itself — one click rather than a menu.
    let output = frame(&ctx, &mut chrome, &state, click_at(pin.center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::SetSpacePinned {
            space: "Cars".to_owned(),
            pinned: false,
        })],
        "the pinned space offers to be unpinned"
    );

    // …and the same three verbs are still in the menu behind the `⋮`.
    settle(&ctx, &mut chrome, &state);
    let _ = frame(&ctx, &mut chrome, &state, click_at(more.center()));
    settle(&ctx, &mut chrome, &state);
    let items = menu_rows(&ctx);
    assert_eq!(items.len(), 3, "rename, pin and delete: {items:?}");
    let output = frame(&ctx, &mut chrome, &state, click_at(items[0].center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::RenameSpace("Cars".to_owned()))]
    );
}

/// *"i can create folders and inside the folders i can have more boards"* — and the
/// gesture a folder implies is dragging something into it. The other two routes stay,
/// but neither is what a hand reaches for with the rail already in view.
#[test]
fn dragging_a_board_onto_a_space_files_it_there() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [board("/boards/site-plan.vellum", "Site plan")];
    chrome.set_spaces(vec![Space::new("Cars", []), Space::new("Books", [])]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let card = cards(&ctx)[0];
    let books = space_rows(&ctx, 2)[1];
    let output = drag(&ctx, &mut chrome, &state, card.center(), books.center());
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::MoveToSpace {
            path: "/boards/site-plan.vellum".into(),
            space: Some("Books".to_owned()),
        })],
        "the drop landed on the wrong row, or on none"
    );

    // A drag that ends anywhere else is one the user thought better of: no filing, and
    // no board opened either, which is what the same gesture would do as a click.
    settle(&ctx, &mut chrome, &state);
    let card = cards(&ctx)[0];
    let output = drag(&ctx, &mut chrome, &state, card.center(), Pos2::new(700.0, 700.0));
    assert!(output.events.is_empty(), "{:?}", output.events);
}

/// The strip is *constant*: with nothing open at all there is still a strip, and the
/// home tab on it is the one thing that cannot be got rid of.
#[test]
fn the_tab_strip_is_there_with_no_board_open_and_home_cannot_be_closed() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = library_state(&[]);
    settle(&ctx, &mut chrome, &state);

    assert!(board_tabs(&ctx).is_empty(), "no boards are open");
    let strip = strip_widgets(&ctx);
    assert_eq!(strip.len(), 2, "the home tab and the `+`, and nothing else: {strip:?}");
    let (home, plus) = (strip[0], strip[1]);
    assert!(home.left() <= 0.5, "home is first");
    assert!(home.width() < vellum_ui::theme::TAB_MIN_WIDTH, "and narrow");
    assert!(plus.right() > SCREEN.x - 40.0, "the `+` is at the end of the strip");

    // Clicking home when home is already in front asks for nothing: it is where the
    // window already is.
    let output = frame(&ctx, &mut chrome, &state, click_at(home.center()));
    assert!(output.events.is_empty(), "{:?}", output.events);
    assert!(chrome.showing_library());

    // There is no close button on it to click, at any distance from it.
    let output = frame(&ctx, &mut chrome, &state, hover_at(home.center()));
    assert!(output.events.is_empty());
    settle(&ctx, &mut chrome, &state);
    assert_eq!(strip_widgets(&ctx).len(), 2, "home grew a control on hover");
}

/// Chrome's behaviour, because it is the one every user already has: a board is a tab,
/// a click switches, the `×` closes and the middle button closes without one.
#[test]
fn a_tab_switches_on_a_click_and_closes_from_its_button_or_the_middle_one() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    chrome.set_tabs(vec![
        BoardTab::new(TabKey(1), "Site plan").with_path("/boards/site-plan.vellum"),
        BoardTab::new(TabKey(2), "Roadmap"),
        BoardTab::new(TabKey(3), "Wiring"),
    ]);
    let state = library_state(&[]);
    settle(&ctx, &mut chrome, &state);

    let tabs = board_tabs(&ctx);
    assert_eq!(tabs.len(), 3, "one tab per open board: {tabs:?}");
    assert!(tabs[0].left() >= vellum_ui::theme::TAB_HOME_WIDTH, "board tabs follow home");
    for tab in &tabs {
        assert!(approx(tab.height(), STRIP), "a tab fills the strip: {tab:?}");
        assert!(tab.width() <= vellum_ui::theme::TAB_MAX_WIDTH + 0.5);
    }

    // A click brings it forward and asks the app for the document behind it.
    let output = frame(&ctx, &mut chrome, &state, click_at(tabs[1].center()));
    assert_eq!(output.events, vec![UiEvent::SelectTab(2)]);
    assert_eq!(chrome.active_tab(), 2);
    assert!(!chrome.showing_library());

    // The `×` appears under the pointer; the tab in front already has one.
    settle(&ctx, &mut chrome, &state);
    let close = strip_widgets(&ctx)
        .into_iter()
        .find(|rect| approx(rect.width(), 16.0) && approx(rect.height(), 16.0))
        .expect("the tab in front has a close button");
    let output = frame(&ctx, &mut chrome, &state, click_at(close.center()));
    assert_eq!(output.events, vec![UiEvent::CloseTab(2)]);
    assert_eq!(chrome.tabs().len(), 2);
    assert_eq!(chrome.active_tab(), 2, "the right-hand neighbour came forward");

    // Middle-click closes wherever it lands, with no button to find first.
    settle(&ctx, &mut chrome, &state);
    let tabs = board_tabs(&ctx);
    let output = frame(&ctx, &mut chrome, &state, middle_click_at(tabs[0].center()));
    assert_eq!(output.events, vec![UiEvent::CloseTab(1)]);
    assert_eq!(chrome.tabs().len(), 1);
}

/// Drag to reorder, and the strip rearranges under the pointer rather than at the end.
#[test]
fn a_tab_can_be_dragged_into_a_new_slot() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    chrome.set_tabs(vec![
        BoardTab::new(TabKey(1), "Site plan"),
        BoardTab::new(TabKey(2), "Roadmap"),
        BoardTab::new(TabKey(3), "Wiring"),
    ]);
    let state = library_state(&[]);
    settle(&ctx, &mut chrome, &state);

    let tabs = board_tabs(&ctx);
    let width = tabs[0].width();
    let output = drag(
        &ctx,
        &mut chrome,
        &state,
        tabs[0].center(),
        tabs[0].center() + vec2(width * 1.5, 0.0),
    );
    assert_eq!(output.events, vec![UiEvent::ReorderTabs { from: 1, to: 2 }]);
    assert_eq!(
        chrome.tabs().iter().map(|t| t.key).collect::<Vec<_>>(),
        vec![TabKey(2), TabKey(1), TabKey(3)]
    );

    // …and nothing can be dropped in front of home.
    settle(&ctx, &mut chrome, &state);
    let tabs = board_tabs(&ctx);
    let _ = drag(&ctx, &mut chrome, &state, tabs[0].center(), Pos2::new(2.0, STRIP / 2.0));
    assert_eq!(
        chrome.tabs().iter().map(|t| t.key).collect::<Vec<_>>(),
        vec![TabKey(2), TabKey(1), TabKey(3)],
        "the first tab has nowhere further left to go"
    );
    assert!(chrome.showing_library(), "and home is still home");
}

/// The unsaved-work dot the user asked to be able to *see*. It must not move the
/// title, or the strip twitches on every keystroke.
#[test]
fn the_saving_dot_does_not_change_a_tabs_geometry() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = library_state(&[]);
    chrome.set_tabs(vec![BoardTab::new(TabKey(1), "Site plan")]);
    settle(&ctx, &mut chrome, &state);
    let clean = board_tabs(&ctx);

    chrome.set_tabs(vec![BoardTab::new(TabKey(1), "Site plan").dirty(true)]);
    settle(&ctx, &mut chrome, &state);
    assert_eq!(board_tabs(&ctx), clean, "the dot moved the tab it appeared on");
}

/// The strip takes its band off the top of the window, and the board starts under it.
#[test]
fn the_canvas_starts_below_the_strip_and_under_the_floating_pill() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let output = settle(&ctx, &mut chrome, &board_state(&[]));
    // Only the strip is docked. The menu pill floats, so the board runs underneath it
    // and the 36 points it used to take across the whole window came back.
    assert!(
        output.canvas_rect.top() >= STRIP,
        "the canvas runs under the strip: {:?}",
        output.canvas_rect
    );

    // The pointer being on the strip is the chrome's, not the board's.
    let on_strip = Pos2::new(SCREEN.x / 2.0, STRIP / 2.0);
    let output = frame(&ctx, &mut chrome, &board_state(&[]), hover_at(on_strip));
    assert!(output.pointer_over_ui);
}

/// `docs/04-ui-reference.md` §3's *My Shapes*. The app supplies them; the picker lists
/// them above the built-in categories and asks for more.
#[test]
fn the_shape_picker_lists_the_users_own_shapes_and_can_ask_for_another() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    chrome.set_custom_shapes(vec![CustomShape {
        id: CustomShapeId(7),
        name: "Wiring symbol".to_owned(),
        thumbnail: None,
    }]);
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    let buttons = widgets(&ctx, |rect| {
        approx(rect.width(), 40.0) && approx(rect.height(), 40.0) && rect.left() < 100.0
    });
    let shape_button = buttons[4];
    let _ = frame(&ctx, &mut chrome, &state, click_at(shape_button.center()));
    settle(&ctx, &mut chrome, &state);

    let tiles = widgets(&ctx, |rect| {
        approx(rect.width(), 30.0)
            && approx(rect.height(), 30.0)
            && rect.left() > shape_button.right()
    });
    assert_eq!(
        tiles.len(),
        vellum_ui::SHAPE_CATALOGUE.len() + 1,
        "the catalogue plus the one uploaded shape"
    );

    // My Shapes sits above the categories, so the uploaded one is the first tile.
    let output = frame(&ctx, &mut chrome, &state, click_at(tiles[0].center()));
    assert_eq!(
        output.events,
        vec![
            UiEvent::CustomShapeChosen(CustomShapeId(7)),
            UiEvent::ToolChanged(Tool::Shape),
        ]
    );
}

/// Miro's *Apply colors*, per category. The colours are a default for what gets
/// created, so choosing one has to reach the app rather than only the picker.
#[test]
fn apply_colours_reports_the_choice_for_the_category_it_was_opened_from() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    let buttons = widgets(&ctx, |rect| {
        approx(rect.width(), 40.0) && approx(rect.height(), 40.0) && rect.left() < 100.0
    });
    let shape_button = buttons[4];
    let _ = frame(&ctx, &mut chrome, &state, click_at(shape_button.center()));
    settle(&ctx, &mut chrome, &state);

    // One 20-point square per category heading, to the right of the palette.
    let controls = widgets(&ctx, |rect| {
        approx(rect.width(), 20.0)
            && approx(rect.height(), 20.0)
            && rect.left() > shape_button.right()
    });
    assert_eq!(controls.len(), 2, "one per category: {controls:?}");

    let output = frame(&ctx, &mut chrome, &state, click_at(controls[0].center()));
    assert!(output.events.is_empty(), "opening the popover changes nothing yet");
    settle(&ctx, &mut chrome, &state);

    // The popover's swatches are 18-point squares and nothing else in the flyout is.
    let swatches = widgets(&ctx, |rect| {
        approx(rect.width(), 18.0) && approx(rect.height(), 18.0)
    });
    assert!(!swatches.is_empty(), "the popover drew no swatches");
    let output = frame(&ctx, &mut chrome, &state, click_at(swatches[0].center()));
    assert!(
        output.events.iter().any(|e| matches!(e, UiEvent::ShapeColorsChanged { .. })),
        "{:?}",
        output.events
    );
}

/// The menu bar, end to end: the tree is Miro's, and clicking a row in it emits the
/// command that row names.
#[test]
fn the_board_menu_opens_and_its_first_row_makes_a_board() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    // ☰ offers the four groups; Board's own first row is New board.
    let groups = main_menu(&ctx, &mut chrome, &state);
    assert_eq!(groups.len(), 4, "the ☰ menu drew {} group rows", groups.len());

    let rows = group_rows(&ctx, &mut chrome, &state, 0);
    assert!(rows.len() > 4, "Board drew {} rows", rows.len());
    let output = frame(&ctx, &mut chrome, &state, click_at(rows[0].center()));
    assert_eq!(output.events, vec![UiEvent::Command(Command::NewBoard)]);
}

/// The rule the whole `Availability` type exists for: a row that cannot act is
/// disabled, and a click that lands on it does nothing rather than emitting a command
/// the app has to recognise and reject.
///
/// Disabled widgets are absent from `interactive_rects_last_pass` entirely, which is
/// what makes this checkable without hard-coding a row's position: the gap between two
/// interactive rows *is* the disabled rows.
#[test]
fn a_disabled_menu_row_cannot_be_clicked_while_its_neighbours_can() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    // A board with something to undo, something to paste, and nothing selected — so
    // Cut, Copy, Duplicate and Delete are all withdrawn.
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    // Edit is where Cut/Copy/Duplicate/Delete withdraw with nothing selected.
    let rows = group_rows(&ctx, &mut chrome, &state, 1);
    assert!(rows.len() >= 2, "Edit drew no rows: {rows:?}");
    let gap = rows
        .windows(2)
        .map(|pair| (pair[0].bottom(), pair[1].top()))
        // Adjacent rows are one item-spacing apart; anything wider is disabled rows.
        .find(|(bottom, top)| top - bottom > 40.0)
        .expect("some entry in the Edit menu is disabled with nothing selected");
    let dead = Pos2::new(rows[0].center().x, (gap.0 + gap.1) / 2.0);

    let output = frame(&ctx, &mut chrome, &state, click_at(dead));
    assert!(output.events.is_empty(), "a disabled row acted: {:?}", output.events);

    // …and a selection brings those same rows back, which is what proves the gap was
    // enablement rather than a layout accident.
    let selection = [sticky()];
    let selected = board_state(&selection);
    settle(&ctx, &mut chrome, &selected);
    let with_selection = group_rows(&ctx, &mut chrome, &selected, 1);
    assert!(
        with_selection.len() > rows.len(),
        "selecting something offered no further commands"
    );
}

/// A sticky note, for the tests that need a selection.
fn sticky() -> vellum_ui::SelectionItem {
    use vellum_ui::{ItemFacet, Placement, SelectionItem};
    SelectionItem::new(
        "1@1".parse().expect("well-formed item id"),
        ItemFacet::Sticky,
        Placement::new(0.0, 0.0, 200.0, 200.0),
    )
}

/// `docs/05-design-language.md` §3a asks for an in-app translucency override. It is in
/// Preferences, it is ticked, and the chrome applies it to itself.
/// *"in the settings i want you to have an option to the previous red and also blue"* —
/// driven through the real menu, because that is the only way the user reaches it.
///
/// The assertion is on the **palette**, not on the event. An event says a row was clicked;
/// the palette is whether anything happened, and those came apart once already in this
/// crate — the padlock that emitted `Locked(true)` into an arm that discarded it.
#[test]
fn picking_an_accent_in_preferences_repaints_the_interface() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    let before = chrome.palette().accent;
    assert_eq!(chrome.accent(), vellum_ui::Accent::Teal, "the default the user chose");

    // Preferences is the last of the four groups, so its rows are the tail of the flat
    // list, and Accent colour is the fifth of them.
    //
    // Opened **once**. `main_menu` clicks the ☰, which is a toggle, so calling it twice
    // shut the menu again and left `menu_rows` with nothing to report — the tail slice
    // then underflowed rather than failing on the assertion it was written for.
    let rows = group_rows(&ctx, &mut chrome, &state, 3);
    assert_eq!(rows.len(), PREFERENCES_ROWS, "the Preferences group: {rows:?}");
    let _ = frame(&ctx, &mut chrome, &state, click_at(rows[4].center()));
    settle(&ctx, &mut chrome, &state);

    // Teal · Red · Blue, in `Accent::ALL` order. Filtered to the submenu's own column:
    // `menu_rows` reports every row on screen, and the Preferences menu is still open
    // beside the flyout it opened — which is what a submenu looks like.
    let parent_right = rows[4].right();
    let mut colours: Vec<egui::Rect> =
        menu_rows(&ctx).into_iter().filter(|r| r.left() > parent_right).collect();
    colours.sort_by(|a, b| a.top().total_cmp(&b.top()));
    assert_eq!(colours.len(), 3, "the accent submenu: {colours:?}");
    let output = frame(&ctx, &mut chrome, &state, click_at(colours[1].center()));
    assert_eq!(output.events, vec![UiEvent::AccentChanged(vellum_ui::Accent::Red)]);

    assert_eq!(chrome.accent(), vellum_ui::Accent::Red, "the chrome applies its own choice");
    let after = chrome.palette();
    assert_ne!(after.accent, before, "the palette did not move");
    assert_eq!(after.accent, vellum_ui::Accent::Red.swatch());
    // The tint and its ink came with it. Moving the accent alone is how the two "you are
    // here" indicators go illegible, which is why `with_accent` exists at all.
    assert_ne!(after.accent_soft, vellum_ui::theme::Palette::LIGHT.accent_soft);
    assert_ne!(after.on_accent_soft, vellum_ui::theme::Palette::LIGHT.on_accent_soft);
    // …and nothing else did.
    assert_eq!(after.surface, vellum_ui::theme::Palette::LIGHT.surface);
    assert_eq!(after.canvas, vellum_ui::theme::Palette::LIGHT.canvas);
}

#[test]
fn the_preferences_menu_switches_the_material_off() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);
    assert!(chrome.translucency());

    // Translucent chrome · Align objects · Fetch link previews · Transparency ▸ ·
    // Accent colour ▸ · Keyboard shortcuts · Documentation · About. No Appearance submenu: the app
    // is light only — *"i want only light mode"* — but the accent *is* a choice now, which
    // is a different question from which palette the app is in.
    let rows = group_rows(&ctx, &mut chrome, &state, 3);
    assert_eq!(rows.len(), PREFERENCES_ROWS, "the Preferences group: {rows:?}");

    let output = frame(&ctx, &mut chrome, &state, click_at(rows[0].center()));
    assert_eq!(output.events, vec![UiEvent::Command(Command::ToggleTranslucency)]);
    assert!(!chrome.translucency(), "the chrome applies its own preference");

    // …and with the material off there is nothing for the renderer to blur.
    settle(&ctx, &mut chrome, &state);
    assert!(chrome.glass_surfaces().is_empty(), "{:?}", chrome.glass_surfaces());
}

/// Filing a board is the fifth verb Spaces needs, and the only one that happens on a
/// board rather than on the space. It is on the row the user is already pointing at.
#[test]
fn a_board_can_be_moved_into_a_space_from_its_context_menu() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [board("/boards/site-plan.vellum", "Site plan")];
    chrome.set_spaces(vec![Space::new("Cars", []), Space::new("Books", [])]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let card = cards(&ctx)[0];
    let output = frame(&ctx, &mut chrome, &state, right_click_at(card.center()));
    assert!(output.events.is_empty(), "opening a context menu is not an action");
    settle(&ctx, &mut chrome, &state);

    // Open · Rename… · Star · Move to ▸ · Duplicate · Delete…
    let rows = menu_rows(&ctx);
    assert_eq!(rows.len(), 6, "the board context menu: {rows:?}");

    // A submenu opens on hover, not on click: clicking a row closes the menu it is
    // in, which is what every other row is for.
    let _ = frame(&ctx, &mut chrome, &state, hover_at(rows[3].center()));
    settle(&ctx, &mut chrome, &state);
    // The submenu opens beside the menu it hangs off.
    // The submenu opens beside the menu it hangs off, so its rows are the ones to the
    // right of the parent's. *No space* is not among them: the board is not in one, and
    // a row that would do nothing is disabled — which also takes it out of egui's
    // interactive set, so this count is itself the enablement rule being observed.
    let spaces = widgets(&ctx, |rect| {
        approx(rect.height(), 24.0)
            && rect.left() > rows[3].right()
            && rect.top() >= rows[3].top()
    });
    assert_eq!(spaces.len(), 2, "one row per space: {spaces:?}");

    let output = frame(&ctx, &mut chrome, &state, click_at(spaces[1].center()));
    assert_eq!(
        output.events,
        vec![UiEvent::Library(LibraryEvent::MoveToSpace {
            path: "/boards/site-plan.vellum".into(),
            space: Some("Books".to_owned()),
        })]
    );
}

// ----- what the library *paints* --------------------------------------------------------
//
// The two tests below assert on the paint list rather than on emitted events, because both
// defects they pin were purely visual: the widget was found, the click worked, and what was
// drawn was wrong. `interactive_rects_last_pass` cannot see either of them.

/// Every rectangle painted in the frame, in paint order, as `(index, rect, fill, stroke)`.
///
/// Flattened out of `FullOutput::shapes`, which is the tessellator's input — so this is what
/// the renderer will draw, not a reconstruction of it. The index is the useful part for
/// anything about *covering*: later shapes are drawn on top.
fn painted_rects(full: &egui::FullOutput) -> Vec<(usize, Rect, egui::Color32, egui::Color32)> {
    full.shapes
        .iter()
        .enumerate()
        .filter_map(|(i, clipped)| match &clipped.shape {
            egui::Shape::Rect(r) => Some((i, r.rect, r.fill, r.stroke.color)),
            _ => None,
        })
        .collect()
}

/// Where each run of text was painted, in the same paint order.
fn painted_text(full: &egui::FullOutput) -> Vec<(usize, String, Rect)> {
    full.shapes
        .iter()
        .enumerate()
        .filter_map(|(i, clipped)| match &clipped.shape {
            egui::Shape::Text(text) => {
                Some((i, text.galley.text().to_owned(), text.galley.rect.translate(text.pos.to_vec2())))
            }
            _ => None,
        })
        .collect()
}

/// Runs one pass and hands back the paint list with it.
fn frame_painted(
    ctx: &Context,
    chrome: &mut Chrome,
    state: &ChromeState<'_>,
    raw: RawInput,
) -> egui::FullOutput {
    ctx.run_ui(raw, |ui| {
        let _ = chrome.show(ui, state);
    })
}

/// The hover ring has to land **on** the card's own edge.
///
/// *"when i hover over it doesnt select properly."* The ring was painted on the card's
/// *interaction* rect — the fixed slot the grid allocates — while the card's frame wrapped its
/// content and was shorter than that. So hovering drew a cyan box standing off the card, with a
/// gap below the metadata row, which reads as a misdrawn rectangle rather than as a highlight.
///
/// Asserted against the card's own painted frame rather than against a hard-coded size, so a
/// change to the card's padding or its content cannot make this pass while the ring drifts.
#[test]
fn hovering_a_board_card_rings_the_card_and_not_its_slot() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [
        board("/boards/a.vellum", "Alpha"),
        board("/boards/b.vellum", "Beta"),
        board("/boards/c.vellum", "Gamma"),
    ];
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let grid = cards(&ctx);
    assert_eq!(grid.len(), boards.len(), "one card per board: {grid:?}");
    let slot = grid[1];

    // Hovered near the top, over the preview, so the pointer is unambiguously on the card.
    let full = frame_painted(
        &ctx,
        &mut chrome,
        &state,
        hover_at(Pos2::new(slot.center().x, slot.top() + 20.0)),
    );
    let rects = painted_rects(&full);

    // The ring: the only card-sized rect stroked in the info colour.
    let info = vellum_ui::theme::Palette::LIGHT.info;
    let rings: Vec<Rect> = rects
        .iter()
        .filter(|(_, rect, _, stroke)| *stroke == info && rect.width() > 200.0)
        .map(|(_, rect, ..)| *rect)
        .collect();
    assert_eq!(rings.len(), 1, "exactly one card is ringed: {rings:?}");
    let ring = rings[0];

    // It is the *hovered* card that is ringed, and not a neighbour.
    assert!(ring.center().x > slot.left() && ring.center().x < slot.right(), "{ring:?} vs {slot:?}");

    // The ring is on the card, and the card **is** its slot now, so the two agree exactly.
    // This assertion used to be the opposite — `abs() > 1.0`, that the ring must *not* be the
    // slot — because a card was a wrapped `Frame` 17 points shorter than the slot the grid
    // allocated for it, and a ring on the slot stood off the card's bottom edge. Organising
    // the grid removed that gap at the source: `card` paints its frame at the slot, so a card
    // cannot be a different rectangle from the thing it is hovered by.
    //
    // The inversion is deliberate and it is still a real assertion — it is the one that fails
    // if the ring is ever drawn on the content rect inside the frame's padding, or outset
    // around it.
    assert!(
        (ring.height() - slot.height()).abs() < 1.0 && (ring.width() - slot.width()).abs() < 1.0,
        "the ring ({ring:?}) is not the card's own edge ({slot:?})"
    );

    // And it coincides with a surface-filled rect of the same width — the card's own frame,
    // which is the edge the user sees.
    let surface = vellum_ui::theme::Palette::LIGHT.surface;
    let frames: Vec<Rect> = rects
        .iter()
        .filter(|(_, rect, fill, _)| {
            *fill == surface && (rect.width() - ring.width()).abs() < 1.0
        })
        .map(|(_, rect, ..)| *rect)
        .collect();
    assert!(
        frames.iter().any(|frame| {
            (frame.top() - ring.top()).abs() < 1.0 && (frame.bottom() - ring.bottom()).abs() < 1.0
        }),
        "the ring at {ring:?} is on no card frame: {frames:?}"
    );
}

/// A space's name stays readable while a board is dragged onto it.
///
/// *"when i am dragging a board to a space the name of the space dissapears."* The drop
/// highlight was painted onto its own `Order::Middle` layer, because a card's painter is
/// clipped to the central panel and could not otherwise reach the sidebar. That layer is drawn
/// **after** the panel every row lives in, so the tint went over the very name it was pointing
/// at — and no choice of layer fixes it, since a panel's background and its text are the same
/// layer.
///
/// So the assertion is about paint *order*: the highlight has to be painted before the label,
/// which is only possible if the row paints it itself.
#[test]
fn dragging_a_board_onto_a_space_does_not_paint_over_its_name() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [board("/boards/site-plan.vellum", "Site plan")];
    chrome.set_spaces(vec![Space::new("Cars", []), Space::new("Books", [])]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let grid = cards(&ctx);
    assert_eq!(grid.len(), 1, "one card: {grid:?}");

    // The space rows sit in the sidebar, left of the grid. `Books` is the last of them.
    let rows = widgets(&ctx, |rect| {
        rect.right() < grid[0].left() && rect.width() > 100.0 && rect.height() > 20.0
    });
    assert!(rows.len() >= 2, "the sidebar drew {} candidate rows", rows.len());
    let target = rows[rows.len() - 1].center();

    // Press on the card, then drag onto the space — and hold there, which is the state being
    // described. The release is deliberately not sent: the highlight is a mid-drag thing.
    let _ = frame(&ctx, &mut chrome, &state, press_at(grid[0].center()));
    let full = frame_painted(&ctx, &mut chrome, &state, drag_to(target));

    // The highlight is painted, in the row.
    let info_soft = vellum_ui::theme::Palette::LIGHT.info_soft;
    let highlight = painted_rects(&full)
        .into_iter()
        .find(|(_, rect, fill, _)| *fill == info_soft && rect.contains(target));
    let Some((highlight_at, row, _, _)) = highlight else {
        panic!("no drop highlight was painted on the row under the pointer");
    };

    // …and every piece of text **inside that row's own rectangle** is painted after it, so the
    // tint is behind the name rather than over it. Filtered by the highlight's rect rather than
    // by distance: rows are 24pt apart, and a radius wide enough to be safe catches the
    // neighbour's board count, which is painted earlier and is not evidence of anything.
    let labels: Vec<(usize, String, Rect)> = painted_text(&full)
        .into_iter()
        .filter(|(_, _, rect)| row.contains(rect.center()))
        .collect();
    assert!(
        labels.iter().any(|(_, text, _)| text == "Books"),
        "the highlighted row is not the one holding the space's name: {labels:?}"
    );
    for (at, text, _) in &labels {
        assert!(
            *at > highlight_at,
            "{text:?} is painted at {at} and the highlight at {highlight_at}, so the \
             highlight covers it"
        );
    }
}

// ---------------------------------------------------------------------------
// The floating selection chrome
//
// *"almost all controls appear right above the what i right clicked … i can right
// click and i get all of these options"*. These are the two halves of that, driven
// through real pointer input rather than by calling into either module — because the
// question they answer is not "does the list derive correctly" (`context_bar` and
// `context_menu` each pin that with pure tests) but "does a click at a screen position
// reach the control that is drawn there".
// ---------------------------------------------------------------------------

/// A selection somewhere in the middle of the board, with room above it for a bar.
const SELECTED: Rect = Rect {
    min: Pos2::new(600.0, 420.0),
    max: Pos2::new(840.0, 560.0),
};

/// [`sticky`], with the paint that makes the bar draw more than its two fixed
/// buttons — a fill and an opacity, which is what a real sticky reports.
fn painted_sticky() -> vellum_ui::SelectionItem {
    vellum_ui::SelectionItem {
        fill: Some(Some(vellum_ui::Color::rgb(0xFF, 0xE0, 0x66))),
        opacity: Some(1.0),
        ..sticky()
    }
}

fn selected_state<'a>(
    selection: &'a [vellum_ui::SelectionItem],
    at: Option<Rect>,
) -> ChromeState<'a> {
    ChromeState { selection_rect: at, ..board_state(selection) }
}

/// Where the bar ended up, read from egui's own record of the area rather than
/// guessed from the widgets inside it.
fn bar_rect(ctx: &Context) -> Option<Rect> {
    ctx.memory(|m| m.area_rect(egui::Id::new("velm-context-bar")))
}

/// The bar has to sit **above** the selection and not touch it. A bar that overlapped
/// the thing it belongs to would cover the very object being restyled, which is the
/// one failure mode this placement exists to avoid.
#[test]
fn the_context_bar_floats_clear_above_the_selection() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);

    let bar = bar_rect(&ctx).expect("a selection gets a bar");
    assert!(
        bar.bottom() <= SELECTED.top(),
        "the bar at {bar:?} overlaps the selection at {SELECTED:?}"
    );
    assert!(bar.width() > 0.0 && bar.height() > 0.0);
    // Centred on the selection, which is what makes it read as belonging to it.
    assert!(
        approx(bar.center().x, SELECTED.center().x),
        "the bar is centred at {} and the selection at {}",
        bar.center().x,
        SELECTED.center().x
    );
}

/// A selection tight against the top of the canvas has no room above it, so the bar
/// goes underneath instead of behind the menu bar where it cannot be clicked.
#[test]
fn the_context_bar_flips_below_a_selection_at_the_top_of_the_window() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    let high = Rect::from_min_max(Pos2::new(600.0, MENU_BOTTOM + 2.0), Pos2::new(840.0, MENU_BOTTOM + 60.0));
    let state = selected_state(&selection, Some(high));
    settle(&ctx, &mut chrome, &state);

    let bar = bar_rect(&ctx).expect("a selection gets a bar");
    assert!(bar.top() >= high.bottom(), "the bar at {bar:?} did not flip below {high:?}");
}

/// Nothing selected is no bar at all — not an empty frame floating over the board.
#[test]
fn no_selection_draws_no_context_bar() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = selected_state(&[], None);
    settle(&ctx, &mut chrome, &state);
    assert!(bar_rect(&ctx).is_none_or(|rect| !rect.is_positive()));
}

/// The whole point of the bar: a control in it must actually restyle the selection.
/// The `⋮` button is the one every configuration has, so it is what this aims at.
#[test]
fn the_more_button_on_the_bar_opens_the_context_menu() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);

    let bar = bar_rect(&ctx).expect("a selection gets a bar");
    // `⋮` is last, so it is the rightmost widget inside the bar.
    let more = widgets(&ctx, |rect| bar.contains(rect.center()))
        .into_iter()
        .max_by(|a, b| a.left().total_cmp(&b.left()))
        .expect("the bar has controls in it");

    assert!(!chrome.context_menu_open(), "nothing has opened it yet");
    let _ = frame(&ctx, &mut chrome, &state, click_at(more.center()));
    assert!(chrome.context_menu_open(), "clicking ⋮ did not open the menu");

    // …and it draws rows the next frame, rather than merely setting a flag.
    let _ = frame(&ctx, &mut chrome, &state, input());
    let rows = menu_rows(&ctx);
    assert!(rows.len() >= 6, "the menu drew {} rows", rows.len());
}

/// A link card, with an address and a preview, as the app describes one to the chrome.
fn linked_card(url: Option<&str>) -> vellum_ui::SelectionItem {
    use vellum_ui::{CardMode, ItemFacet, LinkSummary, Placement, SelectionItem};
    SelectionItem {
        link: Some(LinkSummary {
            url: url.map(str::to_owned),
            provider: Some("Example".to_owned()),
            mode: CardMode::Card,
            has_image: true,
        }),
        opacity: Some(1.0),
        ..SelectionItem::new(
            "1@1".parse().expect("well-formed item id"),
            ItemFacet::Link,
            Placement::new(0.0, 0.0, 240.0, 140.0),
        )
    }
}

/// *"add an option to copy the link with a button right here in the menu"* — a card's bar
/// carries **two** address buttons now, and they must be two.
///
/// Every control in the bar is clicked in turn, each on its own chrome, and what it emits
/// is collected. Aiming at a position instead — "the fifth widget" — would pass on a bar
/// that emitted `CopyLink` from the button drawn where *Open page* is, which is the one
/// mistake that matters here: the two sit side by side and are told apart only by an icon.
#[test]
fn a_link_cards_bar_copies_its_address_as_well_as_opening_it() {
    const URL: &str = "https://example.com/thing";

    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [linked_card(Some(URL))];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);
    let bar = bar_rect(&ctx).expect("a selected card gets a bar");
    let controls = widgets(&ctx, |rect| bar.contains(rect.center()));
    assert!(controls.len() >= 6, "the card's bar drew {} controls", controls.len());

    // What each control answers, as (copies, opens). Counting *rects* would be wrong:
    // egui registers a widget for the icon and another for the frame around it, so two
    // rects can share one button. What has to be true is about the buttons, not the
    // rectangles — some control copies and does not open, some opens and does not copy,
    // and no control does both.
    let answers: Vec<(bool, bool)> = controls
        .iter()
        .map(|control| {
            let ctx = Context::default();
            let mut chrome = Chrome::new();
            settle(&ctx, &mut chrome, &state);
            let events = frame(&ctx, &mut chrome, &state, click_at(control.center())).events;
            (
                events.contains(&UiEvent::CopyLink(URL.to_owned())),
                events.contains(&UiEvent::OpenLink(URL.to_owned())),
            )
        })
        .collect();

    assert!(answers.contains(&(true, false)), "no control copied the address: {answers:?}");
    assert!(answers.contains(&(false, true)), "no control opened the page: {answers:?}");
    assert!(
        !answers.contains(&(true, true)),
        "one control did both, so they are not two buttons: {answers:?}"
    );
}

/// A card that arrived without an address gets neither button. A copy button there would
/// put an empty string on the pasteboard over whatever the user had on it.
#[test]
fn a_card_with_no_address_offers_neither_link_button() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [linked_card(None)];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);
    let bar = bar_rect(&ctx).expect("a selected card still gets a bar");

    for control in widgets(&ctx, |rect| bar.contains(rect.center())) {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        settle(&ctx, &mut chrome, &state);
        let events = frame(&ctx, &mut chrome, &state, click_at(control.center())).events;
        assert!(
            !events.iter().any(|event| matches!(
                event,
                UiEvent::CopyLink(_) | UiEvent::OpenLink(_)
            )),
            "a control on an address-less card emitted {events:?}"
        );
    }
}

/// A row in the menu emits its command *and* closes the menu. Both halves matter: a
/// context menu that stays open after acting is one the user has to dismiss twice.
#[test]
fn a_context_menu_row_acts_and_then_closes() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);

    chrome.open_context_menu(Pos2::new(700.0, 300.0), vellum_ui::ContextTarget::Selection);
    let _ = frame(&ctx, &mut chrome, &state, input());
    let _ = frame(&ctx, &mut chrome, &state, input());

    // Copy is the first row of a selection's menu.
    let first = menu_rows(&ctx)
        .into_iter()
        .min_by(|a, b| a.top().total_cmp(&b.top()))
        .expect("the menu drew rows");
    let output = frame(&ctx, &mut chrome, &state, click_at(first.center()));

    assert!(
        output.events.contains(&UiEvent::Command(Command::Copy)),
        "the first row emitted {:?}",
        output.events
    );
    assert!(!chrome.context_menu_open(), "the menu is still open after acting");
}

/// The click that *opens* the menu is still in egui's input on the frame the menu
/// first draws, and it landed outside a rectangle that did not exist yet. Without the
/// `fresh` guard the menu opens and closes in the same frame and never appears — so
/// this asserts it survives its own opening click, then closes on the next one.
#[test]
fn the_context_menu_survives_its_own_click_and_closes_on_the_next() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);

    let at = Pos2::new(700.0, 300.0);
    chrome.open_context_menu(at, vellum_ui::ContextTarget::Selection);
    // A frame carrying a click at the very position the menu opened at — which is
    // exactly what the real right-click delivers.
    let _ = frame(&ctx, &mut chrome, &state, right_click_at(at));
    assert!(chrome.context_menu_open(), "the menu closed on the click that opened it");

    let _ = frame(&ctx, &mut chrome, &state, input());
    let _ = frame(&ctx, &mut chrome, &state, click_at(Pos2::new(200.0, 700.0)));
    assert!(!chrome.context_menu_open(), "a click on bare canvas left the menu open");
}

/// A menu about a selection that has been deleted is pointing at nothing — and
/// *Delete* is a row in that very menu, so this is reachable in one click.
#[test]
fn the_context_menu_withdraws_when_its_selection_goes() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);

    chrome.open_context_menu(Pos2::new(700.0, 300.0), vellum_ui::ContextTarget::Selection);
    let _ = frame(&ctx, &mut chrome, &state, input());
    assert!(chrome.context_menu_open());

    let empty = selected_state(&[], None);
    let _ = frame(&ctx, &mut chrome, &empty, input());
    assert!(!chrome.context_menu_open(), "the menu outlived its selection");
}

/// The panel is off by default and the toggle reaches it from both ends. Driven
/// through the real command rather than through `set_properties_open`, because the
/// path that matters is the one a menu row and `⌥⌘P` take.
#[test]
fn the_properties_panel_is_off_until_the_command_asks_for_it() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);
    assert!(!chrome.properties_open(), "the panel should start closed");

    let mut raw = input();
    raw.events.push(Event::Key {
        key: egui::Key::P,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND | egui::Modifiers::ALT,
    });
    let output = frame(&ctx, &mut chrome, &state, raw);
    assert!(
        output.events.contains(&UiEvent::Command(Command::TogglePropertiesPanel)),
        "the shortcut emitted {:?}",
        output.events
    );
    assert!(chrome.properties_open(), "the chrome did not apply its own toggle");
}

/// The board runs *underneath* the floating tool palette on purpose — `canvas_rect`
/// does not subtract it — but a **toolbar** underneath it is a row of controls that
/// cannot be clicked. Measured before the fix: a selection at the board's left edge
/// put the bar's fill swatch behind the tool column.
#[test]
fn the_context_bar_is_pushed_clear_of_the_tool_palette() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [painted_sticky()];
    // Hard against the left edge of the canvas, which is where the collision was.
    let edge = Rect::from_min_max(Pos2::new(4.0, 420.0), Pos2::new(240.0, 560.0));
    let state = selected_state(&selection, Some(edge));
    settle(&ctx, &mut chrome, &state);

    let bar = bar_rect(&ctx).expect("a selection gets a bar");
    // The palette's own area, not "the leftmost widgets" — the first version of this
    // test used the latter and measured the *bar's* fill swatch, which sits at the
    // same height and is the same size as a tool button. It failed against a bar that
    // was already correctly placed.
    let palette = ctx
        .memory(|m| m.area_rect(egui::Id::new("vellum-toolbar")))
        .expect("the tool palette drew");
    assert!(
        bar.left() >= palette.right(),
        "the bar starts at {} and the tool palette ends at {}",
        bar.left(),
        palette.right()
    );
}

// ---------------------------------------------------------------------------
// Filing a board by dragging it — what the gesture *looks like*
//
// *"when i start to drag it … a small rectangle will move and if i drop it on that
// specific space it will do like a small something to indicate that it has been
// moved"*. The gesture already worked; what it lacked was any sign that it had begun
// or that it had finished. Both halves are painted, so both are checked in the paint
// output rather than in the events.
// ---------------------------------------------------------------------------

/// The proxy, found by its size.
///
/// `ClippedShape` carries no layer id in egui 0.35, so the layer cannot be the
/// discriminator the way it is for `overlay_widgets`. Its size can be: the proxy is
/// `space::of(34)` × `space::of(7)`, and nothing else the library paints is anywhere
/// near — cards are 236 wide and sidebar rows are the full rail by 24.
fn proxy_rect(full: &egui::FullOutput) -> Option<Rect> {
    painted_rects(full)
        .into_iter()
        .map(|(_, rect, _, _)| rect)
        .find(|rect| approx(rect.width(), 136.0) && approx(rect.height(), 28.0))
}

/// Picking a card up has to look different from failing to pick it up. Before this, the
/// only difference was the cursor's shape.
#[test]
fn dragging_a_board_carries_a_proxy_that_follows_the_pointer() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [board("/boards/site-plan.vellum", "Site plan")];
    chrome.set_spaces(vec![Space::new("Cars", []), Space::new("Books", [])]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let card = cards(&ctx)[0];
    let idle = frame_painted(&ctx, &mut chrome, &state, input());
    assert!(proxy_rect(&idle).is_none(), "nothing is being dragged yet");

    let _ = frame(&ctx, &mut chrome, &state, press_at(card.center()));
    let first = Pos2::new(card.center().x + 60.0, card.center().y + 40.0);
    let moved = frame_painted(&ctx, &mut chrome, &state, drag_to(first));
    let one = proxy_rect(&moved).expect("a card in flight carries a proxy");

    // …and it *follows*: a second sample, a hundred points away, moves it by the same.
    let second = Pos2::new(first.x + 100.0, first.y);
    let again = frame_painted(&ctx, &mut chrome, &state, drag_to(second));
    let two = proxy_rect(&again).expect("the proxy is still in flight");
    assert!(
        (two.left() - one.left() - 100.0).abs() < 1.0,
        "the proxy moved from {one:?} to {two:?} for a 100pt drag"
    );

    // Small, as asked for — and clear of the pointer, so it never covers the row being
    // aimed at. That is the same failure the drop highlight had, arriving by a different
    // route.
    assert!(two.width() < 200.0 && two.height() < 40.0, "the proxy is {two:?}, not small");
    assert!(two.min.x > second.x && two.min.y > second.y, "the proxy sits on the pointer");

    let _ = frame(&ctx, &mut chrome, &state, release_at(second));
    let after = frame_painted(&ctx, &mut chrome, &state, input());
    assert!(proxy_rect(&after).is_none(), "the proxy outlived the drag");
}

/// Dropping onto a space has to say so. The flourish is painted **before** the row's
/// label, exactly as the drop highlight is, because a confirmation that covers the name
/// of the space it is confirming is the bug this file already fixed once.
///
/// **Written as an A/B, because the first version of this test had no teeth**: it looked
/// for "a wide rect over the Books row", which the sidebar's own panel background
/// satisfies, and passed unchanged with the flourish commented out. Comparing the
/// dropped frame against an otherwise identical undropped one is what makes the
/// assertion about *the flourish* rather than about the library drawing anything at all.
#[test]
fn dropping_a_board_on_a_space_flashes_that_row_under_its_name() {
    /// Rects painted over `row`, with the pointer parked well off the sidebar so no
    /// hover tint can be mistaken for the flourish.
    fn over(full: &egui::FullOutput, row: Rect) -> Vec<(usize, Rect)> {
        painted_rects(full)
            .into_iter()
            .filter(|(_, rect, ..)| row.contains(rect.center()) && rect.width() >= row.width())
            .map(|(at, rect, ..)| (at, rect))
            .collect()
    }

    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let boards = [board("/boards/site-plan.vellum", "Site plan")];
    chrome.set_spaces(vec![Space::new("Cars", []), Space::new("Books", [])]);
    let state = library_state(&boards);
    settle(&ctx, &mut chrome, &state);

    let card = cards(&ctx)[0];
    let books = space_rows(&ctx, 2)[1];
    let away = Pos2::new(900.0, 700.0);

    // The control: no drop has happened, and the pointer is nowhere near the row.
    let _ = frame(&ctx, &mut chrome, &state, hover_at(away));
    let quiet = frame_painted(&ctx, &mut chrome, &state, hover_at(away));
    let before = over(&quiet, books).len();

    // The drop, and the frame straight after it.
    let _ = drag(&ctx, &mut chrome, &state, card.center(), books.center());
    let full = frame_painted(&ctx, &mut chrome, &state, hover_at(away));
    let after = over(&full, books);

    assert!(
        after.len() > before,
        "the row painted {} rects before the drop and {} after — no flourish",
        before,
        after.len()
    );
    // **It has to stop.** That is the whole difference between this and the caret blink
    // the app deliberately does not have: a flourish drives its own repaints for a
    // third of a second and then leaves the row exactly as it found it. `egui` advances
    // its clock by one `predicted_dt` per pass, so forty passes is well past 0.45s.
    //
    // Asserted rather than assumed because the failure is invisible: a fade whose
    // progress never reaches 1.0 looks finished — it is fully transparent — while still
    // asking for a repaint sixty times a second forever.
    let mut settled = None;
    for _ in 0..40 {
        let painted = frame_painted(&ctx, &mut chrome, &state, hover_at(away));
        settled = Some(over(&painted, books).len());
    }
    assert_eq!(
        settled,
        Some(before),
        "the flourish never finished: the row is still painting extra"
    );

    // …and every one of them is **under** the label, which is the whole rule.
    let last = after.iter().map(|(at, _)| *at).max().expect("the flourish painted");
    let name: Vec<(usize, String, Rect)> = painted_text(&full)
        .into_iter()
        .filter(|(_, text, rect)| text == "Books" && books.contains(rect.center()))
        .collect();
    assert!(!name.is_empty(), "the row's own name was not painted at all");
    for (at, text, _) in &name {
        assert!(*at > last, "{text:?} is painted at {at} and the flourish at {last}");
    }
}


/// The six folded tools are still reachable, and picking one arms it.
///
/// *"put table charts kanabn and mindmap image and the connector into a smaller menu in
/// this bar"*. The risk in folding a tool away is that it becomes unreachable by pointer
/// entirely — the palette's own unit test can only see that the *lists* agree, not that
/// the button opens anything. This drives the real click path.
#[test]
fn the_more_button_opens_the_folded_tools_and_picking_one_arms_it() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let state = board_state(&[]);
    settle(&ctx, &mut chrome, &state);

    let buttons = widgets(&ctx, |rect| {
        approx(rect.width(), 40.0) && approx(rect.height(), 40.0) && rect.left() < 100.0
    });
    // Last in the column: Select, Hand | Sticky, Text, Shape, Frame | Pen, Eraser | More.
    let more = *buttons.last().expect("the palette drew no buttons");
    let output = frame(&ctx, &mut chrome, &state, click_at(more.center()));
    assert!(
        output.events.is_empty(),
        "More is not a tool and must not arm one by itself: {:?}",
        output.events
    );
    settle(&ctx, &mut chrome, &state);

    // The flyout opens beside the palette, so its rows are to the right of it.
    let mut rows: Vec<Rect> = overlay_widgets(&ctx)
        .into_iter()
        // The picker's own rows: a fixed 192 × 32 — `space::of(48)` by `space::of(8)`, the
        // same 32pt every list and the context bar answer a pointer at, raised from 24 when
        // the user compared these pop-ups against Miro's. Bounded on both axes because the
        // flyout's container rect and its "More tools" header are also interactive and
        // also to the right of the palette, and so is the status cluster in the far
        // corner — a width-only filter caught all three.
        .filter(|r| {
            r.left() > more.right() && approx(r.width(), 192.0) && approx(r.height(), 32.0)
        })
        .collect();
    rows.sort_by(|a, b| a.top().total_cmp(&b.top()));
    assert_eq!(rows.len(), Tool::OCCASIONAL.len(), "the More flyout: {rows:?}");

    // First row is Table, per `Tool::OCCASIONAL`.
    let output = frame(&ctx, &mut chrome, &state, click_at(rows[0].center()));
    assert_eq!(output.events, vec![UiEvent::ToolChanged(Tool::Table)]);
}

// ============================================================================
// The Agent Canvas layer
// ============================================================================

/// An agent node as the app describes one to the chrome: a worker, idle, inheriting
/// everything.
fn agent_node(running: bool) -> vellum_ui::SelectionItem {
    use vellum_ui::{
        AgentRules, AgentSummary, DisplayMode, ItemFacet, Placement, Provider, ProviderChoice,
        RoleKind, RuleFile, SelectionItem, WorktreeState,
    };
    let own = AgentRules::default();
    SelectionItem {
        agent: Some(AgentSummary {
            role: "Reviewer".to_owned(),
            role_kind: RoleKind::Worker,
            provider: None,
            inherited_provider: ProviderChoice::new(Provider::Claude),
            display: None,
            inherited_display: DisplayMode::Clean,
            working_dir: None,
            project_dir: Some("/tmp/project".to_owned()),
            worktree: WorktreeState::Off,
            schedule: None,
            territory: None,
            spawn_cap: 0,
            context: Vec::new(),
            running,
            rules: vellum_ui::vellum_agent::rules::resolve(
                &RuleFile::default(),
                &RuleFile::default(),
                &own,
                "Reviewer",
            ),
            own_rules: own,
            connected: Vec::new(),
            accepts_messages: true,
            voice: false,
        }),
        opacity: Some(1.0),
        ..SelectionItem::new(
            "1@1".parse().expect("well-formed item id"),
            ItemFacet::Agent,
            Placement::new(0.0, 0.0, 420.0, 300.0),
        )
    }
}

/// An agent's bar is three buttons that do three different things, driven by real clicks.
///
/// The same shape as the link card's two address buttons, and for the same reason: *Run* and
/// *Raw* sit side by side and are told apart by an icon and a word, so aiming at "the first
/// control" would pass on a bar where the wrong button emitted the right event. Every control
/// is clicked in turn, each on its own chrome, and what it answers is collected.
///
/// **Counting rects is wrong here**, as feedback 32 records: egui registers the icon and the
/// frame around it, so two rectangles can share one button. The assertions are about which
/// answers appear at all, never about how many controls there are.
#[test]
fn an_agents_bar_runs_it_switches_its_output_and_names_its_provider() {
    use vellum_ui::AgentEdit;

    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [agent_node(false)];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);
    let bar = bar_rect(&ctx).expect("a selected agent gets a bar");
    let controls = widgets(&ctx, |rect| bar.contains(rect.center()));
    assert!(controls.len() >= 4, "the agent's bar drew {} controls", controls.len());

    // (runs, switches output) per control.
    let answers: Vec<(bool, bool)> = controls
        .iter()
        .map(|control| {
            let ctx = Context::default();
            let mut chrome = Chrome::new();
            settle(&ctx, &mut chrome, &state);
            let events = frame(&ctx, &mut chrome, &state, click_at(control.center())).events;
            (
                events.contains(&UiEvent::Command(Command::RunAgent)),
                events.contains(&UiEvent::Command(Command::ToggleAgentRaw)),
            )
        })
        .collect();

    assert!(answers.contains(&(true, false)), "no control ran the agent: {answers:?}");
    assert!(answers.contains(&(false, true)), "no control switched its output: {answers:?}");
    assert!(
        !answers.contains(&(true, true)),
        "one control did both, so they are not two buttons: {answers:?}"
    );

    // Nothing on this bar may stop the agent while it is idle — the row names the verb that
    // applies, and offering both would mean one of them is always wrong.
    for control in &controls {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        settle(&ctx, &mut chrome, &state);
        let events = frame(&ctx, &mut chrome, &state, click_at(control.center())).events;
        assert!(
            !events.contains(&UiEvent::Command(Command::StopAgent)),
            "an idle agent's bar offered Stop: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(event, UiEvent::Agent(AgentEdit::Provider(_)))),
            "a plain click chose a provider: {events:?}"
        );
    }

    // …and a running one offers the other verb, from the same position.
    let running = [agent_node(true)];
    let state = selected_state(&running, Some(SELECTED));
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    settle(&ctx, &mut chrome, &state);
    let bar = bar_rect(&ctx).expect("a running agent gets a bar too");
    let stopped = widgets(&ctx, |rect| bar.contains(rect.center())).into_iter().any(|control| {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        settle(&ctx, &mut chrome, &state);
        frame(&ctx, &mut chrome, &state, click_at(control.center()))
            .events
            .contains(&UiEvent::Command(Command::StopAgent))
    });
    assert!(stopped, "a running agent's bar had no way to stop it");
}

/// An agent's right-click menu carries the agent verbs, driven through the real menu.
#[test]
fn an_agents_context_menu_offers_its_rules_and_its_schedule() {
    let ctx = Context::default();
    let mut chrome = Chrome::new();
    let selection = [agent_node(false)];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);

    chrome.open_context_menu(Pos2::new(700.0, 300.0), vellum_ui::ContextTarget::Selection);
    let _ = frame(&ctx, &mut chrome, &state, input());
    let _ = frame(&ctx, &mut chrome, &state, input());

    let rows = menu_rows(&ctx);
    assert!(rows.len() >= 8, "an agent's menu drew {} rows", rows.len());

    // Every row is clicked on its own chrome, and the four agent verbs must all be
    // reachable. Aiming at an index would pin the *order*, which is not what matters and
    // is the half of the list most likely to move.
    let mut seen: Vec<Command> = Vec::new();
    for row in rows {
        let ctx = Context::default();
        let mut chrome = Chrome::new();
        settle(&ctx, &mut chrome, &state);
        chrome.open_context_menu(Pos2::new(700.0, 300.0), vellum_ui::ContextTarget::Selection);
        let _ = frame(&ctx, &mut chrome, &state, input());
        let _ = frame(&ctx, &mut chrome, &state, input());
        for event in frame(&ctx, &mut chrome, &state, click_at(row.center())).events {
            if let UiEvent::Command(command) = event {
                seen.push(command);
            }
        }
    }

    for wanted in [
        Command::RunAgent,
        Command::ToggleAgentRaw,
        Command::EditAgentRules,
        Command::EditAgentSchedule,
    ] {
        assert!(seen.contains(&wanted), "{wanted:?} was not reachable: {seen:?}");
    }
    assert!(
        !seen.contains(&Command::StopAgent),
        "an idle agent's menu offered Stop: {seen:?}"
    );
}

/// The inspector's agent section draws, and shows the rule cascade's provenance.
///
/// A drawing test rather than an event one: what matters here is that the panel comes up at
/// all for a node kind it has never seen, and that the words *Inherited from this project*
/// reach the screen — which is feature 11's whole visible promise, and the one thing a pure
/// test of `ResolvedRules` cannot check.
#[test]
fn the_inspector_shows_where_an_agents_rules_came_from() {
    use vellum_ui::{AgentRules, RuleFile};

    let ctx = Context::default();
    let mut chrome = Chrome::new();
    chrome.set_properties_open(true);

    let own = AgentRules::default();
    let mut node = agent_node(false);
    node.agent.as_mut().expect("an agent").rules = vellum_ui::vellum_agent::rules::resolve(
        &RuleFile::parse("---\ntone: warm\n---\nHouse style."),
        &RuleFile::parse("---\nlanguage: German\n---\nProject style."),
        &own,
        "Reviewer",
    );
    let selection = [node];
    let state = selected_state(&selection, Some(SELECTED));
    settle(&ctx, &mut chrome, &state);

    let full = frame_painted(&ctx, &mut chrome, &state, input());
    let painted: Vec<String> =
        painted_text(&full).into_iter().map(|(_, text, _)| text).collect();
    assert!(painted.iter().any(|t| t.contains("Reviewer")), "the role label: {painted:?}");
    assert!(
        painted.iter().any(|t| t.contains("Inherited from this project")),
        "the provenance of the project's language never reached the screen: {painted:?}"
    );
    assert!(
        painted.iter().any(|t| t.contains("German")),
        "the value itself never reached the screen: {painted:?}"
    );
    // The provider row states who pays, which is the point of putting it on the node.
    assert!(
        painted.iter().any(|t| t.contains("subscription")),
        "the billing was never stated: {painted:?}"
    );
}
