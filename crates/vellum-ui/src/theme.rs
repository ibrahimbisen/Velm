//! The look of the chrome: the user's colourway, and everything derived from it.
//!
//! `docs/05-design-language.md` supplies the palette and one hard constraint —
//! *"make it beautiful and don't make it look like AI design"* — stated there as a
//! list of prohibitions because those are the defaults a generated interface drifts
//! toward. This module is where that constraint is made mechanical rather than
//! remembered:
//!
//! - **[`swatch`] holds the only colour literals in the crate.** Every other value
//!   is one of those or is derived from them by [`mix`] or [`alpha`], and
//!   `tests/tokens.rs` scans `src/` and fails if a widget names a colour itself. That
//!   is not tidiness: the two cuts of the palette differ *deliberately* — the red
//!   deepens into dark so it holds instead of glowing, the cyan brightens so it
//!   stays legible — so a hex literal in a widget is a colour that is right in one
//!   mode and wrong in the other.
//! - **[`radius`] tops out at 6px.** No pillowy corners.
//! - **[`space`] is a 4px grid**, and [`text`] is four sizes with real weight
//!   contrast, because hierarchy is carried by type and hairlines rather than by
//!   slabs of contrasting grey — the neutrals are only 4% apart in luminance and
//!   that is the point.
//! - **[`numeric`] and [`tabular`] put every digit in the monospace family.**
//!   Coordinates, dimensions, the zoom percentage and hex values are read while
//!   something is being dragged, and proportional digits jitter sideways as the
//!   value changes.
//! - **[`Glass`] is the floating-chrome material**, honoured or refused by
//!   [`SystemAppearance`] rather than by a widget's opinion.
//!
//! Everything downstream reads [`Palette`] rather than `egui::Visuals`, so a widget
//! cannot reach a colour that only exists in one mode.

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Painter, Rect, RichText, Shadow, Stroke,
    TextStyle, Ui, Visuals,
};

// ---------------------------------------------------------------------------
// The colourway
// ---------------------------------------------------------------------------

/// The user's own swatch card, transcribed from `docs/05-design-language.md` §1.
///
/// The two cuts are listed separately, and the dark one is **not** derived from the
/// light one, because they are two specifications rather than one transformation:
/// `xr-red` deepens `#E65B58 → #C8102E` so it holds against a dark ground instead of
/// glowing, and `monitor-cyan` brightens `#6FD6E6 → #57E5FF` so it stays legible.
/// Same intent, different value.
///
/// A handful of entries are marked *derived*: the user's card does not name them, and
/// each one is computed from the entries that surround it so the ramp keeps its
/// spacing rather than acquiring a value someone eyeballed.
mod swatch {
    use egui::Color32;

    /// Not a palette colour. The light a physical edge catches, used only for the
    /// specular line along the top of a glass surface — see [`Glass`](super::Glass).
    pub const SPECULAR: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);

    /// Cool, technical, near-*white*, with three saturated accents held in reserve.
    ///
    /// # The neutrals moved up the ramp — by translation, not by compression
    ///
    /// *"the app is too gray make it more white ish"* — the user. Every one of the
    /// four rose by **exactly 8/255 in every channel**, which moves the whole ramp
    /// 3% closer to white and leaves the **steps between them untouched**: 5, 12 and
    /// 6 levels, the same three gaps as before.
    ///
    /// That distinction is the whole of it. The design's actual claim is *"the
    /// neutrals are only 4% apart in luminance… hierarchy comes from type, spacing
    /// and hairlines"*, and the gaps are what carry it — `frost` on `bone` is the
    /// hairline this design uses **instead of a shadow**. Reaching white by squashing
    /// the steps would have dissolved every panel edge in the app to make the app
    /// whiter, which is not what was asked for. Translating the ramp costs nothing.
    /// [`super::super::Palette`]'s own test measures both halves.
    pub mod light {
        use egui::Color32;

        /// Lightest surface — panels, menus, popovers. Was `#F4F5F6`.
        pub const BONE: Color32 = Color32::from_rgb(0xFC, 0xFD, 0xFE);
        /// The glass tint, and **the one pure white in the palette**.
        ///
        /// *"make the color of them white ffffff"*. Everything else in this ramp is a
        /// surface a hairline has to be visible against, which is why `BONE` stops two
        /// steps short of white; glass has no such duty — it is the *only* token whose
        /// neighbour is the blurred board rather than another neutral, so it can be the
        /// full value without dissolving an edge anywhere. Deliberately its own constant
        /// rather than `BONE` moved: pulling the panel ramp to `#FFFFFF` would flatten
        /// the `bone`/`milk`/`pearl` steps this design uses *instead of a shadow*.
        pub const GLASS_WHITE: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
        /// Default app background. Was `#EFF1F2`.
        pub const MILK: Color32 = Color32::from_rgb(0xF7, 0xF9, 0xFA);
        /// A raised chrome surface — the step above `BONE`'s panels. Was `#E3E6E8`.
        ///
        /// **This is no longer the canvas.** It was both until the user compared Velm's
        /// board against Miro's and said *"the background color on Miro looks a lot better"*
        /// — Miro's board is near-white where this is a distinctly blue-grey. See [`PAPER`].
        pub const PEARL: Color32 = Color32::from_rgb(0xEB, 0xEE, 0xF0);
        /// The board itself.
        ///
        /// # Why the canvas had to stop sharing `PEARL`
        ///
        /// One token served two jobs that only looked alike: a *raised* chrome surface,
        /// which sits on `MILK` and must be distinguishable from it, and the *board*, which
        /// sits behind everything and wants to be paper. Whitening the shared value would
        /// have taken `raised` to within 2/255 of the `MILK` it sits on and dissolved every
        /// raised surface in the chrome — the exact failure feedback 22 avoided by
        /// translating the ramp rather than compressing it. Splitting the token costs one
        /// constant and keeps both jobs.
        ///
        /// # Why this far and no further
        ///
        /// `#F5F7F8` leaves 7/255 to `BONE`, which is close to Miro's own board-to-card
        /// separation. It cannot go much whiter: this design draws a **hairline where other
        /// designs draw a shadow**, so a card is told apart from the board by its border
        /// (`FROST`, now 16/255 from the board rather than 6 — *more* legible than before,
        /// not less) and by 7/255 of fill. Past this the fill difference stops carrying any
        /// of it. If the board still reads grey, the next lever is a soft shadow under
        /// cards, **not** a whiter canvas.
        pub const PAPER: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF2);
        /// Borders, dividers, inset wells, disabled fills. Was `#DDE2E5`.
        pub const FROST: Color32 = Color32::from_rgb(0xE5, 0xEA, 0xED);
        /// **Primary accent** — selection, the active tool, primary buttons.
        ///
        /// *"lets come up with another color that wil help the app pop more"*. Chosen
        /// by the user from three candidates, against a whiter ramp that had left the
        /// coral looking washed out: a saturated colour needs somewhere to be
        /// saturated *against*, and at `#FCFDFD` there is more of that than there was.
        ///
        /// It is deep — luminance 0.28 against the surface's 0.98 — which is why it
        /// reads as an accent and not as decoration, and why [`super::super::Palette::on_accent`]
        /// stays `ink` rather than becoming white (white on this measures 3.2:1 and
        /// fails AA; charcoal measures 5.3:1 and passes).
        ///
        /// **Violet was ruled out before it was offered.** `docs/05` §2's first
        /// prohibition is *"No purple. No violet-to-blue gradients"* — in the user's
        /// own words — which is exactly where an accent chosen for "pop" would
        /// otherwise land. `tests/tokens.rs` enforces it whatever anyone intends.
        pub const SIGNAL_TEAL: Color32 = Color32::from_rgb(0x00, 0xA3, 0x8C);
        /// **Destructive, and only that now.**
        ///
        /// It used to be the primary accent as well — `docs/05` §1 gave `xr-red` all
        /// three roles. Handing selection and the active tool to the teal left this
        /// one job, which is the better arrangement anyway: *delete* now looks like
        /// nothing else in the interface instead of looking like *selected*.
        /// No colour was invented to do it — this is still the supplied swatch.
        pub const XR_RED: Color32 = Color32::from_rgb(0xE6, 0x5B, 0x58);
        /// Secondary accent — snap guides, hover, informational.
        pub const MONITOR_CYAN: Color32 = Color32::from_rgb(0x6F, 0xD6, 0xE6);
        /// The third option Preferences ▸ Accent offers. Not part of the user's original
        /// card — see [`super::super::Accent::Blue`] for why it is this exact value.
        pub const COBALT: Color32 = Color32::from_rgb(0x1B, 0x62, 0xE8);

        /// Primary text. Not pure black, which reads as harsh against these greys.
        pub const INK: Color32 = Color32::from_rgb(0x1A, 0x1D, 0x1F);
        /// Secondary text, labels.
        pub const INK_MUTED: Color32 = Color32::from_rgb(0x5C, 0x65, 0x6B);
        /// Placeholders, disabled text.
        pub const INK_FAINT: Color32 = Color32::from_rgb(0x8B, 0x95, 0x9B);
    }

    /// The second swatch card in the same system.
    ///
    /// Two things the user was explicit about, preserved here rather than
    /// rediscovered later: **"not too dark"** — `VOID` is near-black and is therefore
    /// *not* the background, it is reserved for recessed wells, so the app reads as
    /// charcoal rather than as a void — and **depth does not mirror**: panels are
    /// lighter than the canvas in *both* modes, because panels always float and the
    /// canvas always recedes.
    pub mod dark {
        use egui::Color32;

        /// Deepest recess — input wells, inset fields, dropdown backdrops.
        pub const VOID: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x10);
        /// Canvas backdrop; the working field.
        pub const PEARL: Color32 = Color32::from_rgb(0x1C, 0x1F, 0x23);
        /// The board. Identical to [`PEARL`] here, unlike in light mode, where the two split
        /// so the canvas could be whitened without taking `raised` with it. The dark cut is
        /// frozen — nothing can select it (`ThemePreference::resolve` always answers `Light`)
        /// — so it keeps the arrangement it had rather than inventing a value no one can see.
        pub const PAPER: Color32 = PEARL;
        /// Panels, menus, popovers — floating *above* the canvas, so lighter.
        pub const BONE: Color32 = Color32::from_rgb(0x2B, 0x2F, 0x34);
        /// Borders and dividers. Lighter than `BONE`, which is the inversion: in
        /// dark a hairline is drawn by lifting, not by darkening.
        pub const FROST: Color32 = Color32::from_rgb(0x3A, 0x40, 0x48);
        pub const INK: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF6);
        pub const INK_MUTED: Color32 = Color32::from_rgb(0xA7, 0xAC, 0xB3);
        /// Primary accent — selection, active tool.
        pub const XR_RED: Color32 = Color32::from_rgb(0xC8, 0x10, 0x2E);
        /// Secondary accent — snap guides, hover, focus.
        pub const MONITOR_CYAN: Color32 = Color32::from_rgb(0x57, 0xE5, 0xFF);

        /// *Derived.* The app background, placed where `MILK` sits in the light cut:
        /// one step back from `BONE` on the way to `PEARL`.
        pub const MILK: Color32 = super::super::mix(BONE, PEARL, 50);
        /// *Derived.* Placeholders and disabled text, between the muted ink and the
        /// hairline it must not be confused with.
        pub const INK_FAINT: Color32 = super::super::mix(INK_MUTED, FROST, 55);
    }
}

