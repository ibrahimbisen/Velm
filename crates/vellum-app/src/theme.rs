//! The canvas palette, as tokens.
//!
//! `docs/05-design-language.md` supplies the colourway and one rule that matters
//! more than the values: *no widget may contain a hex literal*. Every colour the
//! board draws with resolves through this module, so the day dark mode lands it is
//! one table that changes rather than every call site.
//!
//! These are the **canvas** tokens only — what an item falls back to when the
//! document says nothing. Panel and chrome colours belong to `vellum-ui`, which owns
//! the egui side; duplicating them here would give the two a way to disagree.

use vellum_render::Rgba;

/// One resolved set of canvas colours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// `pearl` — the field objects sit on.
    pub canvas: Rgba,
    /// `ink` — primary text.
    pub text: Rgba,
    /// `ink-muted` — secondary text: a card's URL, a document's page count.
    pub text_muted: Rgba,
    /// `frost` — hairline borders and dividers.
    pub border: Rgba,
    /// `bone` — a surface that floats above the canvas: a card, a frame's fill.
    pub surface: Rgba,
    /// `signal-teal` — selection.
    ///
    /// Was `xr-red`. The coral kept the destructive role and gave up this one, so a
    /// selection ring and a delete no longer look alike. `vellum_ui::Palette` carries
    /// the same split for the chrome, and the two are pinned to each other by a test
    /// in that crate's `theme.rs` rather than being kept in step by memory.
    pub accent: Rgba,
    /// Miro's canonical sticky yellow, used when a note carries no colour of its own.
    pub sticky: Rgba,
    /// Default ink and connector colour.
    pub stroke: Rgba,
    /// The grid: dots, crosses or graph lines on the canvas.
    ///
    /// Its own token because the obvious reuse does not work. The grid was drawn in
    /// `border` — `frost` — which is right for a hairline on a `bone` panel and
    /// useless on the canvas: against `pearl` it is a **1.7% luminance difference**,
    /// so the grid rendered correctly and could not be seen at all.
    ///
    /// Quietly darker than the canvas: present when looked for, invisible when not.
    ///
    /// # The delta was deliberately reduced, and that reverses an earlier tuning
    ///
    /// Feedback 11 raised this to **27/24/22** per channel because the dots were then
    /// invisible at 6/255 — a real fix, and it is not being undone by accident. The user
    /// then put Miro's board beside Velm's and said *"the background color on Miro looks a
    /// lot better"*, choosing a near-white board with dots that are barely there. Miro's
    /// grid at a working zoom is a whisper; ours was a texture.
    ///
    /// # It is Miro's dot, measured - and the token is exactly what lands
    ///
    /// # Black, at the user's word
    ///
    /// *"make the dots and crosses completely black."* Asked for directly after a run of
    /// values tuned by eye — 6/255 (invisible), 27 (too loud), 13, 20, 7, 18 — every one of
    /// them an attempt to guess a middle the user could not see. Black is not a guess, and it
    /// is trivially reversible: this is the one line.
    ///
    /// It works because the *dot* is one device pixel (`GRID_DOT`, reduced to 1.0 in the same
    /// round). Ink is size times contrast, and a single black pixel every twenty-four is a
    /// texture you can navigate by rather than a wash — which is what a grid is for. If it
    /// reads as too heavy at a fitted zoom, the lever is `GRID_MIN_PIXELS`, which decides how
    /// close together the dots are allowed to get, **not** this colour.
    ///
    /// **A correction worth keeping, because it nearly became a third round of guessing.** A
    /// sample of a Velm frame appeared to show a `#E5E5E5` token compositing to `#ECECEC`, and
    /// the conclusion drawn was that a round dot one to three pixels across loses half its
    /// contrast to partial coverage - so the token was raised to 20 to compensate. It was
    /// wrong: the sampled region was over a card's artwork rather than bare board. Re-sampled
    /// on empty canvas, the dots render at **exactly** the token, to the byte.
    ///
    /// Tuning this by eye produced 6/255 (invisible), then 27 (too loud), then 13, then
    /// briefly 20. **Sample a frame over bare board and compare against Miro's 7.**
    pub grid: Rgba,
    /// A frame's fill: **opaque white**, not `surface`.
    ///
    /// Its own token because a frame has a job the other surfaces do not — marking a
    /// region off as separate from the board. With a grid now drawn by default, a
    /// frame tinted like every other panel let the grid read straight through it and
    /// stopped doing that job. White is the contrast, and the user asked for it
    /// directly: *"all frames should [have] white background so there is contrast"*.
    pub frame_fill: Rgba,
}

