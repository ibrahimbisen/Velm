//! The `Cmd+F` find bar.
//!
//! `docs/04-ui-reference.md` §4 records *Find ⌘F* in Miro's Edit menu;
//! `docs/features/README.md` §3 puts the search itself over `vellum-search`'s inverted
//! index. Those are two different jobs and this module only does the first one: it
//! owns the field, the stepper and the match readout, and reports what the user typed
//! as a [`FindEvent`]. It never touches an index or a camera.
//!
//! The count is supplied by the app through
//! [`ChromeState::find_matches`](crate::ChromeState::find_matches) rather than
//! computed here, for the same reason: the chrome cannot know how many times a word
//! appears on a board it has never read.

use crate::event::{EventSink, FindEvent, UiEvent};
use crate::icon::Icon;
use crate::theme::{
    Backing, GlassSurface, MENU_BAR_HEIGHT, Palette, floating_frame, numeric, paint_glass_edge,
    radius, space,
};
use crate::widgets::{icon_button, search_field_for};
use egui::{Align2, Context, Id, Key, Modifiers, Order, Rect, vec2};

/// Width of the bar. A board's search terms are short; this is wide enough for a
/// phrase and no wider, because it sits over the user's work.
const WIDTH: f32 = space::of(72);

/// Edge length of its buttons — the same as the status cluster's, since both are
/// places the pointer visits rather than lives.
const BUTTON: f32 = space::of(7) - 2.0;

/// The find bar's own state between frames.
#[derive(Debug, Default)]
pub struct FindBar {
    open: bool,
    query: String,
    /// See [`CommandPalette`](crate::CommandPalette): the click that opened it is
    /// still in this frame's input.
    just_opened: bool,
}

impl FindBar {
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Opens the bar, keeping whatever was last searched for.
    ///
    /// Unlike the command palette this *does* remember: `Cmd+F` twice in a row is how
    /// a user resumes a search they were in the middle of, and the field's contents
    /// are selected on focus anyway.
    pub fn open(&mut self) {
        self.open = true;
        self.just_opened = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Draws the bar, if it is open. Returns the rectangle it occupied.
    pub(crate) fn show(
        &mut self,
        ctx: &Context,
        palette: Palette,
        matches: Option<(usize, usize)>,
        events: &mut EventSink,
    ) -> Option<Rect> {
        if !self.open {
            return None;
        }

        // Consumed before the field is drawn, so a focused `TextEdit` does not eat
        // them: Enter steps forward, Shift+Enter back, Escape closes.
        //
        // Shift first, and that order is load-bearing. egui matches modifiers
        // *logically* — an extra Shift is ignored — so a bare `Enter` pattern also
        // matches `Shift+Enter`, and reading it first would step forward on both. It is
        // the same trap the command table documents, one binding smaller.
        let (mut step_previous, mut step_next, dismiss) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::SHIFT, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });

        let mut changed = false;
        let mut closed_by_button = false;
        let area = egui::Area::new(Id::new("vellum-find"))
            .anchor(Align2::CENTER_TOP, vec2(0.0, MENU_BAR_HEIGHT + space::of(3)))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                let inner = floating_frame(palette).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = space::UNIT;
                        ui.scope(|ui| {
                            ui.set_width(WIDTH);
                            changed = search_field_for(
                                ui,
                                palette,
                                Id::new("vellum-find-query"),
                                "Find on this board",
                                &mut self.query,
                            );
                        });
                        ctx.memory_mut(|m| m.request_focus(Id::new("vellum-find-query")));

                        // The readout is monospace for the reason every number in the
                        // chrome is: it changes as the user steps through matches, and
                        // proportional digits shift the buttons beside it sideways.
                        let label = match matches {
                            Some((current, total)) if total > 0 => format!("{current}/{total}"),
                            Some(_) => "0/0".to_owned(),
                            None => String::new(),
                        };
                        ui.add_sized(
                            vec2(space::of(11), BUTTON),
                            egui::Label::new(numeric(label).color(palette.faint))
                                .selectable(false),
                        );