/// A gamma-space blend of two tokens, `percent` of the way from `a` to `b`.
///
/// Gamma space rather than linear, deliberately: the ramp the user supplied is
/// specified as hex values four percent apart, and mixing those in linear light
/// produces steps that do not sit where the card puts them. Every derived token in
/// this module goes through here, so a reader can recompute any of them.
pub const fn mix(a: Color32, b: Color32, percent: u8) -> Color32 {
    const fn channel(a: u8, b: u8, t: u32) -> u8 {
        ((a as u32 * (100 - t) + b as u32 * t + 50) / 100) as u8
    }
    let t = if percent > 100 { 100 } else { percent as u32 };
    Color32::from_rgba_premultiplied(
        channel(a.r(), b.r(), t),
        channel(a.g(), b.g(), t),
        channel(a.b(), b.b(), t),
        channel(a.a(), b.a(), t),
    )
}

/// An opaque token at reduced opacity, premultiplied the way `Color32` stores it.
///
/// Takes an opaque colour: `Color32` holds premultiplied bytes, so scaling one that
/// is already translucent would compound the two alphas.
pub const fn alpha(color: Color32, opacity: u8) -> Color32 {
    const fn scale(v: u8, a: u8) -> u8 {
        ((v as u32 * a as u32 + 127) / 255) as u8
    }
    Color32::from_rgba_premultiplied(
        scale(color.r(), opacity),
        scale(color.g(), opacity),
        scale(color.b(), opacity),
        opacity,
    )
}

/// WCAG contrast between two opaque colours, `1.0..=21.0`.
///
/// Lives beside the palette rather than in the tests because [`Palette::with_accent`] has to
/// *decide* with it at runtime, not merely be checked with it afterwards: which ink an accent
/// takes is a property of the accent, and three of them are user-selectable.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    fn luminance(c: Color32) -> f32 {
        let channel = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }
    let (x, y) = (luminance(a), luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// The vertex colour that leaves a texture unmodulated.
///
/// Not a colour choice — a multiplier of one. It lives here so that "no widget names
/// a colour" stays a rule with no exceptions rather than a rule with one.
pub const UNTINTED: Color32 = swatch::SPECULAR;

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// Which palette the chrome is drawn in.
///
/// Exactly two, because a palette is either the light cut or the dark one. What the
/// *user* asked for — which may be "whatever the system is doing" — is
/// [`ThemePreference`], and it resolves to one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Theme {
    #[default]
    Light,
    Dark,
}

impl Theme {
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

/// Which colour the interface's primary accent wears.
///
/// *"in the settings i want you to have an option to the previous red and also blue if i
/// want to change it in the future"* — so the accent stopped being a constant and became a
/// preference, with the colour it replaced kept as one of the choices.
///
/// **Three, not a colour picker.** Every one of these has been checked against the surfaces
/// it lands on — [`Palette::on_accent`] has to clear 4.5:1 on the accent and
/// [`Palette::on_accent_soft`] on its own tint — and an arbitrary hue cannot promise that. A
/// picker would offer pale yellow, which fails both, and the interface would quietly stop
/// being legible in the two places that say *you are here*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Accent {
    /// `signal-teal` `#00A38C`. The default, chosen by the user from three candidates.
    #[default]
    Teal,
    /// `xr-red` `#E65B58` — the accent before the teal, and still the destructive colour.
    ///
    /// Picking this puts the interface back where it was, *including* the part that was
    /// worth changing: delete and selected go back to being the same colour. That is what
    /// "the previous red" means and it is the user's to choose.
    Red,
    /// A cobalt, `#1B62E8`. The one choice here that is not already in the colourway, and
    /// the reason it is this exact value rather than a brighter one: it is the lightest blue
    /// that still carries **white** at 5.3:1, and a blue light enough to take charcoal
    /// instead does not read as an accent at all.
    Blue,
}

impl Accent {
    pub const ALL: [Self; 3] = [Self::Teal, Self::Red, Self::Blue];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Teal => "Teal",
            Self::Red => "Red",
            Self::Blue => "Blue",
        }
    }

    /// The colour itself, in the light cut.
    pub const fn swatch(self) -> Color32 {
        match self {
            Self::Teal => swatch::light::SIGNAL_TEAL,
            Self::Red => swatch::light::XR_RED,
            Self::Blue => swatch::light::COBALT,
        }
    }
}

/// What the user chose in Preferences.
///
/// Separate from [`Theme`] rather than a third variant of it: a palette must always
/// be one of two things, and "follow the system" is a *rule for picking one*, not a
/// third set of colours. Keeping them apart means no widget can be handed a theme it
/// cannot draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThemePreference {
    Light,
    Dark,
    /// Whatever the OS is doing, re-read every frame.
    #[default]
    System,
}

impl ThemePreference {
    pub const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    /// **The app is light only.** *"i want only light mode"* — the user, superseding
    /// their earlier request for a dark cut. So this resolves to [`Theme::Light`]
    /// whatever it is asked, whatever the operating system is doing, and there is no
    /// switch anywhere in the chrome that can reach the other answer.
    ///
    /// The dark tokens and this enum stay: they are two or three constants, they cost
    /// nothing to keep, and the decision may reverse. Deleting them and rebuilding
    /// them later is the expensive way round. [`Self::resolve_either`] is what a test
    /// uses to check the dark cut still holds together.
    ///
    /// Note that *Reduce Transparency* is a different setting and is still honoured —
    /// see [`Palette::resolve`].
    pub const fn resolve(self, _system: SystemAppearance) -> Theme {
        Theme::Light
    }

    /// What the preference *would* resolve to if the app offered both cuts. Not
    /// reachable from the interface; see [`Self::resolve`].
    pub const fn resolve_either(self, system: SystemAppearance) -> Theme {
        match self {
            Self::Light => Theme::Light,
            Self::Dark => Theme::Dark,
            Self::System => {
                if system.dark {
                    Theme::Dark
                } else {
                    Theme::Light
                }
            }
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::System => "System",
        }
    }
}

/// What the operating system is telling us right now.
///
/// Sampled by the app every frame and handed over, never cached here.
/// `docs/05-design-language.md` §3a is explicit that Reduce Transparency must be
/// *detected at runtime and reacted to live* — a user who turns it on mid-session
/// gets an opaque toolbar on the next frame, not the next launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemAppearance {
    /// The OS is in dark mode. Only consulted by [`ThemePreference::System`].
    pub dark: bool,
    /// macOS *Reduce Transparency* / Windows *Transparency effects = off*. Glass
    /// falls back to a fully opaque surface with no blur behind it.
    pub reduce_transparency: bool,
    /// macOS *Increase Contrast* / Windows high-contrast themes. Deepens the ink and
    /// thickens the hairlines, and — because a high-contrast user is asking for
    /// legibility over material — also switches translucency off.
    pub increase_contrast: bool,
}

impl SystemAppearance {
    /// The appearance implied by a theme alone, for a caller that has no OS reading.
    pub const fn of(theme: Theme) -> Self {
        Self { dark: theme.is_dark(), reduce_transparency: false, increase_contrast: false }
    }
}

// ---------------------------------------------------------------------------
// The palette
// ---------------------------------------------------------------------------