impl Theme {
    /// The light colourway of `docs/05-design-language.md` §1.
    pub const LIGHT: Self = Self {
        canvas: Rgba::new(0.949, 0.949, 0.949, 1.0), // #F2F2F2 — Miro's, measured
        text: Rgba::new(0.102, 0.114, 0.122, 1.0),   // #1A1D1F
        text_muted: Rgba::new(0.361, 0.396, 0.420, 1.0), // #5C656B
        border: Rgba::new(0.898, 0.918, 0.929, 1.0), // #E5EAED
        surface: Rgba::new(0.988, 0.992, 0.996, 1.0), // #FCFDFE
        accent: Rgba::new(0.0, 0.639, 0.549, 1.0),   // #00A38C
        sticky: Rgba::new(1.0, 0.969, 0.620, 1.0),   // #FFF79E
        stroke: Rgba::new(0.102, 0.114, 0.122, 1.0), // #1A1D1F
        grid: Rgba::new(0.0, 0.0, 0.0, 1.0),         // #000000
        frame_fill: Rgba::new(1.0, 1.0, 1.0, 1.0),   // #FFFFFF
    };

    /// The dark colourway. `pearl` is the canvas; panels sit *above* it, so the
    /// surface token is lighter here rather than darker — §1 is explicit that the
    /// ramp must not be mirrored mechanically.
    pub const DARK: Self = Self {
        canvas: Rgba::new(0.110, 0.122, 0.137, 1.0), // #1C1F23
        text: Rgba::new(0.961, 0.961, 0.965, 1.0),   // #F5F5F6
        text_muted: Rgba::new(0.655, 0.675, 0.702, 1.0), // #A7ACB3
        border: Rgba::new(0.227, 0.251, 0.282, 1.0), // #3A4048
        surface: Rgba::new(0.169, 0.184, 0.204, 1.0), // #2B2F34
        accent: Rgba::new(0.784, 0.063, 0.180, 1.0), // #C8102E
        sticky: Rgba::new(1.0, 0.969, 0.620, 1.0),
        stroke: Rgba::new(0.961, 0.961, 0.965, 1.0),
        // Dark is retained but unreachable (docs/05). A frame still needs to read as
        // a distinct region, so it lifts off the canvas rather than going white.
        grid: Rgba::new(0.180, 0.196, 0.220, 1.0),
        frame_fill: Rgba::new(0.204, 0.220, 0.243, 1.0),
    };

    /// The same canvas palette wearing the user's chosen accent.
    ///
    /// The chrome's [`vellum_ui::Palette::with_accent`] derives four tokens; this one moves
    /// a single field, because the board has only one use for the accent — the selection
    /// ring — and nothing is drawn *on* it that needs a matching ink.
    ///
    /// The two are kept in step by `crate::app::theme_for`, which is the one place the
    /// chrome palette and the canvas palette are chosen together.
    pub const fn with_accent(self, accent: vellum_ui::Accent) -> Self {
        // Spelled out rather than converted from `Accent::swatch`, which answers a
        // `Color32` and cannot be unpacked in a `const fn`. A test asserts the two agree,
        // which is the same join `the_canvas_tokens_agree_with_the_chrome` makes for the
        // rest of the palette — this file has been bitten once by a hand-copied constant
        // going stale in another crate.
        let accent = match accent {
            vellum_ui::Accent::Teal => Rgba::new(0.0, 0.639, 0.549, 1.0), // #00A38C
            vellum_ui::Accent::Red => Rgba::new(0.902, 0.357, 0.345, 1.0), // #E65B58
            vellum_ui::Accent::Blue => Rgba::new(0.106, 0.384, 0.910, 1.0), // #1B62E8
        };
        Self { accent, ..self }
    }

