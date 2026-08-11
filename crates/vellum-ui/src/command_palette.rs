//! The `Cmd+K` command palette.
//!
//! `docs/04-ui-reference.md` §4 records *Commands ⌘K* in Miro's own Edit menu and
//! `docs/features/README.md` §9 lists it as planned. It is drawn here rather than left
//! to the app for the reason [`crate::command`] gives: the command table is data, and
//! a palette is that data with a filter over it. Adding a command to a menu adds it to
//! this list with its shortcut and its enablement rule already attached.
//!
//! # Disabled commands are listed, not hidden
//!
//! A palette that hides what it cannot run teaches nothing: the user types "group",
//! sees no row, and concludes the feature does not exist. Listing it greyed with
//! *"Select at least two items"* answers the question that was actually being asked.
//! Only enabled rows are selectable, so Enter can never fire something inert.

use crate::command::{Command, CommandContext, Menu, format_shortcut};
use crate::event::EventSink;
use crate::theme::{Backing, GlassSurface, Palette, floating_frame, paint_glass_edge, radius, space};
use crate::widgets::search_field_for;
use egui::{Align2, Context, Id, Key, Modifiers, Order, Rect, Sense, Ui, vec2};

/// Width of the panel. Wide enough for the longest label, its trail and its shortcut
/// on one line — a palette whose rows wrap is unreadable at speed.
const WIDTH: f32 = space::of(120);

/// How far down the window the panel hangs. Clear of the menu bar, and high enough
/// that the list below it is on screen rather than centred over the canvas.
const DROP: f32 = space::of(18);

/// Height of one row.
const ROW: f32 = space::of(7);

/// The palette's own state between frames.
#[derive(Debug, Default)]
pub struct CommandPalette {
    open: bool,
    query: String,
    /// Index into the *enabled* rows of the current result list.
    cursor: usize,
    /// True for the frame it opened on. The click that opened it — on a menu row, or
    /// on nothing at all — is still in this frame's input, and without this flag the
    /// click-outside rule closes the palette on the frame it appears.
    just_opened: bool,
}