/// Every colour the chrome is allowed to use.
///
/// Deliberately small, and every field carries the role it plays rather than the
/// value it holds. A panel that needs a colour not in this list is a sign the design
/// has drifted, not that the list is short.
///
/// The two constants are the only place the ramp is assembled. Reading them side by
/// side is the fastest way to check the rule
/// `docs/05-design-language.md` §1 sets out: **panels float and the canvas recedes in
/// both modes**, so the ramp is not mirrored — in light the surface is the lightest
/// value in play, in dark it is a step *up* from the canvas rather than a step down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Behind everything — the board library backdrop and the gap between panels.
    pub backdrop: Color32,
    /// Panels, menus, popovers. Always lighter than [`Palette::canvas`], in both
    /// modes, because a panel always floats.
    pub surface: Color32,
    /// A card or field sitting on `surface`, one step back from it so it reads as
    /// inset rather than as a second panel.
    pub raised: Color32,
    /// The deepest recess — text fields, dropdown backdrops, the wells the user types
    /// into. `frost` in light, `void` in dark.
    pub well: Color32,
    /// The board itself. Not drawn by this crate — `vellum-render` clears to it — but
    /// it belongs to the palette, because the panels are specified *relative* to it.
    pub canvas: Color32,
    /// Hover fill for an otherwise frameless control. Carries a trace of
    /// `monitor-cyan`, which is the token `docs/05` §3 assigns to hover.
    pub hover: Color32,
    /// Pressed or checked fill for an otherwise frameless control.
    pub pressed: Color32,
    /// Hairlines: panel edges, field borders, separators. One of these does the work
    /// a shadow would otherwise be asked to do.
    pub border: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text: shortcut hints, item counts, section labels.
    pub muted: Color32,
    /// Placeholders and disabled text. Never carries meaning on its own.
    pub faint: Color32,
    /// The primary accent: `signal-teal` in light, `xr-red` in the retained dark cut.
    /// Selection, the active tool, primary buttons. If more than about 5% of the
    /// screen is wearing it, it is overused — which on a near-white ramp is easier to
    /// breach than it was, because there is nothing else competing.
    pub accent: Color32,
    /// Text and icons drawn on top of `accent`.
    pub on_accent: Color32,
    /// A tint of `accent` used to fill a selected row without shouting.
    pub accent_soft: Color32,
    /// The second accent: `monitor-cyan`. Snap guides, focus rings, informational
    /// state. Decorative by design — never the sole carrier of meaning.
    pub info: Color32,
    /// A tint of `info`, for a hovered row or an informational strip.
    pub info_soft: Color32,
    /// Destructive actions.
    ///
    /// **In light this is no longer `accent`, and that is a change of system rather
    /// than of value.** `docs/05` §1 gave `xr-red` all three roles — selection,
    /// active tool, destructive — so *delete* and *selected* wore the same colour.
    /// Moving the first two to `signal-teal` left the coral doing one job, which is
    /// strictly better: nothing else in the light interface is red, so red means one
    /// thing. No colour was invented — the coral is still the supplied swatch.
    ///
    /// The retained dark cut keeps the original arrangement; see [`Palette::DARK`].
    pub danger: Color32,
    /// *Derived.* A state that must not read as an error. Desaturated to sit in the
    /// same cool family rather than being a stock amber.
    pub warning: Color32,
    /// *Derived.* Confirmation. Same reasoning as `warning`.
    pub success: Color32,
    /// The tight drop shadow under something that genuinely floats. Already carries
    /// its alpha.
    pub shadow: Color32,
    /// The very soft ambient pass under the same thing. Two passes rather than one
    /// big blur is what stops a floating panel reading as a 2003 drop-shadow filter.
    pub ambient: Color32,
    /// The ink drawn **on top of** [`Self::accent_soft`] — a selected tool's glyph, a
    /// chosen sidebar row's label, the active row in the command palette.
    ///
    /// A separate token because the obvious answer is wrong. `accent` on `accent_soft` is
    /// the same hue on a wash of itself, which measures **2.50:1** in light and 2.0:1 in
    /// dark: below the 3:1 AA floor for a UI graphic, let alone the 4.5:1 for a label, on
    /// the two most important "you are here" indicators in the product. This is that accent
    /// pushed toward the mode's own ink until it passes, and it still reads as the accent
    /// because the hue is untouched.
    ///
    /// The figures move with the accent, which is now a preference: 2.50:1 on the teal's
    /// 20% tint, 2.73:1 on the coral's, 3.91:1 on the cobalt's. Only the last clears 3:1,
    /// and none clears 4.5:1 — which is the point, and why this token cannot be dropped
    /// whichever colour the user picks.
    pub on_accent_soft: Color32,
    /// The tint a glass surface lays over the blurred canvas behind it.
    pub glass_tint: Color32,
    /// How much of that tint, `0..=255`. Forced to 255 when the OS asks for reduced
    /// transparency, and replaceable by the supplied figure — see
    /// [`Palette::with_glass_opacity`] and the Preferences slider.
    pub glass_opacity: u8,
    /// The specular catch along the top edge of a glass surface — the 1px inner line
    /// that separates the material from a plain blur.
    pub glass_highlight: Color32,
    /// The pale square of the transparency checkerboard behind a translucent swatch.
    pub checker: Color32,
    /// The dark square of the same checkerboard.
    pub checker_alt: Color32,
    /// The colour-picker's drag handle, which has to stay visible over any hue at
    /// all, so it is near-white in *both* modes rather than following the ink.
    pub handle: Color32,
    /// The handle's outline, for the hues the handle itself disappears against.
    pub handle_edge: Color32,
    /// Whether floating chrome may be translucent at all. False under Reduce
    /// Transparency or high contrast.
    pub translucent: bool,
    /// Hairline width in points: 1, or 2 under high contrast.
    pub hairline: u8,
}

impl Palette {
    /// The thinnest tint the Preferences slider will hand out, **~28%**.
    ///
    /// Was 140 (~55%), and moved on *"increase the transparency range"*. The slider's
    /// whole track is derived from this constant, so widening the range **is** this
    /// number — `menu::transparency` needs no edit to offer the new span.
    ///
    /// `docs/05-design-language.md` §3a still holds and is the reason this is not zero:
    /// *"Legibility is non-negotiable… Legibility wins over the material, every time."*
    /// What changed is where the line sits, not that there is one. **Be honest about the
    /// trade the user asked for**: the old floor was set at the point where chrome text
    /// still passed contrast over a *busy* board, and below roughly 55% it stops doing
    /// so. So the bottom of this range is now legibly thin over a plain canvas and can be
    /// hard to read over a dense photograph — which is the user's call to make, and
    /// reversible with one drag of the slider. Zero stays out of reach, because a control
    /// that can make the toolbar wholly invisible has no way back except from memory.
    pub const MIN_GLASS_OPACITY: u8 = 72;

    /// The thickest, ~90%. Past this the blur behind is doing nothing anyone can see
    /// and the honest setting is Translucent chrome *off*, which also skips the blur
    /// pass rather than paying for one nobody can perceive.
    pub const MAX_GLASS_OPACITY: u8 = 230;

    pub const LIGHT: Self = {
        use swatch::light as c;
        Self {
            backdrop: c::MILK,
            surface: c::BONE,
            raised: c::PEARL,
            well: c::FROST,
            canvas: c::PAPER,
            // A hover that is a plain step darker reads as a grey button; a hover
            // that is pure cyan shouts. Ten percent of the way is enough to be felt
            // as cool rather than seen as blue.
            hover: mix(c::PEARL, c::MONITOR_CYAN, 12),
            pressed: mix(c::FROST, c::MONITOR_CYAN, 18),
            border: c::FROST,
            text: c::INK,
            muted: c::INK_MUTED,
            faint: c::INK_FAINT,
            accent: c::SIGNAL_TEAL,
            // Ink rather than white, and for the same reason the coral took ink
            // before it: white on `#00A38C` is 3.2:1 — enough for a large glyph and
            // not enough for a button label; charcoal on it is 5.3:1 and meets AA,
            // which `docs/05` §6 requires of text on its own surface.
            on_accent: c::INK,
            // 20%, where the coral wanted 14. Not taste — separation. `info_soft`
            // below is a tint of `monitor-cyan`, and the teal accent is 20° of hue
            // from that cyan where the coral was 160° away, so the two soft fills can
            // no longer tell themselves apart by *hue* and have to do it by **value**:
            // 202 red channel against 232. They meet in the board library, where a
            // space row can be the selected one and the drop target at the same time,
            // and two indistinguishable pale washes there would have made the drag
            // feedback that was just built say nothing at all.
            accent_soft: mix(c::BONE, c::SIGNAL_TEAL, 20),
            // 45% of the way from the teal to the ink: 5.3:1 on `accent_soft`, where
            // the teal itself is 2.5:1.
            on_accent_soft: mix(c::SIGNAL_TEAL, c::INK, 45),
            info: c::MONITOR_CYAN,
            // 14%, where it used to be 20 — the other half of the separation above.
            // The accent's tint deepened and the informational one lifted, so they
            // part in both directions rather than one meeting the other.
            info_soft: mix(c::BONE, c::MONITOR_CYAN, 14),
            danger: c::XR_RED,
            warning: Color32::from_rgb(0x8A, 0x5D, 0x0B),
            success: Color32::from_rgb(0x1F, 0x7A, 0x55),
            shadow: alpha(c::INK, 26),
            ambient: alpha(c::INK, 12),
            glass_tint: c::GLASS_WHITE,
            // **47%**, asked for twice. It began at 184 (~72%, what `docs/05` §3a
            // specified), went to 160 (~63%) when the user asked for *"slighlty more
            // translucent"*, and comes here on *"make these like they are liquid glass
            // but increase the transparency range"*. Under half means the board is the
            // louder half of the composite, which is the difference between a panel
            // that is lit by what is behind it and a frosted slab that merely admits
            // it — and it is what the phrase "liquid glass" is describing.
            //
            // The floor moved with it (`MIN_GLASS_OPACITY`), because a default at the
            // bottom of the old range would have left the slider nothing to give.
            glass_opacity: 120,
            // **Not ~14%, and that is the point.** `docs/05` §3a specifies the catch
            // by its *effect* — "what separates the Apple material from a plain
            // blur" — and a fixed white alpha cannot produce equal effect on two
            // grounds. Over the dark cut's panel, 36/255 of white lifts the top row
            // by 14/255 and is plainly visible; over this one it lifted it by 2/255
            // and was not there at all.
            //
            // **Whitening the ramp took most of the rest.** This was 150, measured at
            // a 10/255 lift on the composited toolbar. The tint is now `bone` at
            // `#FCFDFE`, which composites at 160/255 over the blurred canvas to
            // ~246 — leaving **9 levels** of headroom where there were 17. Raised to
            // the practical ceiling it recovers 8/255 of that, and it cannot do
            // better: a specular catch is a *lift toward white*, and there is barely
            // any white left to lift toward. On this ramp the hairline is doing more
            // of the separating than the catch is, which is the honest description
            // rather than a regression to chase.
            glass_highlight: alpha(swatch::SPECULAR, 230),
            checker: c::BONE,
            checker_alt: c::FROST,
            handle: c::BONE,
            handle_edge: alpha(c::INK, 110),
            translucent: true,
            hairline: 1,
        }
    };

    /// The retained dark cut — **deliberately left where it was** when the light one
    /// was whitened and given a teal accent.
    ///
    /// Nothing can select this palette (*"i want only light mode"*), and every value
    /// in it is a transcription of the second swatch card. Inventing a
    /// dark teal to keep the two cuts structurally identical would mean inventing a
    /// colour the user never gave, for a palette nothing can reach, and then
    /// maintaining it. So this one still has `accent == danger`, which is the
    /// arrangement `docs/05` §1 specifies; the light cut is where the decision moved,
    /// because the light cut is the product.
    pub const DARK: Self = {
        use swatch::dark as c;
        Self {
            backdrop: c::MILK,
            surface: c::BONE,
            raised: c::PEARL,
            well: c::VOID,
            canvas: c::PAPER,
            hover: mix(c::FROST, c::MONITOR_CYAN, 12),
            pressed: mix(c::FROST, c::MONITOR_CYAN, 22),
            border: c::FROST,
            text: c::INK,
            muted: c::INK_MUTED,
            faint: c::INK_FAINT,
            accent: c::XR_RED,
            // `#C8102E` is dark enough that the ink reads on it at 5.2:1, so the same
            // rule as the light cut gives the opposite-looking answer.
            on_accent: c::INK,
            accent_soft: mix(c::BONE, c::XR_RED, 30),
            // Toward the ink again, which in this cut means *lighter*: 5.0:1 on
            // `accent_soft`, where the red itself is 2.0:1.
            on_accent_soft: mix(c::XR_RED, c::INK, 58),
            info: c::MONITOR_CYAN,
            info_soft: mix(c::BONE, c::MONITOR_CYAN, 26),
            danger: c::XR_RED,
            warning: Color32::from_rgb(0xDF, 0xA4, 0x3C),
            success: Color32::from_rgb(0x58, 0xC2, 0x95),
            shadow: alpha(c::VOID, 150),
            ambient: alpha(c::VOID, 90),
            glass_tint: c::BONE,
            glass_opacity: 173,
            glass_highlight: alpha(swatch::SPECULAR, 36),
            checker: c::FROST,
            checker_alt: c::BONE,
            handle: c::INK,
            handle_edge: alpha(c::VOID, 150),
            translucent: true,
            hairline: 1,
        }
    };