    /// The palette **one agent node's transcript** is drawn against.
    ///
    /// *"there should be themes for chats that i can adjust like claude theme or chat gpt
    /// theme or kimi theme."*
    ///
    /// # Why a whole `Theme` rather than four extra colours
    ///
    /// Everything a node paints already resolves through `draw::tone_colour` and the plate
    /// match beside it, and both take a `&Theme`. Handing those a *different* `Theme` themes
    /// the node completely — its card, its wells, its primary and muted text, its accent
    /// rail — in one substitution, with no arm anywhere learning about chat themes. The
    /// alternative is a `theme_colour_for(tone, chat_theme)` at every call site, which is the
    /// second copy of a rule this file has already been bitten by once.
    ///
    /// `ChatTheme::Velm` answers `self` unchanged, so a board that has never chosen a theme
    /// pays one compare and gets byte-identical paint.
    ///
    /// # What is *not* re-themed, and it is deliberate
    ///
    /// The status dot, the error wash and the needs-you wash keep their own constants. Those
    /// are not decoration: they are the three things that say *"this agent has stopped"*,
    /// and a theme that could recolour them is a theme that can hide a failure. `docs/05`
    /// §3a's rule — legibility beats the material — applied to a colourway.
    pub fn for_chat(self, chat: vellum_agent::ChatTheme) -> Self {
        let Some((paper, ink, muted, accent)) = chat.colours() else { return self };
        let rgb = |[r, g, b]: [u8; 3]| Rgba::from_rgb8(r, g, b);
        Self {
            // The card, and the plates that float on it.
            surface: rgb(paper),
            frame_fill: rgb(paper),
            // A well is a *recess* in the card, so it is the paper shaded rather than the
            // board's canvas colour — which on a cream theme would put a grey hole in it.
            canvas: shade(rgb(paper), WELL_SHADE),
            text: rgb(ink),
            text_muted: rgb(muted),
            accent: rgb(accent),
            // The hairline between the paper and what sits on it. Derived from the ink at a
            // fixed remove rather than kept from the board, for the reason the well is:
            // `frost` on cream is a cool line on a warm surface, and it reads as a seam.
            border: rgb(ink).with_alpha(BORDER_ALPHA),
            ..self
        }
    }

    /// The clear colour for the swapchain.
    pub fn clear_color(&self) -> wgpu::Color {
        clear_color(self.canvas)
    }


    /// The colour the canvas is actually cleared to, given the board's own choice.
    ///
    /// `None` — a board that never picked one — follows the palette, so a board made
    /// today does not freeze today's `pearl` into its document.
    pub fn canvas_color(&self, chosen: Option<vellum_doc::Color>) -> Rgba {
        chosen.map_or(self.canvas, crate::theme::convert)
    }
}

/// A canvas colour as wgpu wants it for the clear value.
pub fn clear_color(color: Rgba) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(color.r),
        g: f64::from(color.g),
        b: f64::from(color.b),
        a: 1.0,
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::LIGHT
    }
}

/// Converts a document colour into the renderer's straight RGBA.
///
/// The one conversion point between `vellum_doc::Color` (8-bit sRGB with alpha) and
/// `vellum_render::Rgba` (float, same space). Both crates are deliberately unaware
/// of each other, so this bridge lives with the palette rather than in either of
/// them.
/// One colour, a fraction of the way towards black. Alpha is untouched.
fn shade(color: Rgba, by: f32) -> Rgba {
    let keep = (1.0 - by).clamp(0.0, 1.0);
    Rgba::new(color.r * keep, color.g * keep, color.b * keep, color.a)
}