impl CommandPalette {
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Opens it, cleared. Reopening on the previous query would be a small
    /// convenience and a large surprise — the first keystroke would filter a list the
    /// user has not seen.
    pub fn open(&mut self) {
        self.open = true;
        self.just_opened = true;
        self.query.clear();
        self.cursor = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Draws it, if it is open. Returns the rectangle it occupied, for the renderer to
    /// blur the canvas behind.
    pub(crate) fn show(
        &mut self,
        ctx: &Context,
        palette: Palette,
        cmd_ctx: &CommandContext,
        events: &mut EventSink,
    ) -> Option<Rect> {
        if !self.open {
            return None;
        }

        // Read the keys *before* the field is drawn, so a `TextEdit` with focus does
        // not eat Enter and Escape on the way past. Consuming them here is also what
        // stops the board's own bindings seeing them.
        let mut step: i32 = 0;
        let (accept, mut dismiss) = ctx.input_mut(|i| {
            step += i32::from(i.consume_key(Modifiers::NONE, Key::ArrowDown));
            step -= i32::from(i.consume_key(Modifiers::NONE, Key::ArrowUp));
            (
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        // `Cmd+K` toggles rather than reopening. The chrome's own keymap is suspended
        // while a field has focus, so the binding has to be consumed here.
        if let Some(shortcut) = Command::CommandPalette.shortcut()
            && ctx.input_mut(|i| i.consume_shortcut(&shortcut))
        {
            dismiss = true;
        }

        // Enabled first, then the rest, each in table order. A stable sort keeps the
        // menu's ordering inside both halves, so the list reads the way the menus do.
        let mut rows: Vec<Command> =
            Command::ALL.iter().copied().filter(|c| self.matches(*c)).collect();
        rows.sort_by_key(|c| !c.is_enabled(cmd_ctx));
        let selectable = rows.iter().take_while(|c| c.is_enabled(cmd_ctx)).count();

        self.cursor = if selectable == 0 {
            0
        } else {
            // Wraps, because a list this short is faster to cycle than to reverse.
            (self.cursor.min(selectable - 1) as i32 + step).rem_euclid(selectable as i32) as usize
        };

        let mut chosen = None;
        let area = egui::Area::new(Id::new("vellum-command-palette"))
            .anchor(Align2::CENTER_TOP, vec2(0.0, DROP))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                let inner = floating_frame(palette).show(ui, |ui| {
                    ui.set_width(WIDTH);
                    if search_field_for(
                        ui,
                        palette,
                        Id::new("vellum-command-palette-query"),
                        "Type a command",
                        &mut self.query,
                    ) {
                        // A new query invalidates the old position, and leaving the
                        // cursor where it was would run whatever happened to land
                        // under it.
                        self.cursor = 0;
                    }
                    ctx.memory_mut(|m| {
                        m.request_focus(Id::new("vellum-command-palette-query"));
                    });

                    ui.add_space(space::UNIT);
                    if rows.is_empty() {
                        ui.label(
                            egui::RichText::new("No command by that name").color(palette.faint),
                        );
                        return;
                    }
                    egui::ScrollArea::vertical()
                        .max_height(ROW * 9.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (index, command) in rows.iter().enumerate() {
                                let enabled = index < selectable;
                                let active = enabled && index == self.cursor;
                                if row(ui, palette, *command, cmd_ctx, active) && enabled {
                                    chosen = Some(*command);
                                }
                            }
                        });
                });
                paint_glass_edge(ui.painter(), inner.response.rect, palette, Backing::Canvas);
                inner.response.rect
            });

        if accept && let Some(command) = rows.get(self.cursor).copied().filter(|_| selectable > 0) {
            chosen = Some(command);
        }
        // A click anywhere else dismisses it, the same way a menu behaves.
        let clicked_away = !self.just_opened
            && ctx.input(|i| i.pointer.any_click())
            && ctx.pointer_interact_pos().is_some_and(|p| !area.inner.contains(p));
        self.just_opened = false;

        if let Some(command) = chosen {
            self.open = false;
            events.command(command);
        } else if dismiss || clicked_away {
            self.open = false;
        }
        Some(area.inner)
    }

    /// Substring on the label and on the trail, case-insensitively.
    ///
    /// The trail is included so "export" finds the five formats and "view" finds the
    /// zoom commands — the words a user reaches for are as often the category as the
    /// action. Substring rather than fuzzy for the reason the board search gives:
    /// the vocabulary is small and the person typing it wrote it.
    fn matches(&self, command: Command) -> bool {
        let query = self.query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let (menu, sub) = command.path();
        let haystack = format!(
            "{} {} {}",
            command.label(),
            menu.title(),
            sub.unwrap_or_default()
        )
        .to_lowercase();
        query.split_whitespace().all(|word| haystack.contains(word))
    }
}

/// One result. Returns true when it was clicked.
fn row(
    ui: &mut Ui,
    palette: Palette,
    command: Command,
    cmd_ctx: &CommandContext,
    active: bool,
) -> bool {
    let available = command.availability(cmd_ctx);
    let (rect, mut response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());

    if ui.is_rect_visible(rect) {
        let corner = egui::CornerRadius::same(radius::SMALL);
        if active {
            ui.painter().rect_filled(rect, corner, palette.accent_soft);
        } else if response.hovered() && available.is_enabled() {
            ui.painter().rect_filled(rect, corner, palette.hover);
        }

        let inner = rect.shrink2(vec2(space::of(2), 0.0));
        let (label_ink, trail_ink) = if available.is_enabled() {
            (palette.text, palette.faint)
        } else {
            (palette.faint, palette.faint)
        };
        let label_end = ui
            .painter()
            .text(
                inner.left_center(),
                egui::Align2::LEFT_CENTER,
                command.label(),
                egui::FontId::proportional(crate::theme::text::BODY),
                label_ink,
            )
            .right();

        // Where the command lives, so a palette hit teaches the menu it came from.
        let (menu, sub) = command.path();
        ui.painter().text(
            egui::pos2(label_end + space::of(3), inner.center().y),
            egui::Align2::LEFT_CENTER,
            trail(menu, sub),
            egui::FontId::proportional(crate::theme::text::LABEL),
            trail_ink,
        );

        let right = match command.shortcut() {
            Some(shortcut) => ui
                .painter()
                .text(
                    inner.right_center(),
                    egui::Align2::RIGHT_CENTER,
                    format_shortcut(shortcut, cfg!(target_os = "macos")),
                    egui::FontId::monospace(crate::theme::text::NUMERIC),
                    palette.faint,
                )
                .left(),
            None => inner.right(),
        };
        // The reason, where there is one, sits where the shortcut is not.
        if let Some(why) = available.reason() {
            ui.painter().text(
                egui::pos2(right - space::of(2), inner.center().y),
                egui::Align2::RIGHT_CENTER,
                why,
                egui::FontId::proportional(crate::theme::text::LABEL),
                palette.faint,
            );
        }
    }

    if let Some(why) = available.reason() {
        response = response.on_hover_text(why);
    }
    response.clicked() && available.is_enabled()
}

fn trail(menu: Menu, sub: Option<&'static str>) -> String {
    match sub {
        Some(sub) => format!("{} · {sub}", menu.title()),
        None => menu.title().to_owned(),
    }
}

/// The glass region the palette occupied, for [`Chrome::glass_surfaces`].
///
/// [`Chrome::glass_surfaces`]: crate::Chrome::glass_surfaces
pub(crate) fn glass(palette: Palette, rect: Rect) -> Option<GlassSurface> {
    let spec = palette.glass(Backing::Canvas)?;
    rect.is_positive().then_some(GlassSurface {
        rect,
        corner_radius: f32::from(radius::LARGE),
        opacity: spec.opacity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::UiEvent;

    fn context() -> Context {
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        ctx
    }

    fn run(
        palette_state: &mut CommandPalette,
        ctx: &Context,
        cmd_ctx: &CommandContext,
        input: egui::RawInput,
    ) -> Vec<UiEvent> {
        let mut events = EventSink::default();
        let _ = ctx.run_ui(input, |ui| {
            let _ = palette_state.show(ui.ctx(), Palette::LIGHT, cmd_ctx, &mut events);
        });
        events.take()
    }

    fn open_board() -> CommandContext {
        CommandContext {
            board_open: true,
            board_saved: true,
            dirty: true,
            can_undo: true,
            selected: 2,
            ..CommandContext::default()
        }
    }

    fn key(key: Key) -> egui::RawInput {
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
        input
    }

    #[test]
    fn a_closed_palette_draws_nothing_and_reports_no_rectangle() {
        let ctx = context();
        let mut palette = CommandPalette::default();
        let mut events = EventSink::default();
        let mut rect = Some(Rect::NOTHING);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            rect = palette.show(ui.ctx(), Palette::LIGHT, &CommandContext::default(), &mut events);
        });
        assert_eq!(rect, None);
        assert!(events.is_empty());
    }

    #[test]
    fn it_opens_cleared_and_draws_without_emitting_anything() {
        let ctx = context();
        let mut palette = CommandPalette::default();
        palette.query.push_str("stale");
        palette.open();
        assert!(palette.is_open());
        assert!(palette.query.is_empty(), "a reopened palette starts blank");
        assert!(run(&mut palette, &ctx, &open_board(), egui::RawInput::default()).is_empty());
    }

    /// The query matches the label *and* the trail, which is what makes "export" a
    /// useful thing to type.
    #[test]
    fn the_query_matches_the_label_and_the_menu_it_lives_in() {
        let mut palette = CommandPalette { query: "zoom".to_owned(), ..Default::default() };
        assert!(palette.matches(Command::ZoomToFit));
        assert!(!palette.matches(Command::Group));

        palette.query = "export".to_owned();
        assert!(palette.matches(Command::ExportCsv), "the trail names the submenu");
        assert!(palette.matches(Command::ExportBackup));

        // Every word has to land, so two words narrow rather than widen.
        palette.query = "board export".to_owned();
        assert!(palette.matches(Command::ExportPng));
        assert!(!palette.matches(Command::ZoomIn));

        palette.query = "  ".to_owned();
        assert!(palette.matches(Command::About), "a blank query lists everything");
    }

    /// Escape closes it without running anything — the failure mode that matters is a
    /// palette that fires the highlighted command on the way out.
    #[test]
    fn escape_closes_it_without_running_anything() {
        let ctx = context();
        let mut palette = CommandPalette::default();
        palette.open();
        let _ = run(&mut palette, &ctx, &open_board(), egui::RawInput::default());
        let events = run(&mut palette, &ctx, &open_board(), key(Key::Escape));
        assert!(events.is_empty());
        assert!(!palette.is_open());
    }

    /// Enter runs the highlighted row, and the highlight starts on the first thing
    /// that can actually run.
    #[test]
    fn enter_runs_the_highlighted_command_and_closes() {
        let ctx = context();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "zoom to fit".to_owned();
        let _ = run(&mut palette, &ctx, &open_board(), egui::RawInput::default());

        let events = run(&mut palette, &ctx, &open_board(), key(Key::Enter));
        assert_eq!(events, vec![UiEvent::Command(Command::ZoomToFit)]);
        assert!(!palette.is_open());
    }

    /// With nothing runnable under the cursor, Enter must do nothing rather than fire
    /// the first disabled row.
    #[test]
    fn enter_does_nothing_when_every_match_is_disabled() {
        let ctx = context();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "distribute".to_owned();
        // No board at all, so both distribute commands are disabled.
        let ctx_none = CommandContext::default();
        let _ = run(&mut palette, &ctx, &ctx_none, egui::RawInput::default());
        let events = run(&mut palette, &ctx, &ctx_none, key(Key::Enter));
        assert!(events.is_empty(), "{events:?}");
    }

    /// The arrows move the highlight, and it wraps rather than sticking at the end.
    #[test]
    fn the_arrows_move_the_highlight_and_wrap() {
        let ctx = context();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "zoom".to_owned();
        let cmd_ctx = open_board();
        let _ = run(&mut palette, &ctx, &cmd_ctx, egui::RawInput::default());
        assert_eq!(palette.cursor, 0);

        let _ = run(&mut palette, &ctx, &cmd_ctx, key(Key::ArrowDown));
        assert_eq!(palette.cursor, 1);
        let _ = run(&mut palette, &ctx, &cmd_ctx, key(Key::ArrowUp));
        assert_eq!(palette.cursor, 0);
        let _ = run(&mut palette, &ctx, &cmd_ctx, key(Key::ArrowUp));
        assert!(palette.cursor > 0, "the highlight wrapped to the end");
    }

    /// It floats over the canvas, so `docs/05-design-language.md` §3a makes it glass —
    /// and a glass surface the renderer is not told about is a washed-out panel.
    #[test]
    fn an_open_palette_reports_itself_as_glass() {
        let ctx = context();
        let mut palette = CommandPalette::default();
        palette.open();
        let mut events = EventSink::default();
        let mut rect = None;
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                rect = palette.show(ui.ctx(), Palette::LIGHT, &open_board(), &mut events);
            });
        }
        let rect = rect.expect("an open palette occupies a rectangle");
        let surface = glass(Palette::LIGHT, rect).expect("glass over the canvas");
        assert!(surface.opacity < u8::MAX);
        assert_eq!(glass(Palette::LIGHT.opaque(), rect), None);
    }
}