    pub const fn of(theme: Theme) -> Self {
        match theme {
            Theme::Light => Self::LIGHT,
            Theme::Dark => Self::DARK,
        }
    }

    /// The palette a mode produces once the operating system has had its say.
    ///
    /// This is the only entry point that should reach a widget. [`Palette::of`]
    /// answers "what does the dark cut look like"; this answers "what am I allowed to
    /// draw right now", which is a different question the moment the user has turned
    /// on Reduce Transparency.
    pub const fn resolve(theme: Theme, system: SystemAppearance) -> Self {
        let palette = Self::of(theme);
        let palette = if system.reduce_transparency { palette.opaque() } else { palette };
        if system.increase_contrast { palette.high_contrast(theme) } else { palette }
    }

    /// The same palette with the user's chosen tint strength.
    ///
    /// Bounded below rather than taken literally. `docs/05-design-language.md` §3a is
    /// explicit that legibility beats the material — a panel over a busy board has to
    /// stay readable — and past a certain thinness the tint stops separating the text
    /// from whatever is behind it. [`Palette::MIN_GLASS_OPACITY`] is where that stops
    /// being true, so the slider bottoms out there instead of reaching zero.
    pub const fn with_glass_opacity(self, opacity: u8) -> Self {
        let opacity = if opacity < Self::MIN_GLASS_OPACITY {
            Self::MIN_GLASS_OPACITY
        } else {
            opacity
        };
        Self { glass_opacity: opacity, ..self }
    }

    /// The same palette wearing a different primary accent.
    ///
    /// Four tokens move together and they cannot be set independently, which is the whole
    /// reason this is a method rather than three setters: `accent` is the colour,
    /// `accent_soft` is its wash, and `on_accent`/`on_accent_soft` are the ink drawn on each
    /// of those. Changing the first and forgetting either of the last two is how the two
    /// most important "you are here" indicators in the product go illegible, which
    /// [`Palette::LIGHT`]'s own comments record having happened once already.
    ///
    /// **`on_accent` is derived rather than fixed**, and this is the part that would have
    /// been wrong if it were not. Charcoal on the teal is 5.3:1 and white is 3.2:1; on the
    /// cobalt the numbers swap — white is 5.3:1 and charcoal is 3.2:1. A single constant is
    /// right for two of the three and fails AA on the other, so this takes whichever of the
    /// mode's own ink and white actually reads, and a test measures all three.
    ///
    /// The dark cut is left alone: it is unreachable, and see [`Palette::DARK`].
    pub fn with_accent(self, accent: Accent) -> Self {
        let colour = accent.swatch();
        let ink = swatch::light::INK;
        Self {
            accent: colour,
            on_accent: if contrast(ink, colour) >= contrast(swatch::SPECULAR, colour) {
                ink
            } else {
                swatch::SPECULAR
            },
            accent_soft: mix(self.surface, colour, 20),
            on_accent_soft: mix(colour, ink, 45),
            // `danger` is deliberately **not** touched. It is the user's coral and it means
            // destruction; choosing the coral as the accent puts the interface back to one
            // colour doing both jobs, which is exactly what picking "the previous red"
            // asks for.
            ..self
        }
    }

    /// The same palette with the material switched off: a fully opaque surface and no
    /// specular edge, since there is nothing behind it to catch light.
    pub const fn opaque(self) -> Self {
        Self {
            translucent: false,
            glass_opacity: u8::MAX,
            glass_highlight: Color32::TRANSPARENT,
            ..self
        }
    }

    /// The high-contrast cut: deeper ink, hairlines at 2px, and no translucency.
    ///
    /// The neutrals stay where they are. Darkening the surfaces as well would produce
    /// a fourth palette to maintain, and the contrast that matters is text against
    /// its own background, which the ink alone controls.
    pub const fn high_contrast(self, theme: Theme) -> Self {
        let (text, border) = match theme {
            Theme::Light => (swatch::dark::VOID, swatch::light::INK_MUTED),
            Theme::Dark => (swatch::SPECULAR, swatch::dark::INK_MUTED),
        };
        Self {
            text,
            muted: mix(self.muted, text, 45),
            faint: mix(self.faint, text, 45),
            border,
            hairline: 2,
            ..self.opaque()
        }
    }

    /// Hairline width in points, as a stroke wants it.
    pub const fn hairline_width(self) -> f32 {
        self.hairline as f32
    }

    /// A one-point hairline in the border colour, the separator this design uses
    /// instead of a shadow.
    pub fn hairline_stroke(self) -> Stroke {
        Stroke::new(self.hairline_width(), self.border)
    }

    /// The glass specification for a surface sitting over `backing`, or `None` when
    /// the surface must be opaque.
    ///
    /// `None` is returned for three separate reasons and the caller does not need to
    /// know which: the OS asked for reduced transparency, the user is in high
    /// contrast, or the surface is over another panel. That last one is how
    /// *"glass never stacks"* is enforced rather than remembered — a popover opened
    /// from the properties panel asks for [`Backing::Panel`] and simply cannot get
    /// the material.
    pub const fn glass(self, backing: Backing) -> Option<Glass> {
        if !self.translucent || !matches!(backing, Backing::Canvas) {
            return None;
        }
        Some(Glass {
            tint: self.glass_tint,
            opacity: self.glass_opacity,
            highlight: self.glass_highlight,
            edge: self.border,
        })
    }

    /// The fill a floating surface takes over `backing` — the glass tint at its
    /// opacity, or the opaque surface colour.
    pub const fn floating_fill(self, backing: Backing) -> Color32 {
        match self.glass(backing) {
            Some(glass) => alpha(glass.tint, glass.opacity),
            None => self.surface,
        }
    }
}

/// What a floating surface sits over.
///
/// The distinction exists because translucency is only *honest* over the canvas:
/// `docs/05-design-language.md` §3a wants it there — you can see your own work
/// continuing under the toolbar, which for an infinite canvas is worth real money —
/// and rules it out everywhere else, including on top of another glass surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backing {
    /// The board. The user's work continues underneath, so the surface is glass.
    #[default]
    Canvas,
    /// Another panel, a docked surface, or a modal. Opaque.
    Panel,
}

/// The translucent material a floating surface is made of.
///
/// egui cannot blur its own backdrop — it has no access to what was drawn beneath it
/// — so this type expresses the *intent* and `vellum-render` supplies the blur. The
/// chrome reports where the glass is via
/// [`Chrome::glass_surfaces`](crate::Chrome::glass_surfaces); the renderer blurs a
/// quarter-resolution copy of exactly those regions and draws it behind the tint.
/// Until it does, the tint alone renders as a flat surface, which is the documented
/// fallback rather than a missing feature.
///
/// # What is not glass yet, and why
///
/// `docs/05` §3a lists menus and popovers alongside the toolbar. The surfaces this
/// crate lays out itself — the tool palette, its flyouts, the status cluster, toasts
/// — are glass and are reported. **egui's own menu dropdowns are deliberately left
/// opaque**: they are laid out inside `MenuButton`, so their rectangles never reach
/// [`Chrome::glass_surfaces`](crate::Chrome::glass_surfaces), and a translucent
/// surface the renderer has not been told to blur behind is not a material — it is a
/// washed-out panel with the board showing through the text. §3a settles that case
/// explicitly: legibility wins over the material, every time. They become glass when
/// egui can report the rectangle, not before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glass {
    /// Laid over the blurred backdrop.
    pub tint: Color32,
    /// How much of it, `0..=255`.
    pub opacity: u8,
    /// The 1px specular line along the **top edge only**. That catch is what
    /// separates a physical-feeling material from a plain blur.
    pub highlight: Color32,
    /// The 1px hairline around the whole surface.
    pub edge: Color32,
}

/// A floating surface the renderer should blur the canvas behind.
///
/// Reported per frame rather than assumed, because §3a's performance budget — under
/// 0.5ms — is only reachable by blurring the regions actually behind glass instead of
/// the whole frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassSurface {
    pub rect: Rect,
    /// So the blurred copy is masked to the same silhouette the tint is drawn in.
    pub corner_radius: f32,
    /// The tint's opacity, `0..=255`. The renderer needs it to decide how strong the
    /// blur has to be for text to stay legible against the worst-case backdrop.
    pub opacity: u8,
}

// ---------------------------------------------------------------------------
// Geometry, spacing and type
// ---------------------------------------------------------------------------

/// The radius scale.
///
/// Three steps, all inside the 4–6px band `docs/05-design-language.md` §2 allows.
/// It is not a scale that grows — it is a scale that discriminates: the difference
/// between a field and the cluster it sits in is one point, and that is enough.
/// Pillowy 16px corners are the single most recognisable tell of generated UI.
pub mod radius {
    /// Smaller than the scale proper, and used once: the 5px-wide drag handle on a
    /// colour strip, where 4 would round it into a lozenge rather than soften it.
    pub const TIGHT: u8 = 2;
    /// Fields, small buttons, swatches.
    pub const SMALL: u8 = 4;
    /// Tool buttons, cards, popovers.
    pub const MEDIUM: u8 = 5;
    /// The floating tool palette, the status cluster, dialogs.
    pub const LARGE: u8 = 6;
}

/// The 4px grid every measurement in the chrome lands on.
///
/// Dense on purpose. This is a tool someone works in all day, not a landing page, and
/// `docs/05` §2 rules out oversized whitespace by name.
pub mod space {
    /// The unit. Nothing in the chrome is spaced by a value that is not a multiple.
    pub const UNIT: f32 = 4.0;

    /// `n` grid units.
    pub const fn of(n: u8) -> f32 {
        UNIT * n as f32
    }
}