/// How much darker a well is than the paper it is cut into.
///
/// A shade of the theme's own paper rather than a separate token, so a cream theme gets a
/// warm recess and a white one a cool grey — which is what each vendor's own interface does,
/// and what stops a well reading as a hole punched through to a different design.
const WELL_SHADE: f32 = 0.045;

/// How present a themed hairline is, as the ink's alpha.
///
/// Matched by eye against `Theme::LIGHT`'s own `border` on `surface`: 4% of ink on paper is
/// the same weight of line, and derived from the ink it cannot be a cool line on a warm
/// surface the way a fixed `frost` would be.
const BORDER_ALPHA: f32 = 0.10;

pub fn convert(color: vellum_doc::Color) -> Rgba {
    Rgba::from_rgb8(color.r, color.g, color.b).with_alpha(f32::from(color.a) / 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::Color;

    #[test]
    fn a_document_colour_survives_the_trip_to_the_renderer() {
        let sticky = convert(Color::rgb(0xFF, 0xF7, 0x9E));
        assert_eq!(sticky.pack(), [0xFF, 0xF7, 0x9E, 0xFF]);
        // The token and the converted colour have to agree to the bit a display can
        // show; they are written by different routes and only meet here.
        assert_eq!(sticky.pack(), Theme::LIGHT.sticky.pack());

        let half = convert(Color::rgba(0x1A, 0x1D, 0x1F, 0x80));
        assert_eq!(half.pack(), [0x1A, 0x1D, 0x1F, 0x80]);
    }

    /// The tokens are transcribed from `docs/05-design-language.md`; a typo in one
    /// channel is invisible on screen and permanent in the file.
    #[test]
    fn the_tokens_round_trip_to_their_documented_hex() {
        assert_eq!(Theme::LIGHT.canvas.pack(), [0xF2, 0xF2, 0xF2, 0xFF]);
        assert_eq!(Theme::LIGHT.text.pack(), [0x1A, 0x1D, 0x1F, 0xFF]);
        assert_eq!(Theme::LIGHT.text_muted.pack(), [0x5C, 0x65, 0x6B, 0xFF]);
        assert_eq!(Theme::LIGHT.border.pack(), [0xE5, 0xEA, 0xED, 0xFF]);
        assert_eq!(Theme::LIGHT.surface.pack(), [0xFC, 0xFD, 0xFE, 0xFF]);
        assert_eq!(Theme::LIGHT.accent.pack(), [0x00, 0xA3, 0x8C, 0xFF]);
        assert_eq!(Theme::LIGHT.grid.pack(), [0x00, 0x00, 0x00, 0xFF]);
        assert_eq!(Theme::DARK.canvas.pack(), [0x1C, 0x1F, 0x23, 0xFF]);
        assert_eq!(Theme::DARK.surface.pack(), [0x2B, 0x2F, 0x34, 0xFF]);
        assert_eq!(Theme::DARK.accent.pack(), [0xC8, 0x10, 0x2E, 0xFF]);
    }

    /// The canvas tokens live in this crate and the chrome tokens live in
    /// `vellum-ui`, and the two are separate on purpose — but `canvas`, `surface`,
    /// `border` and `accent` are the *same four colours* seen from either side.
    ///
    /// They have drifted before: this is the join that the `locked: false` constant
    /// in `inspect.rs` did not have, and whitening the ramp touched every one of
    /// them in two files at once. Nothing on screen would show the drift — the board
    /// is drawn by wgpu and the panel over it by egui, so a one-byte disagreement
    /// reads as a seam only if you go looking for it.
    #[test]
    fn the_canvas_tokens_agree_with_the_chrome_they_sit_under() {
        use vellum_ui::theme::Palette;
        let ui = Palette::LIGHT;
        for (name, canvas, chrome) in [
            ("canvas", Theme::LIGHT.canvas, ui.canvas),
            ("surface", Theme::LIGHT.surface, ui.surface),
            ("border", Theme::LIGHT.border, ui.border),
            ("accent", Theme::LIGHT.accent, ui.accent),
        ] {
            assert_eq!(
                canvas.pack(),
                [chrome.r(), chrome.g(), chrome.b(), chrome.a()],
                "`{name}` disagrees between the board and the chrome"
            );
        }
    }

    /// Every accent Preferences offers means the same colour on the board as in the chrome.
    ///
    /// The canvas values are spelled out as floats in `with_accent` because a `Color32`
    /// cannot be unpacked in a `const fn`, so this is a hand-copied constant in another
    /// crate — the exact shape of the `locked: false` bug this file's neighbours record.
    /// The join is a test or it is nothing.
    #[test]
    fn each_accent_is_the_same_colour_on_the_board_as_in_the_chrome() {
        for accent in vellum_ui::Accent::ALL {
            let chrome = accent.swatch();
            let canvas = Theme::LIGHT.with_accent(accent).accent;
            assert_eq!(
                canvas.pack(),
                [chrome.r(), chrome.g(), chrome.b(), chrome.a()],
                "`{}` differs between the board and the chrome",
                accent.label()
            );
        }
        // …and the default really is what the palette ships with, or the loop above could
        // be comparing two copies of the same mistake.
        assert_eq!(
            Theme::LIGHT.with_accent(vellum_ui::Accent::default()).accent.pack(),
            Theme::LIGHT.accent.pack()
        );
    }

    /// The grid is **black**, and the assertion is on the relationship rather than the value.
    ///
    /// # This value has moved six times; only the last was a decision rather than a guess
    ///
    /// 6/255 (invisible, feedback 11's original fault), then 27 (too loud), then 14/12/11,
    /// then 13, briefly 20 on a mis-sampled frame, then 7 taken from Miro zoomed out, then 18.
    /// Every one of those was an attempt to guess a middle the user could not see, and each was
    /// reported back as wrong in a different direction.
    ///
    /// *"make the dots and crosses completely black"* ends that: it is unambiguous, it is the
    /// user's own call, and it is one line to reverse. **Do not split the difference again** on
    /// the strength of an older entry in the list above — if it is too heavy, the lever is
    /// `GRID_DOT`'s size or `GRID_MIN_PIXELS`' spacing, not a compromise colour.
    ///
    /// The floor is kept because it still says something: a grid that cannot be seen is the
    /// state feedback 11 rejected, and black is as far from it as the value can go.
    #[test]
    fn the_grid_is_black_against_the_board() {
        let (grid, canvas) = (Theme::LIGHT.grid.pack(), Theme::LIGHT.canvas.pack());
        let deltas: Vec<i32> = (0..3)
            .map(|i| i32::from(canvas[i]) - i32::from(grid[i]))
            .collect();
        assert_eq!(deltas, [242, 242, 242], "the grid is no longer black");
        assert!(
            deltas.iter().all(|d| *d >= 6),
            "below 6/255 the dots stop being visible at all — feedback 11's original fault"
        );
    }

    /// Panels float above the canvas in *both* modes. Mirroring the ramp would put
    /// them behind it in dark, which §1 calls out by name.
    #[test]
    fn surfaces_are_lighter_than_the_canvas_in_both_modes() {
        for theme in [Theme::LIGHT, Theme::DARK] {
            assert!(theme.surface.r > theme.canvas.r, "{theme:?}");
        }
    }

    #[test]
    fn the_clear_colour_is_the_canvas_token() {
        let clear = Theme::LIGHT.clear_color();
        assert!((clear.r - f64::from(Theme::LIGHT.canvas.r)).abs() < 1e-9);
        assert_eq!(clear.a, 1.0);
    }
}
