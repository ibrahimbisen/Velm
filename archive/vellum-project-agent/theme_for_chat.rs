//! `Palette::for_chat`, lifted out of `vellum-project/src/theme.rs` when the Agent Canvas was
//! archived on 2026-08-22.
//!
//! It was the one thing in the shared projection crate that reached `vellum_agent`, and its
//! only caller was the agent node's painter. Kept whole rather than deleted, per the archive's
//! rule: put it back beside `Palette` and restore the `vellum-agent` dependency in
//! `crates/vellum-project/Cargo.toml` to bring it back.
//!
//! ⚠ Not compiled — `archive/` is outside `members = ["crates/*"]`. This is a record.


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

// ---- the helpers `for_chat` was the only caller of, lifted with it ----

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