/// The type scale, `docs/05-design-language.md` §5.
///
/// Four sizes and real weight contrast, because with neutrals four percent apart it
/// is type that has to carry the hierarchy.
///
/// # The two families are the app's to supply
///
/// §5 asks for the *platform* faces — SF Pro on macOS, Segoe UI Variable on Windows,
/// with SF Mono and Cascadia Mono for numbers — because a native face beats a bundled
/// webfont both for legibility and for looking like it belongs on the machine. Font
/// registration is a whole-`Context` operation the app owns, so this crate only ever
/// names `FontFamily::Proportional` and `FontFamily::Monospace`. **The app must
/// install the platform faces into those two slots**; until it does, egui's bundled
/// fallbacks render and everything still lays out, just in the wrong voice.
pub mod text {
    /// Section labels, drawn as letterspaced caps.
    pub const LABEL: f32 = 11.0;
    /// Body and controls.
    pub const BODY: f32 = 13.0;
    /// Panel titles.
    pub const PANEL_TITLE: f32 = 15.0;
    /// Screen titles.
    pub const SCREEN_TITLE: f32 = 20.0;
    /// Numbers and hex. A point smaller than body: monospace runs wide, and a
    /// coordinate field has to fit beside its label.
    pub const NUMERIC: f32 = 12.0;
    /// UI line height, as a multiple of the size.
    ///
    /// egui has no global line-height setting — it is a per-`LayoutJob` value — so
    /// this is a constant callers apply with `RichText::line_height` where they lay
    /// out more than one line, rather than something [`apply`](super::apply) can
    /// install. Single-line labels, which is nearly all of the chrome, are unaffected
    /// either way.
    ///
    /// Canvas text uses 1.36 to match Miro; that belongs to `vellum-text`, not here.
    pub const LINE_HEIGHT: f32 = 1.4;
}

/// Height of the menu bar, and the reference for every other vertical rhythm value.
pub const MENU_BAR_HEIGHT: f32 = 36.0;

/// Height of the tab strip along the very top of the window.
///
/// **Seven grid units — a quarter shorter than a browser's.** *"alot smaller
/// footprint"*: Chrome's tab strip is about 34 points with generous padding, and
/// every point of it here is a point of board the user cannot see. Twenty-eight is
/// the floor that still fits an 11-point label, a 16-point close button and the mark
/// side by side without any of them touching.
pub const TAB_STRIP_HEIGHT: f32 = space::of(7);

/// Width of the sticky home tab: the mark, and nothing else.
///
/// *"a sticky home page that i can just go back to"* — it carries no label, so it
/// stays the same narrow square however many boards are open, and the eye learns one
/// fixed target rather than a moving one.
pub const TAB_HOME_WIDTH: f32 = space::of(9);

/// The width a board tab takes when there is room for it.
pub const TAB_MAX_WIDTH: f32 = space::of(40);

/// The width board tabs shrink to before the strip starts scrolling instead.
///
/// Below thirty units a title truncates to two or three characters and the strip
/// stops answering the question it exists to answer, so scrolling is the better
/// failure.
pub const TAB_MIN_WIDTH: f32 = space::of(30);

/// Width of the right-hand properties panel. Wide enough for a label, a colour
/// swatch and a numeric field on one row without wrapping.
pub const PROPERTIES_WIDTH: f32 = 264.0;

/// Width of the board library's sidebar, which holds Spaces.
pub const SIDEBAR_WIDTH: f32 = 192.0;

/// Edge length of a tool button in the left palette.
pub const TOOL_BUTTON: f32 = 40.0;

/// A number, in the monospace family so its digits are tabular.
///
/// Every coordinate, dimension, percentage, count and hex value in the chrome goes
/// through here or through [`tabular`]. The reason is not typographic taste: these
/// values are read *while something is being dragged*, and proportional digits change
/// width as they change value, so the whole field jitters sideways at 120fps. A
/// monospace face gives every digit the same advance, which is what "tabular figures"
/// buys — and egui exposes no OpenType feature toggles, so choosing the family *is*
/// how tabular figures are selected here.
pub fn numeric(text: impl Into<String>) -> RichText {
    RichText::new(text).family(FontFamily::Monospace).size(text::NUMERIC)
}

/// A section label: letterspaced caps, as on the supplied swatch card where
/// `COLORWAY` sits over the hex values in mono.
pub fn section_label(text: &str, palette: Palette) -> RichText {
    RichText::new(text.to_uppercase())
        .size(text::LABEL)
        .color(palette.muted)
        .extra_letter_spacing(0.7)
}

/// A panel title — the heading inside a properties panel or a flyout.
pub fn panel_title(text: impl Into<String>) -> RichText {
    RichText::new(text).size(text::PANEL_TITLE).strong()
}

/// A screen title — the one heading on the library, and a dialog's question.
pub fn screen_title(text: impl Into<String>) -> RichText {
    RichText::new(text).size(text::SCREEN_TITLE).strong()
}

/// Runs `contents` with the numeric type installed, so the widgets egui renders for
/// itself — `DragValue`, `Slider`, `Button` — draw their digits tabular too.
///
/// `DragValue` has no font of its own to set; it draws in the button text style. This
/// swaps that style for the length of the closure rather than restyling the whole
/// context, so a label beside the field keeps the interface face.
pub fn tabular<R>(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        let mono = FontId::new(text::NUMERIC, FontFamily::Monospace);
        let styles = &mut ui.style_mut().text_styles;
        styles.insert(TextStyle::Button, mono.clone());
        styles.insert(TextStyle::Body, mono);
        contents(ui)
    })
    .inner
}

// ---------------------------------------------------------------------------
// Applying it to egui
// ---------------------------------------------------------------------------

/// Applies the palette, spacing and text scale to a context.
///
/// Call once after creating the context and again whenever the appearance changes;
/// egui's style is a whole-context value, so there is no per-widget cost to this.
pub fn apply(ctx: &egui::Context, theme: Theme) {
    apply_appearance(ctx, theme, SystemAppearance::of(theme));
}

/// [`apply`], with the operating system's accessibility settings folded in.
///
/// The extra argument is what makes Reduce Transparency and Increase Contrast live
/// settings rather than launch-time ones: the app re-reads them each frame and calls
/// this again when they move.
pub fn apply_appearance(ctx: &egui::Context, theme: Theme, system: SystemAppearance) {
    apply_palette(ctx, theme, Palette::resolve(theme, system));
}

/// [`apply_appearance`], for a caller that has already resolved the palette.
///
/// This exists because [`Palette::resolve`] is **not** the whole story any more: the accent
/// is a user preference applied after it ([`Palette::with_accent`]), and a caller that
/// re-derived the palette from `(theme, system)` alone would install egui's own
/// accent-coloured `Visuals` — text-selection fill, the caret, hyperlink colour, the active
/// widget's stroke — in the *default* accent whatever the user had chosen.
///
/// That was a real defect and the symptom was oddly narrow, which is why it is worth naming:
/// everything this crate paints by hand followed the accent immediately, and only the things
/// **egui** paints for itself stayed teal — so a coral interface still highlighted dragged
/// text in mint. `Chrome::show` is the caller that matters; it passes `Chrome::palette`.
pub fn apply_palette(ctx: &egui::Context, theme: Theme, palette: Palette) {
    let slot = egui_theme(theme);
    let mut style = (*ctx.style_of(slot)).clone();

    style.text_styles = text_styles();
    style.visuals = visuals(theme, palette);

    // Everything below is a multiple of `space::UNIT`.
    let spacing = &mut style.spacing;
    spacing.item_spacing = egui::vec2(space::of(2), space::of(1));
    spacing.button_padding = egui::vec2(space::of(2), space::of(1));
    spacing.interact_size = egui::vec2(space::of(7), space::of(6));
    spacing.menu_margin = Margin::same(space::UNIT as i8);
    spacing.window_margin = Margin::same(space::of(4) as i8);
    spacing.menu_width = space::of(58);
    spacing.combo_width = space::of(33);
    spacing.slider_width = space::of(23);
    spacing.text_edit_width = space::of(40);
    spacing.icon_width = space::of(4);
    spacing.icon_width_inner = space::of(2);
    spacing.indent = space::of(4);
    spacing.scroll.bar_width = space::of(2);
    spacing.scroll.floating = true;
    spacing.scroll.foreground_color = false;

    // Tooltips carry the shortcut hints on the tool palette, so they need to appear
    // promptly enough to be an answer rather than an interruption.
    style.interaction.tooltip_delay = 0.35;
    style.interaction.show_tooltips_only_when_still = true;
    // 120–160ms, ease-out, per §3. egui animates over a single duration, so this is
    // the low end of that band: panels settle, nothing bounces.
    style.animation_time = 0.12;

    // egui keeps a style per theme and picks between them; writing only the slot we
    // are switching to and then selecting it keeps the two in step, and means a
    // context that was already dark does not repaint through a light frame.
    ctx.set_style_of(slot, style);
    ctx.set_theme(slot);
}

const fn egui_theme(theme: Theme) -> egui::Theme {
    match theme {
        Theme::Light => egui::Theme::Light,
        Theme::Dark => egui::Theme::Dark,
    }
}

fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    use FontFamily::{Monospace, Proportional};
    [
        (TextStyle::Small, FontId::new(text::LABEL, Proportional)),
        (TextStyle::Body, FontId::new(text::BODY, Proportional)),
        (TextStyle::Button, FontId::new(text::BODY, Proportional)),
        (TextStyle::Heading, FontId::new(text::SCREEN_TITLE, Proportional)),
        (TextStyle::Monospace, FontId::new(text::NUMERIC, Monospace)),
    ]
    .into()
}