                        let empty = matches.is_none_or(|(_, total)| total == 0);
                        for (icon, hint, back) in [
                            (Icon::ChevronUp, "Previous match", true),
                            (Icon::ChevronDown, "Next match", false),
                        ] {
                            let button = ui
                                .add_enabled_ui(!empty, |ui| {
                                    icon_button(ui, palette, icon, BUTTON, false)
                                })
                                .inner;
                            let button = if empty {
                                button.on_disabled_hover_text("Nothing to step through")
                            } else {
                                button.on_hover_text(hint)
                            };
                            if button.clicked() {
                                if back {
                                    step_previous = true;
                                } else {
                                    step_next = true;
                                }
                            }
                        }
                        if icon_button(ui, palette, Icon::Close, BUTTON, false)
                            .on_hover_text("Close")
                            .clicked()
                        {
                            closed_by_button = true;
                        }
                    });
                });
                paint_glass_edge(ui.painter(), inner.response.rect, palette, Backing::Canvas);
                inner.response.rect
            });

        let clicked_away = !self.just_opened
            && ctx.input(|i| i.pointer.any_click())
            && ctx.pointer_interact_pos().is_some_and(|p| !area.inner.contains(p));
        self.just_opened = false;

        if changed {
            events.push(UiEvent::Find(FindEvent::Query(self.query.clone())));
        }
        // Shift+Enter is also an Enter as far as egui's modifier matching goes, so the
        // more specific one wins — the same rule the keymap follows.
        if step_previous {
            events.push(UiEvent::Find(FindEvent::Previous));
        } else if step_next {
            events.push(UiEvent::Find(FindEvent::Next));
        }
        if dismiss || closed_by_button || clicked_away {
            self.open = false;
            events.push(UiEvent::Find(FindEvent::Closed));
        }
        Some(area.inner)
    }
}

/// The glass region the bar occupied, for [`Chrome::glass_surfaces`].
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

    fn context() -> Context {
        let ctx = Context::default();
        crate::theme::apply(&ctx, crate::theme::Theme::Light);
        ctx
    }

    fn run(
        bar: &mut FindBar,
        ctx: &Context,
        matches: Option<(usize, usize)>,
        input: egui::RawInput,
    ) -> Vec<UiEvent> {
        let mut events = EventSink::default();
        let _ = ctx.run_ui(input, |ui| {
            let _ = bar.show(ui.ctx(), Palette::LIGHT, matches, &mut events);
        });
        events.take()
    }

    fn key(key: Key, modifiers: Modifiers) -> egui::RawInput {
        let mut input = egui::RawInput { modifiers, ..egui::RawInput::default() };
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        });
        input
    }

    #[test]
    fn a_closed_bar_draws_nothing() {
        let ctx = context();
        let mut bar = FindBar::default();
        let mut events = EventSink::default();
        let mut rect = Some(Rect::NOTHING);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            rect = bar.show(ui.ctx(), Palette::LIGHT, None, &mut events);
        });
        assert_eq!(rect, None);
        assert!(events.is_empty());
    }

    #[test]
    fn an_open_bar_draws_with_and_without_matches_and_emits_nothing_untouched() {
        let ctx = context();
        let mut bar = FindBar::default();
        bar.open();
        for matches in [None, Some((0, 0)), Some((3, 12))] {
            let events = run(&mut bar, &ctx, matches, egui::RawInput::default());
            assert!(events.is_empty(), "{matches:?} emitted {events:?}");
        }
    }

    /// Escape closes it *and* says so, because the app is drawing a highlight that has
    /// to come down with it.
    #[test]
    fn escape_closes_the_bar_and_reports_it() {
        let ctx = context();
        let mut bar = FindBar::default();
        bar.open();
        let _ = run(&mut bar, &ctx, Some((1, 4)), egui::RawInput::default());
        let events = run(&mut bar, &ctx, Some((1, 4)), key(Key::Escape, Modifiers::NONE));
        assert_eq!(events, vec![UiEvent::Find(FindEvent::Closed)]);
        assert!(!bar.is_open());
    }

    #[test]
    fn enter_steps_forward_and_shift_enter_steps_back() {
        let ctx = context();
        let mut bar = FindBar::default();
        bar.open();
        let _ = run(&mut bar, &ctx, Some((1, 4)), egui::RawInput::default());

        let events = run(&mut bar, &ctx, Some((1, 4)), key(Key::Enter, Modifiers::NONE));
        assert_eq!(events, vec![UiEvent::Find(FindEvent::Next)]);

        let events = run(&mut bar, &ctx, Some((1, 4)), key(Key::Enter, Modifiers::SHIFT));
        assert_eq!(events, vec![UiEvent::Find(FindEvent::Previous)]);
        assert!(bar.is_open(), "stepping does not close the bar");
    }

    /// It floats over the canvas, so it is glass — and the renderer has to be told.
    #[test]
    fn an_open_bar_reports_itself_as_glass() {
        let ctx = context();
        let mut bar = FindBar::default();
        bar.open();
        let mut events = EventSink::default();
        let mut rect = None;
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                rect = bar.show(ui.ctx(), Palette::LIGHT, Some((1, 2)), &mut events);
            });
        }
        let rect = rect.expect("an open bar occupies a rectangle");
        assert!(glass(Palette::LIGHT, rect).is_some_and(|g| g.opacity < u8::MAX));
        assert_eq!(glass(Palette::LIGHT.opaque(), rect), None);
    }
}