fn visuals(theme: Theme, palette: Palette) -> Visuals {
    let mut visuals = if theme.is_dark() { Visuals::dark() } else { Visuals::light() };
    let hairline = palette.hairline_stroke();

    visuals.dark_mode = theme.is_dark();
    visuals.panel_fill = palette.surface;
    visuals.window_fill = palette.surface;
    visuals.faint_bg_color = palette.raised;
    visuals.extreme_bg_color = palette.well;
    visuals.text_edit_bg_color = Some(palette.well);
    visuals.code_bg_color = palette.well;
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.muted);
    visuals.hyperlink_color = palette.accent;
    visuals.warn_fg_color = palette.warning;
    visuals.error_fg_color = palette.danger;

    visuals.window_stroke = hairline;
    visuals.window_corner_radius = CornerRadius::same(radius::LARGE);
    visuals.menu_corner_radius = CornerRadius::same(radius::MEDIUM);
    visuals.selection.bg_fill = palette.accent_soft;
    visuals.selection.stroke = Stroke::new(palette.hairline_width(), palette.accent);
    // Focus rings are `monitor-cyan`, per §6. egui draws them with the text cursor
    // colour on fields and with the widget stroke elsewhere.
    visuals.text_cursor.stroke = Stroke::new(1.0, palette.info);

    // Tight and low-opacity, per §2: shadow only where something genuinely floats,
    // and then barely. The ambient pass is a second, wider copy at a third of the
    // alpha — egui's `Frame` carries one shadow, so panels that want both draw the
    // ambient themselves through `floating_frame`.
    visuals.popup_shadow = Shadow { offset: [0, 1], blur: 3, spread: 0, color: palette.shadow };
    visuals.window_shadow = Shadow { offset: [0, 2], blur: 8, spread: 0, color: palette.shadow };

    let widget = |bg: Color32, weak_bg: Color32, stroke: Color32, fg: Color32| {
        egui::style::WidgetVisuals {
            bg_fill: bg,
            weak_bg_fill: weak_bg,
            bg_stroke: Stroke::new(palette.hairline_width(), stroke),
            corner_radius: CornerRadius::same(radius::SMALL),
            fg_stroke: Stroke::new(1.0, fg),
            // No expansion: a control that grows under the pointer is the 3D-ish
            // depth §2 rules out, one frame at a time.
            expansion: 0.0,
        }
    };

    visuals.widgets.noninteractive =
        widget(palette.surface, palette.surface, palette.border, palette.muted);
    visuals.widgets.inactive = widget(palette.raised, palette.raised, palette.border, palette.text);
    visuals.widgets.hovered = widget(palette.hover, palette.hover, palette.info, palette.text);
    visuals.widgets.active = widget(palette.pressed, palette.pressed, palette.accent, palette.text);
    visuals.widgets.open = widget(palette.hover, palette.hover, palette.border, palette.text);

    visuals.indent_has_left_vline = false;
    visuals.striped = false;
    visuals.button_frame = true;
    visuals.slider_trailing_fill = true;
    // Square handles, per §4. A circle is the shape of a colour picker, not of an
    // instrument.
    //
    // Wider than it was: at `aspect_ratio: 0.5` the handle came out about 5×9 points
    // and the widget corner radius rounded it into a capsule — the one place "no
    // pillowy radii" visibly slipped. A 0.85 ratio leaves corners that read as
    // corners at the same radius.
    visuals.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.85 };
    visuals.disabled_alpha = 0.45;
    visuals
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// The frame a floating cluster — tool palette, flyout, status cluster — sits in.
///
/// Glass: it hovers over the canvas, so the board keeps running underneath it.
pub fn floating_frame(palette: Palette) -> egui::Frame {
    floating_frame_over(palette, Backing::Canvas)
}

/// [`floating_frame`], for a surface whose backing is not the canvas.
///
/// A popover opened from a docked panel passes [`Backing::Panel`] and gets an opaque
/// frame, which is the whole of the "glass never stacks" rule in one call.
pub fn floating_frame_over(palette: Palette, backing: Backing) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.floating_fill(backing))
        .stroke(palette.hairline_stroke())
        .corner_radius(CornerRadius::same(radius::LARGE))
        .inner_margin(Margin::same(space::UNIT as i8))
        .shadow(Shadow { offset: [0, 1], blur: 3, spread: 0, color: palette.shadow })
}

/// The frame a board-library card or row sits in. Docked, so never glass.
/// The breathing room inside a card, on every side.
///
/// Named rather than left inside [`card_frame`] because the board library paints its cards
/// at a fixed size and has to subtract this from it — a card that took the frame's margin
/// to be a different number would miss its own slot by twice the difference.
pub const CARD_PADDING: f32 = space::of(2);

pub fn card_frame(palette: Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(palette.hairline_stroke())
        .corner_radius(CornerRadius::same(radius::MEDIUM))
        .inner_margin(Margin::same(CARD_PADDING as i8))
}

/// The frame a modal dialog sits in.
///
/// Never glass, and the reason is in `docs/05` §3a: a modal you can see through
/// undermines its own job.
pub fn dialog_frame(palette: Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(palette.hairline_stroke())
        .corner_radius(CornerRadius::same(radius::LARGE))
        .inner_margin(Margin::same(space::of(4) as i8))
        .shadow(Shadow { offset: [0, 2], blur: 12, spread: 0, color: palette.shadow })
}

/// The frame an inset well — a search field, a text field — sits in.
pub fn well_frame(palette: Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.well)
        .stroke(palette.hairline_stroke())
        .corner_radius(CornerRadius::same(radius::SMALL))
        .inner_margin(Margin::symmetric(space::of(2) as i8, space::UNIT as i8))
}

/// Paints the two things a `Frame` cannot: the ambient shadow underneath a floating
/// surface, and the specular catch along its top edge.
///
/// Both are one draw call and neither is optional to the material — §3a is explicit
/// that the top-edge highlight is what separates it from a plain blur. Called after
/// the frame has laid itself out, because the rectangle is not known before then.
pub fn paint_glass_edge(painter: &Painter, rect: Rect, palette: Palette, backing: Backing) {
    let Some(glass) = palette.glass(backing) else { return };
    let corner = f32::from(radius::LARGE);
    let inset = rect.shrink(corner);
    if inset.width() <= 0.0 {
        return;
    }
    // Snapped to the pixel grid before the half-pixel offset. A panel whose top edge
    // lands on a fraction spreads this line across two rows at half strength each,
    // which halves the only contrast the catch has — and on the light cut, where the
    // panel is four percent below white, half of very little is nothing.
    painter.hline(
        inset.x_range(),
        rect.top().round() + 0.5,
        Stroke::new(1.0, glass.highlight),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Color32) -> f32 {
        let f = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    fn ratio(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Text on a surface has to stay legible in both palettes. AA is 4.5:1 for body
    /// text, which is what `docs/05` §6 commits to.
    #[test]
    fn every_text_token_meets_aa_against_the_surface_it_is_drawn_on() {
        for palette in [Palette::LIGHT, Palette::DARK] {
            assert!(ratio(palette.text, palette.surface) > 7.0);
            assert!(ratio(palette.text, palette.raised) > 7.0);
            assert!(ratio(palette.text, palette.well) > 7.0);
            assert!(ratio(palette.muted, palette.surface) > 4.5);
            assert!(ratio(palette.on_accent, palette.accent) > 4.5);
            assert!(ratio(palette.warning, palette.surface) > 4.5);
            assert!(ratio(palette.success, palette.surface) > 4.5);
        }
    }

    /// The "you are here" treatment — a selected tool's glyph, the chosen sidebar
    /// row, the active command-palette row — is drawn on [`Palette::accent_soft`],
    /// and it was the one pairing this file never checked. The accent itself measured
    /// 2.7:1 in light and 2.0:1 in dark on that tint, on the two most important
    /// indicators in the product.
    #[test]
    fn the_selected_treatment_meets_aa_on_its_own_tint() {
        for (name, palette) in [("light", Palette::LIGHT), ("dark", Palette::DARK)] {
            let measured = ratio(palette.on_accent_soft, palette.accent_soft);
            assert!(measured >= 4.5, "{name}: on_accent_soft is {measured:.2}:1");
            // …and it still has to be the accent rather than the body ink, or the
            // selected state stops reading as selected at all.
            //
            // Asked as "the accent's *dominant channel* survives" rather than as
            // `r > g`, which is what this said while the accent was coral. That
            // spelling was a hardcoded fact about one hue: the teal's dominant
            // channel is green, so the literal version failed on a palette that was
            // perfectly correct, and it would have been just as silently wrong the
            // other way if the tint had lost its hue while staying reddish.
            let dominant = |c: Color32| {
                let (r, g, b) = (c.r(), c.g(), c.b());
                if r >= g && r >= b {
                    "red"
                } else if g >= b {
                    "green"
                } else {
                    "blue"
                }
            };
            assert_eq!(
                dominant(palette.on_accent_soft),
                dominant(palette.accent),
                "{name}: on_accent_soft lost the accent's hue"
            );
        }
        // The bug, pinned: what the obvious answer would have measured.
        assert!(ratio(Palette::LIGHT.accent, Palette::LIGHT.accent_soft) < 3.0);
        assert!(ratio(Palette::DARK.accent, Palette::DARK.accent_soft) < 3.0);
    }

    /// The rule `docs/05` §1 is emphatic about: depth does not mirror between modes.
    /// Panels float and the canvas recedes in *both*, so the ramp is not inverted.
    #[test]
    fn panels_sit_above_the_canvas_in_both_modes() {
        for palette in [Palette::LIGHT, Palette::DARK] {
            assert!(
                luminance(palette.surface) > luminance(palette.canvas),
                "the canvas must recede below the panels"
            );
            assert!(
                luminance(palette.backdrop) < luminance(palette.surface),
                "the app ground must sit below a panel"
            );
        }
        // And the mechanical check that they are genuinely two ramps: in light the
        // hairline is darker than the panel, in dark it is lighter.
        assert!(luminance(Palette::LIGHT.border) < luminance(Palette::LIGHT.surface));
        assert!(luminance(Palette::DARK.border) > luminance(Palette::DARK.surface));
    }

    /// *"the app is too gray make it more white ish"* — and the trap in answering it.
    ///
    /// The ramp was **translated** toward white, not compressed into it. Both halves
    /// are pinned here because only the first half is what the user asked for and
    /// only the second half is what keeps the app drawable: the gaps between these
    /// four *are* the hairlines, and this design uses a hairline where other designs
    /// use a shadow. A ramp squashed up against white would have measured as "whiter"
    /// and dissolved every panel edge in the product.
    #[test]
    fn whitening_the_ramp_moved_it_without_closing_the_gaps_that_draw_the_edges() {
        // The ramp as `docs/05` §1 originally specified it.
        let before = [
            Color32::from_rgb(0xF4, 0xF5, 0xF6),
            Color32::from_rgb(0xEF, 0xF1, 0xF2),
            Color32::from_rgb(0xE3, 0xE6, 0xE8),
            Color32::from_rgb(0xDD, 0xE2, 0xE5),
        ];
        use swatch::light as c;
        let after = [c::BONE, c::MILK, c::PEARL, c::FROST];
        let names = ["bone", "milk", "pearl", "frost"];

        for ((name, old), new) in names.iter().zip(before).zip(after) {
            assert!(luminance(new) > luminance(old), "{name} did not get whiter");
            // Uniform, and by the same amount for every token: this is one
            // translation of the whole ramp rather than four eyeballed values.
            for (channel, (o, n)) in [(old.r(), new.r()), (old.g(), new.g()), (old.b(), new.b())]
                .into_iter()
                .enumerate()
            {
                assert_eq!(
                    i32::from(n) - i32::from(o),
                    8,
                    "{name} channel {channel} moved by something other than the ramp"
                );
            }
        }

        // …and therefore the three steps are unchanged, channel for channel. This is
        // implied by the assertion above and asserted anyway, because it is the part
        // that would break first if anyone ever "tidied" one token toward white.
        let gaps = |set: [Color32; 4]| {
            set.windows(2)
                .map(|p| {
                    [
                        i32::from(p[0].r()) - i32::from(p[1].r()),
                        i32::from(p[0].g()) - i32::from(p[1].g()),
                        i32::from(p[0].b()) - i32::from(p[1].b()),
                    ]
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(gaps(before), gaps(after), "the steps between the neutrals moved");
        // The one gap that draws a panel edge, spelled out: `frost` on `bone` is the
        // hairline, and it is the full height of the ramp rather than one step of it.
        let hairline = |set: [Color32; 4]| {
            [
                i32::from(set[0].r()) - i32::from(set[3].r()),
                i32::from(set[0].g()) - i32::from(set[3].g()),
                i32::from(set[0].b()) - i32::from(set[3].b()),
            ]
        };
        assert_eq!(hairline(after), [23, 19, 17]);
        assert_eq!(hairline(after), hairline(before), "the panel edge lost contrast");
    }

    /// The two soft tints have to stay apart, and on this palette they can no longer
    /// do it by hue.
    ///
    /// `accent_soft` is a wash of the teal and `info_soft` a wash of `monitor-cyan` —
    /// 20° apart, where the coral accent was 160° from the cyan and the question
    /// never arose. They meet in the board library, where a space row can be the
    /// selected one *and* the drop target of a drag at the same time, so two
    /// indistinguishable pale washes there would silently undo the drag feedback.
    #[test]
    fn the_selected_tint_and_the_informational_one_are_told_apart_by_value() {
        let palette = Palette::LIGHT;
        let (selected, informational) = (palette.accent_soft, palette.info_soft);
        // Hue alone: what it would have had to rely on, and why it cannot.
        assert!(
            palette.accent.g() > palette.accent.b() && palette.info.b() > palette.info.g(),
            "the two accents are on opposite sides of the green/blue axis, but only just"
        );
        // Value: the separation that actually does the work, measured on the channel
        // the two tints differ in most.
        let apart = i32::from(informational.r()) - i32::from(selected.r());
        assert!(apart >= 25, "the two soft tints are {apart}/255 apart and will be confused");
        // …in the direction that matters: a persistent selection is the heavier of
        // the two, so a transient drop highlight over it still reads as a change.
        assert!(luminance(selected) < luminance(informational));
    }

    /// Every accent Preferences offers stays legible in the four places it lands.
    ///
    /// This is why the choice is three values rather than a colour picker: each one has to
    /// carry ink at AA on *itself* and on its own tint, and only a fixed set can promise it.
    /// The teal and the cobalt disagree about which ink — charcoal on one, white on the
    /// other — which is what makes `on_accent` derived rather than a constant, and this is
    /// the test that would have caught a constant.
    #[test]
    fn every_accent_the_preferences_offer_stays_legible_where_it_lands() {
        for accent in Accent::ALL {
            let palette = Palette::LIGHT.with_accent(accent);
            let name = accent.label();
            assert_eq!(palette.accent, accent.swatch(), "{name}: the swatch did not take");

            let on = ratio(palette.on_accent, palette.accent);
            assert!(on >= 4.5, "{name}: label on the accent is {on:.2}:1");
            let soft = ratio(palette.on_accent_soft, palette.accent_soft);
            assert!(soft >= 4.5, "{name}: label on the tint is {soft:.2}:1");
            // The tint is a tint, not the colour: a selected row must not shout.
            assert!(
                luminance(palette.accent_soft) > luminance(palette.accent),
                "{name}: accent_soft is not a wash of the accent"
            );
            // …and the ink on it still carries the accent's hue, so "selected" reads as
            // selected rather than as ordinary body text.
            assert_ne!(palette.on_accent_soft, palette.text, "{name}: the tint lost its hue");

            // Everything else is untouched. The accent is one decision, not a re-theme.
            assert_eq!(palette.surface, Palette::LIGHT.surface, "{name}: the ramp moved");
            assert_eq!(palette.info, Palette::LIGHT.info, "{name}: the second accent moved");
            assert_eq!(palette.danger, Palette::LIGHT.danger, "{name}: destruction moved");
        }

        // The two inks really are different answers, or `on_accent` being derived buys
        // nothing and a constant would have passed the loop above.
        assert_ne!(
            Palette::LIGHT.with_accent(Accent::Teal).on_accent,
            Palette::LIGHT.with_accent(Accent::Blue).on_accent,
            "both accents took the same ink, so nothing here is being decided"
        );

        // Picking the coral puts the interface back to one colour doing two jobs. That is
        // what "the previous red" means, and it is pinned so nobody later reads it as a bug.
        let previous = Palette::LIGHT.with_accent(Accent::Red);
        assert_eq!(previous.accent, previous.danger);
        // The default does not, which is the arrangement the teal was chosen to allow.
        assert_ne!(Palette::LIGHT.accent, Palette::LIGHT.danger);
        assert_eq!(Palette::LIGHT.with_accent(Accent::default()).accent, Palette::LIGHT.accent);
    }

    /// The reds and cyans are two specifications, not one filtered value, so no token
    /// may be shared between the cuts. Sharing one is exactly the bug that survives
    /// review: it looks right in whichever mode it was authored in.
    #[test]
    fn no_token_is_carried_unchanged_from_one_mode_to_the_other() {
        let (light, dark) = (Palette::LIGHT, Palette::DARK);
        let pairs: [(&str, Color32, Color32); 21] = [
            ("backdrop", light.backdrop, dark.backdrop),
            ("surface", light.surface, dark.surface),
            ("raised", light.raised, dark.raised),
            ("well", light.well, dark.well),
            ("canvas", light.canvas, dark.canvas),
            ("hover", light.hover, dark.hover),
            ("pressed", light.pressed, dark.pressed),
            ("border", light.border, dark.border),
            ("text", light.text, dark.text),
            ("muted", light.muted, dark.muted),
            ("faint", light.faint, dark.faint),
            ("accent", light.accent, dark.accent),
            ("on_accent", light.on_accent, dark.on_accent),
            ("accent_soft", light.accent_soft, dark.accent_soft),
            ("info", light.info, dark.info),
            ("info_soft", light.info_soft, dark.info_soft),
            ("danger", light.danger, dark.danger),
            ("warning", light.warning, dark.warning),
            ("success", light.success, dark.success),
            ("shadow", light.shadow, dark.shadow),
            ("handle_edge", light.handle_edge, dark.handle_edge),
        ];
        for (name, a, b) in pairs {
            assert_ne!(a, b, "`{name}` is the same value in both modes");
        }
        assert_ne!(light.glass_opacity, dark.glass_opacity);
    }

    /// The accents shift in a specific direction, and the direction is the reason the
    /// palette is specified twice: red deepens so it does not glow, cyan brightens so
    /// it stays legible.
    ///
    /// The red half is now asked of `danger`, not of `accent`. It used to be the same
    /// question because the two were the same token in both cuts; light's accent is
    /// `signal-teal` now, and comparing a teal against a red would have kept passing
    /// while measuring nothing.
    #[test]
    fn the_red_deepens_into_dark_and_the_cyan_brightens() {
        assert!(
            luminance(Palette::DARK.danger) < luminance(Palette::LIGHT.danger),
            "xr-red must deepen"
        );
        assert!(
            luminance(Palette::DARK.info) > luminance(Palette::LIGHT.info),
            "monitor-cyan must brighten"
        );
    }

    /// Destruction wears the original coral, and — in the shipped cut — *only*
    /// destruction does.
    ///
    /// `xr-red` used to carry selection, the active tool and destruction all three,
    /// so "delete" and "selected" were the same colour. Giving the first two to
    /// `signal-teal` freed it. What this pins is that the freeing did not turn into
    /// an *invented* second red: `danger` is still the exact swatch value.
    #[test]
    fn destruction_wears_the_users_own_coral_and_nothing_else_does() {
        assert_eq!(Palette::LIGHT.danger, swatch::light::XR_RED, "not an invented red");
        assert_ne!(
            Palette::LIGHT.danger,
            Palette::LIGHT.accent,
            "delete must not look like selected"
        );
        // Nothing else in the light interface may read as red, or the separation
        // above buys nothing. A saturated red is red far above *both* other channels
        // — which is what lets `warning` through: amber is red-dominant (138) but its
        // green sits at 93, only 45 behind, so it reads as gold rather than as a
        // second danger. The coral is 139 and 142 clear.
        let red_gap = |c: Color32| {
            (i32::from(c.r()) - i32::from(c.g())).min(i32::from(c.r()) - i32::from(c.b()))
        };
        assert!(red_gap(Palette::LIGHT.danger) > 80, "the control: the coral is a red");
        for (name, token) in [
            ("accent", Palette::LIGHT.accent),
            ("info", Palette::LIGHT.info),
            ("success", Palette::LIGHT.success),
            ("warning", Palette::LIGHT.warning),
        ] {
            assert!(red_gap(token) <= 80, "{name} is reading as a second red");
        }
        // The retained dark cut keeps the original arrangement; see `Palette::DARK`.
        assert_eq!(Palette::DARK.danger, Palette::DARK.accent);
    }

    #[test]
    fn hover_carries_a_trace_of_the_cyan_it_is_assigned() {
        for palette in [Palette::LIGHT, Palette::DARK] {
            // Bluer than it is red, which a neutral step would not be.
            assert!(palette.hover.b() > palette.hover.r(), "hover has no cyan in it");
            assert!(palette.pressed.b() > palette.pressed.r());
            // …but still a neutral, not a swatch of cyan.
            assert!(
                u32::from(palette.hover.b()) - u32::from(palette.hover.r()) < 40,
                "hover is shouting"
            );
        }
    }

    /// *"i want only light mode"* — the user, superseding their earlier request for a
    /// dark cut. Nothing may resolve to the dark palette, whatever is asked and
    /// whatever the machine is set to.
    #[test]
    fn nothing_resolves_to_the_dark_palette() {
        let dark = SystemAppearance { dark: true, ..SystemAppearance::default() };
        let light = SystemAppearance::default();
        for preference in ThemePreference::ALL {
            for system in [dark, light] {
                assert_eq!(
                    preference.resolve(system),
                    Theme::Light,
                    "{preference:?} on a {} machine",
                    if system.dark { "dark" } else { "light" }
                );
            }
        }
        // The dark cut is kept, unreachable, because it costs nothing and the
        // decision may reverse. `resolve_either` is the only way back to it.
        assert_eq!(ThemePreference::System.resolve_either(dark), Theme::Dark);
        assert_eq!(ThemePreference::System.resolve_either(light), Theme::Light);
        assert_eq!(ThemePreference::Light.resolve_either(dark), Theme::Light);
        assert_eq!(ThemePreference::Dark.resolve_either(light), Theme::Dark);
    }

    /// The accessibility contract: Reduce Transparency must produce genuinely opaque
    /// values, not merely a higher alpha.
    #[test]
    fn reduce_transparency_makes_every_floating_surface_fully_opaque() {
        let system = SystemAppearance { reduce_transparency: true, ..SystemAppearance::default() };
        for theme in [Theme::Light, Theme::Dark] {
            let palette = Palette::resolve(theme, system);
            assert!(!palette.translucent);
            assert_eq!(palette.glass_opacity, u8::MAX);
            assert_eq!(palette.glass(Backing::Canvas), None, "no glass at all");
            assert_eq!(palette.floating_fill(Backing::Canvas), palette.surface);
            assert_eq!(
                palette.floating_fill(Backing::Canvas).a(),
                u8::MAX,
                "the fill must be opaque, not merely opaque-ish"
            );
            assert_eq!(floating_frame(palette).fill.a(), u8::MAX);
        }
    }

    /// …and that without it, the material is actually translucent, or the test above
    /// would pass on a palette that never had glass in the first place.
    #[test]
    fn the_default_appearance_leaves_floating_chrome_translucent() {
        for theme in [Theme::Light, Theme::Dark] {
            let palette = Palette::resolve(theme, SystemAppearance::of(theme));
            let glass = palette.glass(Backing::Canvas).expect("glass over the canvas");
            assert!(glass.opacity < u8::MAX, "the tint has to let the board through");
            // Against the named floor rather than a number written twice: this is the
            // same bound the Preferences slider stops at, and it moved once already.
            assert!(
                glass.opacity >= Palette::MIN_GLASS_OPACITY,
                "…but text on it still has to be legible",
            );
            assert!(palette.floating_fill(Backing::Canvas).a() < u8::MAX);
            assert!(floating_frame(palette).fill.a() < u8::MAX);
        }
    }

    /// The slider only ever thins the tint down to the legibility floor. A control that
    /// can reach zero is a control that can make the toolbar unreadable over a
    /// photograph, and `docs/05-design-language.md` §3a puts legibility first.
    #[test]
    fn the_transparency_slider_cannot_thin_the_tint_past_legibility() {
        let palette = Palette::LIGHT;
        assert_eq!(palette.with_glass_opacity(0).glass_opacity, Palette::MIN_GLASS_OPACITY);
        assert_eq!(palette.with_glass_opacity(200).glass_opacity, 200);
        const { assert!(Palette::MIN_GLASS_OPACITY < Palette::MAX_GLASS_OPACITY) };
        // The default has to sit inside the range the slider offers, or the control
        // opens with its knob off the end of its own track.
        assert!(palette.glass_opacity >= Palette::MIN_GLASS_OPACITY);
        assert!(palette.glass_opacity <= Palette::MAX_GLASS_OPACITY);
    }

    /// Glass never stacks: the same palette, asked for a surface over a panel, has to
    /// refuse.
    #[test]
    fn glass_is_refused_over_anything_that_is_not_the_canvas() {
        for theme in [Theme::Light, Theme::Dark] {
            let palette = Palette::of(theme);
            assert!(palette.glass(Backing::Canvas).is_some());
            assert_eq!(palette.glass(Backing::Panel), None);
            assert_eq!(palette.floating_fill(Backing::Panel), palette.surface);
            assert_eq!(floating_frame_over(palette, Backing::Panel).fill, palette.surface);
            assert_eq!(dialog_frame(palette).fill.a(), u8::MAX, "a modal is opaque");
            assert_eq!(card_frame(palette).fill.a(), u8::MAX, "a library card is opaque");
        }
    }

    #[test]
    fn high_contrast_deepens_the_ink_and_thickens_the_hairline() {
        let system = SystemAppearance { increase_contrast: true, ..SystemAppearance::default() };
        for theme in [Theme::Light, Theme::Dark] {
            let plain = Palette::of(theme);
            let hc = Palette::resolve(theme, system);
            assert!(
                ratio(hc.text, hc.surface) > ratio(plain.text, plain.surface),
                "{theme:?} high contrast did not raise the text contrast"
            );
            assert!(ratio(hc.muted, hc.surface) > ratio(plain.muted, plain.surface));
            assert!(ratio(hc.border, hc.surface) > ratio(plain.border, plain.surface));
            assert_eq!(hc.hairline, 2);
            assert!(!hc.translucent, "legibility wins over the material");
        }
    }

    #[test]
    fn every_radius_stays_inside_the_four_to_six_band() {
        for r in [radius::SMALL, radius::MEDIUM, radius::LARGE] {
            assert!((4..=6).contains(&r), "{r} is not a radius this design uses");
        }
        // Compile-time, because a broken scale should not need a test run to notice.
        const {
            assert!(radius::SMALL < radius::MEDIUM && radius::MEDIUM < radius::LARGE);
            assert!(radius::TIGHT < radius::SMALL, "the exception must be smaller, not larger");
        }
    }

    /// Every dimension the chrome reserves has to land on the grid, or the panels
    /// stop lining up with the controls inside them.
    #[test]
    fn every_reserved_dimension_lands_on_the_four_pixel_grid() {
        for (name, value) in [
            ("menu bar", MENU_BAR_HEIGHT),
            ("properties", PROPERTIES_WIDTH),
            ("sidebar", SIDEBAR_WIDTH),
            ("tool button", TOOL_BUTTON),
        ] {
            assert_eq!(value % space::UNIT, 0.0, "{name} is off the grid at {value}");
        }
        assert_eq!(space::of(3), 12.0);
    }

    #[test]
    fn the_type_scale_is_the_four_sizes_the_design_language_names() {
        assert_eq!(
            [text::LABEL, text::BODY, text::PANEL_TITLE, text::SCREEN_TITLE],
            [11.0, 13.0, 15.0, 20.0]
        );
        const {
            assert!(text::NUMERIC < text::BODY, "mono runs wide and must not overflow a row");
        }
    }

    /// The point of the numeric style. If it ever stops being monospace, coordinates
    /// jitter while they are dragged and nobody notices in a screenshot — every digit
    /// has to have the same advance, and the family is how egui selects that.
    #[test]
    fn numbers_are_drawn_in_the_monospace_family_at_every_width() {
        let ctx = egui::Context::default();
        apply(&ctx, Theme::Light);
        // egui has no font atlas until a pass has run.
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let mono = FontId::new(text::NUMERIC, FontFamily::Monospace);
        let proportional = FontId::new(text::NUMERIC, FontFamily::Proportional);
        let width = |digits: &str, font: &FontId| {
            ctx.fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap(digits.to_owned(), font.clone(), Palette::LIGHT.text)
                    .size()
                    .x
            })
        };

        let samples = ["0000", "1111", "8888", "1234", "-1.5"];
        let widths: Vec<f32> = samples.iter().map(|s| width(s, &mono)).collect();
        assert!(widths[0] > 0.0);
        for (sample, w) in samples.iter().zip(&widths) {
            assert!(
                (w - widths[0]).abs() < 0.01,
                "{sample:?} is {w} wide against {}: a dragged coordinate will jitter",
                widths[0]
            );
        }

        // The control. egui's *bundled* interface face happens to have tabular
        // digits, so digits alone would not prove the family was doing the work —
        // and the app does not ship that face anyway. `docs/05` §5 puts the platform
        // sans in this slot, and SF Pro's figures are proportional by default. Two
        // strings of the same length in the widest and narrowest glyphs available
        // separate the two families whatever font is registered.
        assert!(
            (width("iiii", &proportional) - width("WWWW", &proportional)).abs() > 1.0,
            "the proportional slot is holding a monospace font; this test proves nothing"
        );
        assert!(
            (width("iiii", &mono) - width("WWWW", &mono)).abs() < 0.01,
            "the monospace slot is not monospace"
        );

        assert!(format!("{:?}", numeric("0")).contains("Monospace"), "numeric() lost its family");
    }

    #[test]
    fn applying_a_theme_sets_the_panel_fill_and_the_text_scale() {
        let ctx = egui::Context::default();
        apply(&ctx, Theme::Dark);
        let style = ctx.global_style();
        assert_eq!(style.visuals.panel_fill, Palette::DARK.surface);
        assert!(style.visuals.dark_mode);
        assert_eq!(style.text_styles[&TextStyle::Body].size, text::BODY);
        assert_eq!(style.text_styles[&TextStyle::Monospace].family, FontFamily::Monospace);

        apply(&ctx, Theme::Light);
        assert_eq!(ctx.global_style().visuals.panel_fill, Palette::LIGHT.surface);
        assert!(!ctx.global_style().visuals.dark_mode);
    }

    #[test]
    fn applying_a_reduced_transparency_appearance_reaches_the_context() {
        let ctx = egui::Context::default();
        let system = SystemAppearance {
            dark: true,
            reduce_transparency: true,
            increase_contrast: true,
        };
        apply_appearance(&ctx, Theme::Dark, system);
        let expected = Palette::resolve(Theme::Dark, system);
        assert_eq!(ctx.global_style().visuals.override_text_color, Some(expected.text));
        assert_eq!(
            ctx.global_style().visuals.widgets.inactive.bg_stroke.width,
            expected.hairline_width()
        );
    }

    #[test]
    fn theme_toggles_between_exactly_two_states() {
        assert_eq!(Theme::Light.toggled(), Theme::Dark);
        assert_eq!(Theme::Dark.toggled().toggled(), Theme::Dark);
    }

    /// `mix` is the audit trail for every derived token, so its endpoints have to be
    /// exact or a derived value drifts by a byte from what the comment claims.
    #[test]
    fn mixing_is_exact_at_both_ends_and_symmetric_in_the_middle() {
        let a = swatch::light::BONE;
        let b = swatch::light::PEARL;
        assert_eq!(mix(a, b, 0), a);
        assert_eq!(mix(a, b, 100), b);
        assert_eq!(mix(a, b, 101), b, "an out-of-range mix clamps rather than wraps");
        assert_eq!(mix(a, b, 50), mix(b, a, 50));
    }

    #[test]
    fn alpha_premultiplies_rather_than_merely_setting_the_alpha_byte() {
        let opaque = swatch::light::INK;
        let half = alpha(opaque, 128);
        assert_eq!(half.a(), 128);
        assert!(half.r() <= opaque.r() && half.g() <= opaque.g() && half.b() <= opaque.b());
        assert_eq!(alpha(opaque, 255), opaque);
        assert_eq!(alpha(opaque, 0), Color32::TRANSPARENT);
    }
}
