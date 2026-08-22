//! What a link card says, and how tall the boxes it says it in are.
//!
//! # Why this module exists
//!
//! A link card is the one item on the board whose words are not `ItemKind::text()`. They live
//! in the kind's own fields — `title`, `url`, `description`, `provider` — and they are drawn as
//! **three separate blocks**, because `DrawList::push_layout` takes one colour *and* one size
//! for a whole run and `SpanStyle` has no size field at all. A muted site name, a bold title
//! and a smaller grey blurb is therefore three queues, not one styled string.
//!
//! Every one of the numbers and predicates below was arrived at by a user photographing a Velm
//! card beside Miro's and saying what was wrong with it, and several of them were arrived at
//! twice. That is the whole reason they are *here* rather than in a front end: `vellum-app`
//! draws this card from a native window and `vellum-web` draws it into a browser canvas, and a
//! browser card whose title is set at a different scale from the Mac's is two derivations of
//! one measurement — the failure `look.rs`'s own header exists to name.
//!
//! # Why it is here rather than in `vellum-web`
//!
//! `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so `cargo test` compiles it to an empty
//! crate and **nothing in it can have a runnable test**. Two of the functions below have
//! aborted the application on real board data — see [`strip_site_affix`] — and `panic =
//! "abort"` in `[profile.release]` means an abort on the paint path is not an error anybody
//! catches, it is the window (or the tab) going away on the frame a card first became visible.
//! Arithmetic with that history belongs somewhere a test runner can reach it.
//!
//! # What is deliberately *not* here
//!
//! Entity decoding. `vellum_link::decode_entities` already exists, is a single left-to-right
//! pass that cannot have the `&amp;lt;` ordering bug by shape rather than by order, and is
//! already tested there. Copying it in to give this module a tidier surface would be the
//! second derivation this module exists to prevent, and would cost `vellum-project` a
//! dependency it does not otherwise need.

use crate::look::CARD_PADDING;

// ---------------------------------------------------------------------------------------
// The type scale — three voices, and the distances between them
// ---------------------------------------------------------------------------------------

/// Line spacing inside a card, as a multiple of the font size.
///
/// Tighter than prose: a card is a stack of short labels rather than a paragraph.
///
/// ⚠ **A caller may shape at a slightly different line height, and that is tolerable here in
/// a way it is not elsewhere.** `vellum-app` passes this value through `Style::line_height`,
/// so its boxes and its glyphs agree exactly. `vellum-web` shapes through
/// `LayoutParams::default()`, whose `line_height` is `vellum_text::DEFAULT_LINE_HEIGHT`
/// (1.36, Miro's own) — 0.74% more per line than the reservation below. It is safe there for
/// two specific reasons and it is worth writing them down rather than rediscovering them: the
/// box *height* barely reaches that shaper at all (an explicit `font_size` is passed, so no
/// auto-fit reads it, and the anchor is top-left, so no centring reads it), and
/// [`BLURB_BOTTOM_AIR`] reserves half a line under the last block — some forty times the
/// worst-case drift across a whole card. What must never be done is to reserve at one number
/// and budget characters at another; both come from this constant.
pub const CARD_LINE_HEIGHT: f64 = 1.35;

/// The title's size, as a multiple of the card's base size.
///
/// **Parity with the base size, and it must stay there.** It was raised to a third above once,
/// to answer *"from a distance I want to be able to see more"*, and overshot — against the Miro
/// card the user then sent as a reference, a Velm title was two words filling the card. They
/// asked for it back down. Miro's own title is close to its body size and earns its prominence
/// from **colour** — near-black against the blurb's grey — and from **weight**, rather than
/// from scale. The distance between the three voices is opened from *below*, by
/// [`PROVIDER_SCALE`] and [`BLURB_SCALE`], for exactly that reason.
pub const TITLE_SCALE: f64 = 1.0;

/// The site name's size, as a multiple of the card's base size.
///
/// **Below the base**, so a card's three voices are three sizes rather than two. It used to be
/// the base itself: identical to the title and *larger* than the blurb, so the largest text on
/// the card was the one thing on it that matters least. Measured off the user's reference, Miro
/// runs roughly 0.65 : 1.0 : 0.76 for name : title : blurb.
pub const PROVIDER_SCALE: f64 = 0.80;

/// The blurb's size, as a multiple of the card's base size.
///
/// Below the base rather than at it, so the hierarchy is carried by *two* differences — size
/// and colour — rather than by colour alone. A muted blurb at the same size as the title still
/// reads as one block of text at a glance, which is the state the user was describing when they
/// said the card *"kind of just looks a little bit off"*.
pub const BLURB_SCALE: f64 = 0.86;

/// The most lines of title a card will give up before the blurb gets what is left.
///
/// A **ceiling**, not a reservation — see [`stack`]. Taken flat it left a one-line title with
/// two empty lines of card between itself and the blurb, which the user photographed as a band
/// of white under *"Crush 80 Reboot Pro"* while asking for *more* white space. That is the
/// tell: air a reader cannot account for is never read as spacing, it is read as a broken card.
pub const TITLE_LINES: f64 = 3.0;

/// The most lines of blurb a card will draw.
///
/// *"less decription more white space"*. Unbounded before this, so a tall card gave the blurb
/// six or more lines and read as a paragraph with a heading. The lines it gives up are what pay
/// for the two gaps below; the card's own box does not change. Three is what Miro's own card
/// shows before it ellipsises.
pub const BLURB_LINES: f64 = 3.0;

/// Air under the site name, as a fraction of the card's padding.
///
/// Zero before this — **every gap inside a card came from [`CARD_LINE_HEIGHT`] alone**, so the
/// three blocks sat as close together as two lines of one paragraph and no amount of size or
/// colour difference could make them read as three separate things.
pub const PROVIDER_GAP: f64 = 0.5;

/// Air under the title, as a fraction of the card's padding.
///
/// Larger than [`PROVIDER_GAP`], and deliberately: the name and the title belong together — the
/// name is a label *for* the title — and the blurb is the separate thing. A uniform gap makes
/// the card read as three equal strangers rather than as a heading and its description.
pub const TITLE_GAP: f64 = 0.75;

/// Air below the blurb, in blurb line-heights.
///
/// *"the paragraph should be fixed so that there is a little bit of spacing underneath them."*
/// Half a line: enough that the last row of text is visibly *inside* the card rather than
/// resting on its edge, and not so much that a short card loses a line of blurb to margin.
pub const BLURB_BOTTOM_AIR: f64 = 0.5;

/// How much of a line's raw character capacity survives being word-wrapped.
///
/// **Measured against the failure, not guessed, and it is not a fudge factor.** At half an em
/// per character with no discount, the reference card's title — *"rust-lang/rust: Empowering
/// everyone to build reliable software"*, 62 characters in a 238-unit box at 15 units —
/// estimates 31 per line and so **two** lines. It shapes to three: the wrap puts 26 on each of
/// the first two. The blurb was then positioned two lines down and drew straight through the
/// title's third line, which is a worse fault than the empty band the estimate had replaced.
/// Only a screenshot found it — the `--demo links` one — because no assertion about box
/// geometry could: the boxes were exactly as asked for.
///
/// **The mechanism, since the number reads as a fudge factor without it.** It is the difference
/// between how many characters *fit* on a line and how many a *word wrap* puts there. Text
/// breaks at spaces, so every line but the last gives up part of a word; the raw advance
/// estimate is the count for a string with no spaces in it, which is the one case that never
/// happens in a title.
///
/// 0.85 turns that 31 into 26, which is what the text actually does.
const WRAP_EFFICIENCY: f64 = 0.85;

// ---------------------------------------------------------------------------------------
// The stack
// ---------------------------------------------------------------------------------------

/// A box in the card's own space: `(x, y, width, height)`, origin at the card's top-left, y
/// down.
///
/// Local rather than world so this stays arithmetic a test can drive with three numbers. Each
/// front end adds its own origin — and only its own origin, which is what keeps the two of them
/// from being able to disagree about anything except where the card is.
pub type LocalBox = (f64, f64, f64, f64);

/// Where a card's three voices go, in the card's own space.
///
/// A zero-height box means **draw nothing** and is a legitimate answer for any of the three: a
/// card too short for a blurb should lose the blurb rather than truncate the name of the thing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardStack {
    /// The site name, muted, at [`PROVIDER_SCALE`].
    pub provider: LocalBox,
    /// The title, bold, in the card's own ink, at [`TITLE_SCALE`].
    pub title: LocalBox,
    /// The page's description, muted, at [`BLURB_SCALE`].
    pub blurb: LocalBox,
}

/// Where a card's three text blocks sit, given what else is on the card.
///
/// # The four things that are easy to get wrong here
///
/// - **`picture_bottom` is where the picture actually ends, not a fraction.** The caller passes
///   the bottom edge of the band *it is drawing*, so the words cannot start above or below the
///   picture they are captioning. Deriving it here from a fraction would be a second answer to
///   a question the painter has already answered, and the two front ends do not use the same
///   fraction: `vellum-app` honours `CardMode::image_fraction` (0.46 or 0.62) while
///   `vellum-web` draws every card at 0.46 because it has no mode control to change it with.
/// - **The badge pushes the words aside only when they share a row.** Clipping unconditionally
///   is the easy version, and it makes every title on the board mysteriously short for a
///   collision that cannot happen — a card with a picture starts its words far below the ↗.
///   Asked separately of the provider row and of the body, because the two are at different
///   heights and only one of them may be under the badge. That second half was missed once: the
///   badge is 1.75 line-heights tall against a provider row of one, so on a card with **no**
///   picture it reaches past the site name into the title's first line.
/// - **The title takes the lines it needs, not the lines it may have.** See [`TITLE_LINES`].
/// - **Both boxes hold a whole number of lines.** A box 2.4 lines tall draws a third line 40%
///   inside the card and the rest of the way out through the bottom of it. The character clip
///   upstream cannot prevent that: it estimates from the advance, so it decides how much text
///   to *shape* and never where the last line *lands*.
pub fn stack(
    width: f64,
    height: f64,
    font_size: f64,
    // The bottom edge of the picture band, in card-local units, or `None` for a card drawing
    // no picture. See above — this is measured, never derived.
    picture_bottom: Option<f64>,
    // The ↗ badge's box, in card-local units, or `None` for a card with no openable address.
    badge: Option<LocalBox>,
    // How many characters the title block will actually **draw** — after entity decoding and
    // after `strip_site_affix`, never the stored string's length. Counting the stored one
    // over-reserves, which is the empty band above arriving by a second route.
    title_chars: usize,
    // Whether there is a blurb to draw at all. A video card has none by content rule, and a
    // zero-height box is how that is said in geometry.
    has_blurb: bool,
) -> CardStack {
    let pad = width * CARD_PADDING;
    let line = font_size * CARD_LINE_HEIGHT;

    // A pad of air between the picture and the site name, so the row does not sit against it.
    let mut y = picture_bottom.map_or(pad, |bottom| bottom + pad);

    // `pad < bx` is not redundant with the vertical test: a badge that has been clamped all the
    // way to the card's left edge is not something the text can move aside for, and shortening
    // the row to `max(1.0)` there would leave a one-unit box rather than an honest overlap.
    let collides = |top: f64| badge.is_some_and(|(bx, by, _, bh)| top < by + bh && pad < bx);
    let yield_to_badge = |top: f64| match badge {
        Some((_, _, bw, _)) if collides(top) => pad + bw + pad * 0.5,
        _ => pad,
    };

    let provider = (pad, y, (width - pad - yield_to_badge(y)).max(1.0), line);
    y += line + pad * PROVIDER_GAP;

    let body_right = yield_to_badge(y);
    let body_w = (width - pad - body_right).max(1.0);
    let body_h = (height - y - pad).max(0.0);

    let title_line = font_size * TITLE_SCALE * CARD_LINE_HEIGHT;
    // `title_line / CARD_LINE_HEIGHT` rather than `font_size * TITLE_SCALE` spelled again: the
    // estimate wants the *type* size and the reservation wants the *line* size, and deriving
    // one from the other is what stops them drifting if `TITLE_SCALE` ever moves.
    let lines =
        estimated_lines(title_chars, body_w, title_line / CARD_LINE_HEIGHT).clamp(1.0, TITLE_LINES);
    let title_h = whole_lines(body_h.min(title_line * lines), title_line);

    let blurb_line = font_size * BLURB_SCALE * CARD_LINE_HEIGHT;
    // The blurb hangs off the title rather than touching it — see `TITLE_GAP`. Taken out of the
    // blurb's room rather than added to the card, so the item's own box is untouched: *"without
    // changing the over all size"*.
    let title_gap = pad * TITLE_GAP;
    // …and it is capped at `BLURB_LINES` rather than taking everything left over, and keeps
    // `BLURB_BOTTOM_AIR` beneath it. The bottom `pad` alone is derived from the card's *width*,
    // so on a tall narrow card it is a hairline — and text ending flush against an edge reads
    // as clipped even when every glyph is inside the box.
    let blurb_room = if has_blurb {
        (body_h - title_h - title_gap - blurb_line * BLURB_BOTTOM_AIR)
            .max(0.0)
            .min(blurb_line * BLURB_LINES)
    } else {
        0.0
    };

    CardStack {
        provider,
        title: (pad, y, body_w, title_h),
        blurb: (pad, y + title_h + title_gap, body_w, whole_lines(blurb_room, blurb_line)),
    }
}

/// The tallest whole number of `line_height`s that fits in `available`.
///
/// ⚠ **The nudge is not defensive; an exact fit is the common case.** These heights are built
/// by adding and subtracting the same line height and padding several times, so a box sized to
/// hold exactly one line arrives as `22.463999999999874` against a line of `22.464000000000002`
/// — and `floor` answers **zero**. Measured on a real card: the title box came out empty and
/// the card drew a picture and a site name with no title under it.
///
/// A *relative* epsilon rather than an absolute one, because the line height scales with the
/// card: an absolute tolerance that works at 13 units is either useless or far too generous at
/// 78.
pub fn whole_lines(available: f64, line_height: f64) -> f64 {
    if line_height <= 0.0 {
        return available.max(0.0);
    }
    // ⚠ **The nudge is not defensive; an exact fit is the common case.** These heights are
    // built by adding and subtracting the same line height and padding several times, so a box
    // sized to hold exactly one line arrives as 22.463999999999874 against a line of
    // 22.464000000000002 — and `floor` answers **zero**. Measured, on the video card the user
    // asked to have its caption pinned to the foot: the title box came out empty and the card
    // drew a poster and a provider row with no title under it.
    //
    // A *relative* epsilon rather than an absolute one, because the line height scales with the
    // card: an absolute tolerance that works at 13 units is either useless or far too generous
    // at 78.
    const SNAP: f64 = 1e-6;
    (((available / line_height) + SNAP).floor() * line_height).max(0.0)
}

// ---------------------------------------------------------------------------------------
// How much text fits
// ---------------------------------------------------------------------------------------

/// How many characters of a card's text fit on one line of a box `width` wide at `size`.
///
/// **The one estimate.** Both the character budget a card's text is clipped to and the number of
/// lines its title box reserves are answers to "how long is this run", and two copies of the
/// arithmetic would be two answers — a layout that reserves two lines for a title the clip cut
/// to one is the empty band this pair exists to prevent.
///
/// Advance-blind on purpose: half the font size is a proportional face's mean advance to within
/// a few percent, and measuring properly needs a shaping pass, which is the very thing this
/// decides the input to. The floor of 8 keeps a degenerate box from answering zero.
///
/// Discounted by [`WRAP_EFFICIENCY`]. **Both readers want to be wrong in the same direction**,
/// which is what lets one number serve them: under-counting makes the clip budget *shorter*
/// (less overflow) and the line reservation *taller* (no collision). Over-counting breaks both.
pub fn chars_per_line(width: f64, size: f64) -> usize {
    ((width / (size * 0.5) * WRAP_EFFICIENCY).floor() as usize).max(8)
}

/// The lines `chars` characters need in a box `width` wide at `size`.
///
/// ⚠ **Bolding widens advances by 3-5%**, so a title within a character or two of a line
/// boundary can shape one line longer than this predicts. [`whole_lines`] clamps the box to the
/// card either way, so the failure mode is a tighter gap under the title rather than text
/// leaving the card — which is the direction to be wrong in.
pub fn estimated_lines(chars: usize, width: f64, size: f64) -> f64 {
    chars.max(1).div_ceil(chars_per_line(width, size)) as f64
}

/// How many characters a whole block `width` × `height` can hold at `size`.
///
/// What the text is clipped to before it is shaped. **Clipped rather than allowed to
/// overflow**: a card is a fixed box and a page's blurb is any length, and without this a forum
/// page with a 400-character description draws its text straight out through the bottom of the
/// card and on to the board, over whatever is beneath it.
///
/// Floored at one line's worth, so a block whose height rounds to nothing still draws the
/// beginning of its first line rather than nothing at all.
pub fn line_budget(width: f64, height: f64, size: f64) -> usize {
    let per_line = chars_per_line(width, size);
    let lines = ((height / (size * CARD_LINE_HEIGHT)).floor() as usize).max(1);
    per_line.saturating_mul(lines).max(per_line)
}

/// Shortens `text` to at most `budget` characters, ending in an ellipsis when it had to cut.
///
/// Characters, not bytes: the cut has to land on a boundary, and a budget in bytes would split
/// a multi-byte one.
pub fn ellipsise(text: &str, budget: usize) -> String {
    // **No room means nothing, not everything.** This used to return the whole string when
    // `budget < 2`, on the reasoning that there was no room for an ellipsis — which is true and
    // is the opposite of the right answer. A block with nothing left got the text in full, so a
    // card whose title had already spent the budget then drew its entire 600-character URL
    // underneath and out through the bottom.
    if budget == 0 {
        return String::new();
    }
    if text.chars().count() <= budget {
        return text.to_owned();
    }
    if budget == 1 {
        return "…".to_owned();
    }
    let kept: String = text.chars().take(budget - 1).collect();
    // Cut at the last word boundary in the kept part, so a row ends on a word rather than
    // mid-syllable — unless that would throw most of it away.
    let trimmed = kept.trim_end();
    match trimmed.rsplit_once(' ') {
        Some((head, _)) if head.chars().count() >= budget / 2 => format!("{head}…"),
        _ => format!("{trimmed}…"),
    }
}

// ---------------------------------------------------------------------------------------
// What the words are
// ---------------------------------------------------------------------------------------

/// Removes the site's own name from the front or the back of a page title.
///
/// # Why the row above makes it redundant
///
/// *"for amazon.com you dont have to write the amazon.com at the beginning."* Pages name
/// themselves in their `<title>` because a browser tab has nowhere else to say it — so the
/// reference board carries *"Amazon.com : Superbat 3G/6G/12G SDI Cable…"* and
/// *"IQL-IMX678/FF | DigiKey Electronics"*. A card has already said the site, in its own row.
/// Repeating it spends the first line of the title — the one line that survives being small on
/// screen — on a word the user is not reading.
///
/// Both ends, because pages do both: American retailers lead with it, and most of the rest of
/// the web trails it after a pipe or a dash.
///
/// # What it refuses to do
///
/// It only ever strips across a **separator**, and never leaves a title shorter than
/// `SHORTEST_TITLE`. Both guards earn their place: without the separator a page called
/// *"Amazonian Fish"* on `amazon.com` loses its first word, and without the length floor a
/// DigiKey page titled simply *"DigiKey"* is left with nothing at all — a card with an empty
/// title where the real one was is much worse than a card that repeats itself.
///
/// # ⚠ It aborted the application twice, on real titles
///
/// This runs inside a painter's layout closure and `[profile.release]` sets `panic = "abort"`,
/// so a bad index here does not throw — it takes the window, or the browser tab, on the frame a
/// card first becomes visible, and again on relaunch because a board reopens at the same camera.
/// Both faults were found by an adversarial review *after* the tests that shipped with the
/// function were green, and neither was reachable from them because those tests used only ASCII
/// titles and separator-free providers. The fix is `str::get` throughout rather than two extra
/// guards: `get` answers `None` for both failures, which makes this panic-free **by
/// construction** instead of by argument. Both are pinned by a test below; do not "simplify"
/// either back to indexing.
pub fn strip_site_affix<'a>(title: &'a str, provider: Option<&str>) -> &'a str {
    /// A title this short is very likely *only* the site's name, and the site's name is the
    /// most useful thing left to show.
    const SHORTEST_TITLE: usize = 3;
    /// What a page puts between its own name and its title.
    const SEPARATORS: [char; 6] = [':', '|', '-', '\u{2013}', '\u{2014}', '\u{00BB}'];

    let Some(provider) = provider.map(str::trim).filter(|p| !p.is_empty()) else { return title };
    // `Amazon` against a title leading `Amazon.com` — the row says the short name and the page
    // writes the domain, so the comparison is on the provider as a *prefix* of the run rather
    // than on equality.
    //
    // **`get`, never `[..]`.** `provider.len()` is a boundary in *the provider*, which says
    // nothing about `text`: an Alibaba listing whose title opens in CJK puts a continuation
    // byte at index 7, `Alibaba` is 7 bytes, and the reference board is largely Alibaba.
    let matches_here = |text: &str| {
        text.get(..provider.len()).is_some_and(|head| head.eq_ignore_ascii_case(provider))
    };

    let trimmed = title.trim();
    // Every index below comes from `match_indices`, which yields the separator itself — so the
    // text after it starts at `at + sep.len()`. Adding 1 works for `:` and `|` and **panics on
    // an en dash**, which is three bytes and is what most of the web actually uses. Caught by a
    // test on a real title: `Sony Imx678 Camera – Sincerefirst`.
    let after = |at: usize, sep: &str| &trimmed[at + sep.len()..];

    // Leading: "Amazon.com : Superbat …"
    if matches_here(trimmed)
        && let Some((at, sep)) = trimmed.match_indices(SEPARATORS).next()
        // **The separator has to come *after* the provider.** A provider is only a name until it
        // contains one of these: the fallback naming rule capitalises the registrable domain
        // label, and a hyphen is legal in one — so `acme-parts.com` yields `Acme-parts`, whose
        // own hyphen is the first match in the title, and `trimmed[10..5]` is a reversed range.
        // Guaranteed, not occasional: `matches_here` has just established that the title *starts
        // with* the provider, so a separator inside it is always found first.
        && let Some(between) = trimmed.get(provider.len()..at).map(str::trim)
    {
        let tail = after(at, sep).trim();
        // Only across a separator, and only when what sits *between* the name and it is the
        // rest of a domain rather than words — so "Amazon Basics: …" keeps its first word.
        if tail.chars().count() >= SHORTEST_TITLE && between.len() <= 4 {
            return tail;
        }
    }
    // Trailing: "IQL-IMX678/FF | DigiKey Electronics"
    for (at, sep) in trimmed.match_indices(SEPARATORS).collect::<Vec<_>>().into_iter().rev() {
        if matches_here(after(at, sep).trim()) {
            let head = trimmed[..at].trim();
            if head.chars().count() >= SHORTEST_TITLE {
                return head;
            }
        }
    }
    trimmed
}

/// Whether a card's description only repeats its title, and so is worth no room.
///
/// **18 of the 91 link cards on the reference board carry a `description` byte-identical to
/// their `title`, and 15 of those are alibaba.com.** Measured from the capture, not estimated.
/// That is Miro's data rather than anybody's bug — Miro renders one of them; we rendered both,
/// and the user photographed a card saying the same sixty-word sentence twice.
///
/// A naive `==` misses the second shape, which is just as common: a description that is the
/// title **truncated** to about 256 characters and ended with an ellipsis, sometimes differing
/// in HTML entity spelling as well (`&#43;` against `+`) — which is why `normalise` folds
/// punctuation rather than comparing bytes. So the test is *prefix in either direction* —
/// Miro stores the truncated one in `description` on some cards and a description that runs
/// *past* the title on others.
///
/// ⚠ **Both halves of the final guard earn their place, and the ratio is the one that is easy
/// to leave out.** Without the floor, a card with a two-word title would eat a real
/// description. Without the ratio, a title of `Alibaba.com` swallows the blurb *"Alibaba.com is
/// the world's largest…"* — a real description, thrown away for sharing eleven characters. The
/// duplicate this exists for is two strings of nearly the same length; a prefix that is a small
/// fraction of the whole is a blurb that happens to open with the title, which is ordinary and
/// worth keeping.
pub fn says_the_same_as(title: Option<&str>, description: &str) -> bool {
    /// Case-folded, entity-and-punctuation-agnostic, whitespace-collapsed.
    fn normalise(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut spaced = true;
        for character in text.chars() {
            if character.is_alphanumeric() {
                out.extend(character.to_lowercase());
                spaced = false;
            } else if !spaced {
                out.push(' ');
                spaced = true;
            }
        }
        out.trim_end().to_owned()
    }

    /// The shortest prefix worth calling a repeat.
    ///
    /// Sixteen characters is enough to be past *"Buy"* and *"Amazon.com:"*, and short enough to
    /// catch the truncated case — which diverges from the title only at its very end.
    const ENOUGH: usize = 16;
    let Some(title) = title else { return false };
    let (title, description) = (normalise(title), normalise(description));
    if !(title.starts_with(&description) || description.starts_with(&title)) {
        return false;
    }
    let (shorter, longer) = (title.len().min(description.len()), title.len().max(description.len()));
    shorter >= ENOUGH && shorter * 2 >= longer
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_site_affix: the two aborts ────────────────────────────────────────────────

    /// ⚠ **The reversed range.** `acme-parts.com` names itself `Acme-parts`, whose own hyphen
    /// is the first separator in the title — so the old `trimmed[provider.len()..at]` was
    /// `[10..5]`. Guaranteed rather than occasional, because the leading arm has only been
    /// entered at all once the title is known to *start with* the provider.
    ///
    /// **The assertion is that it returns at all.** Under `panic = "abort"` the failure this
    /// pins is not a wrong string, it is no process — so the expected value is the *unchanged*
    /// title, which is what `get` answering `None` correctly degrades to. A card that declines
    /// to shorten its title is the price of a board that opens.
    #[test]
    fn a_provider_containing_a_separator_does_not_reverse_a_range() {
        let title = "Acme-parts - Brake Discs for the E60";
        assert_eq!(strip_site_affix(title, Some("Acme-parts")), title);
        // The same shape with every separator the table knows, since each has its own byte
        // length and the arithmetic differs for all of them.
        for separator in [':', '|', '-', '\u{2013}', '\u{2014}', '\u{00BB}'] {
            let title = format!("Acme-parts {separator} Brake Discs");
            assert_eq!(strip_site_affix(&title, Some("Acme-parts")), title);
        }
    }

    /// ⚠ **The non-boundary.** `Alibaba` is 7 bytes and a CJK character is 3, so a title opening
    /// in Chinese puts a continuation byte at index 7 — and `&text[..7]` aborts there. This
    /// board is largely Alibaba.
    ///
    /// The comment on the version of this test that shipped before the fix claimed *"100 is not
    /// a multiple of three"* about an index that was exactly 33 × 3, so the walk it existed to
    /// exercise never ran. This one states its own arithmetic: the title's first character is
    /// three bytes, and 7 is inside its third.
    #[test]
    fn a_cjk_title_under_a_seven_byte_provider_does_not_split_a_character() {
        let title = "汽车零件供应商 - 高质量";
        assert_eq!("汽".len(), 3, "the premise: 7 falls inside the third character");
        assert!(!title.is_char_boundary(7));
        assert_eq!(strip_site_affix(title, Some("Alibaba")), title, "nothing to strip");
        // The trailing arm indexes the *tail* by the same length, so a CJK run after the
        // separator is the second way in. `get` answers `None` there too.
        let led = "阿里巴巴 - 汽车零件";
        assert_eq!(strip_site_affix(led, Some("Alibaba")), led);
    }

    // ── strip_site_affix: what it refuses ───────────────────────────────────────────────

    /// Without the floor, a page whose title is barely more than the site's own name is left
    /// with nothing at all — and a card with an empty title where the real one was is much
    /// worse than a card that repeats itself. Both arms have their own floor and both are
    /// checked, because a fix applied to one of two arms is this repository's signature defect.
    #[test]
    fn a_title_is_never_left_shorter_than_the_floor() {
        // Leading: stripping would leave "Go", two characters.
        assert_eq!(strip_site_affix("GitHub: Go", Some("GitHub")), "GitHub: Go");
        assert_eq!(strip_site_affix("GitHub: Gos", Some("GitHub")), "Gos", "three is enough");
        // Trailing: stripping would leave "Hi".
        assert_eq!(strip_site_affix("Hi | DigiKey", Some("DigiKey")), "Hi | DigiKey");
        assert_eq!(strip_site_affix("Hub | DigiKey", Some("DigiKey")), "Hub", "three is enough");
        // A title that is *only* the site's name has no separator to strip across at all, so it
        // survives by the other guard. Both matter: this is the DigiKey page the doc names.
        assert_eq!(strip_site_affix("DigiKey", Some("DigiKey")), "DigiKey");
    }

    /// Without the separator requirement, *"Amazonian Fish"* on amazon.com loses its first word.
    #[test]
    fn the_site_name_is_only_stripped_across_a_separator() {
        assert_eq!(strip_site_affix("Amazonian Fish", Some("Amazon")), "Amazonian Fish");
        // And "Amazon Basics: Cable" keeps its first word: what sits between the provider and
        // the separator is a word, not the rest of a domain.
        assert_eq!(
            strip_site_affix("Amazon Basics: HDMI Cable", Some("Amazon")),
            "Amazon Basics: HDMI Cable"
        );
    }

    #[test]
    fn the_site_name_comes_off_either_end() {
        assert_eq!(
            strip_site_affix("Amazon.com : Superbat 3G/6G/12G SDI Cable", Some("Amazon")),
            "Superbat 3G/6G/12G SDI Cable"
        );
        assert_eq!(
            strip_site_affix("IQL-IMX678/FF | DigiKey Electronics", Some("DigiKey")),
            "IQL-IMX678/FF"
        );
        // ⚠ An en dash is three bytes, and `at + 1` panicked on exactly this real title.
        assert_eq!(
            strip_site_affix("Sony Imx678 Camera \u{2013} Sincerefirst", Some("Sincerefirst")),
            "Sony Imx678 Camera"
        );
        assert_eq!(strip_site_affix("Anything", None), "Anything", "no provider, no change");
        assert_eq!(strip_site_affix("Anything", Some("   ")), "Anything", "nor a blank one");
    }

    // ── says_the_same_as ────────────────────────────────────────────────────────────────

    /// Both shapes Miro's own data actually takes, and the guard that keeps a real blurb.
    #[test]
    fn a_description_that_only_repeats_the_title_is_suppressed() {
        let title = "Superbat 3G/6G/12G SDI Cable BNC to BNC";
        assert!(says_the_same_as(Some(title), title), "byte-identical: 18 of 91 cards");
        assert!(
            says_the_same_as(Some(title), "Superbat 3G/6G/12G SDI Cable BNC to…"),
            "the same title, truncated and ellipsised"
        );
        // ⚠ **The two guards catch different cards and both are needed.** The floor is what
        // saves the short title; the ratio is what saves a long one whose blurb genuinely
        // opens with it. A test that only covered the first would pass with the ratio deleted.
        assert!(
            !says_the_same_as(
                Some("Alibaba.com"),
                "Alibaba.com is the world's largest online commerce company"
            ),
            "the floor: an eleven-character title must not eat a real blurb"
        );
        assert!(
            !says_the_same_as(
                Some("Superbat SDI Cable"),
                "Superbat SDI Cable is a 75-ohm coaxial jumper for broadcast video equipment"
            ),
            "the ratio: an eighteen-character title clears the floor and must still not eat it"
        );
        assert!(!says_the_same_as(Some(title), "A 75-ohm coaxial jumper"), "an unrelated blurb");
        assert!(!says_the_same_as(None, title), "no title, nothing to repeat");
        assert!(!says_the_same_as(Some("Bolt"), "Bolt"), "the length floor, both sides short");
    }

    // ── the estimates ───────────────────────────────────────────────────────────────────

    /// ⚠ The measurement that `WRAP_EFFICIENCY` exists for. Without the discount this title
    /// estimates **two** lines, shapes to three, and the blurb draws through the third.
    #[test]
    fn a_sixty_two_character_title_needs_three_lines_not_two() {
        let title = "rust-lang/rust: Empowering everyone to build reliable software";
        assert_eq!(title.chars().count(), 62, "the measured case, spelled out");
        assert_eq!(estimated_lines(62, 238.0, 15.0), 3.0);
        // A/B: the same arithmetic with no discount is the bug.
        let undiscounted = ((238.0f64 / (15.0 * 0.5)).floor() as usize).max(8);
        assert_eq!(62usize.div_ceil(undiscounted), 2, "which is what overlapped the blurb");
    }

    #[test]
    fn a_degenerate_box_still_offers_a_line_to_work_with() {
        assert_eq!(chars_per_line(0.0, 13.0), 8, "the floor, not zero");
        assert_eq!(chars_per_line(-100.0, 13.0), 8, "and a negative box is not a huge one");
        assert_eq!(estimated_lines(0, 200.0, 13.0), 1.0, "no characters still occupies a line");
        assert!(line_budget(200.0, 0.0, 13.0) >= chars_per_line(200.0, 13.0));
    }

    /// ⚠ An exact fit is the *common* case, not an edge one — these heights are built by adding
    /// and subtracting the same numbers several times.
    #[test]
    fn a_box_sized_for_exactly_one_line_is_offered_one() {
        let line = 13.0 * CARD_LINE_HEIGHT;
        assert!((whole_lines(line, line) - line).abs() < 1e-9);
        // The measured near-miss: a hair under, from float accumulation.
        assert!((whole_lines(line - 1e-13, line) - line).abs() < 1e-9, "floor would answer zero");
        // A genuine shortfall is still refused. Two lines is 2.7 units of line height apart, so
        // a whole unit short is unambiguous.
        assert_eq!(whole_lines(line - 1.0, line), 0.0);
        assert!((whole_lines(line * 2.6, line) - line * 2.0).abs() < 1e-9, "snaps down");
        assert_eq!(whole_lines(50.0, 0.0), 50.0, "a degenerate line height is not a divide");
        assert_eq!(whole_lines(-5.0, 10.0), 0.0);
    }

    #[test]
    fn a_clipped_run_ends_in_an_ellipsis_on_a_character_boundary() {
        assert_eq!(ellipsise("anything at all", 0), "", "no room means nothing, not everything");
        assert_eq!(ellipsise("anything at all", 1), "…");
        assert_eq!(ellipsise("short", 20), "short", "nothing to do");
        // Cut at the last word boundary in what was kept, rather than mid-syllable.
        assert_eq!(ellipsise("Empowering everyone to build", 20), "Empowering…");
        // …unless that would throw most of the row away, which is the `budget / 2` guard: here
        // the boundary would leave two characters of a budget of ten, so the hard cut wins.
        assert_eq!(ellipsise("Ab cdefghijklmnop", 10), "Ab cdefgh…");
        // A multi-byte cut. `chars`, so this is a count of characters and never of bytes.
        let cjk = "汽车零件供应商供应商";
        let cut = ellipsise(cjk, 5);
        assert_eq!(cut.chars().count(), 5, "four characters and the ellipsis");
        assert!(cjk.starts_with(cut.trim_end_matches('…')));
    }

    // ── the stack ───────────────────────────────────────────────────────────────────────

    /// The reference card: 250 × 361 at 13 units, with a picture and a badge.
    fn reference(title_chars: usize, has_blurb: bool) -> CardStack {
        let (w, h, size) = (250.0, 361.0, 13.0);
        let pad = w * CARD_PADDING;
        let picture_bottom = pad + (h * 0.46 - pad).max(1.0);
        let badge = Some((w - pad - 22.75, pad, 22.75, 22.75));
        stack(w, h, size, Some(picture_bottom), badge, title_chars, has_blurb)
    }

    /// ⚠ The band the user photographed. A one-line title must leave no empty lines above the
    /// blurb; a three-line title must get three.
    #[test]
    fn a_short_title_reserves_one_line_and_a_long_one_reserves_three() {
        let line = 13.0 * TITLE_SCALE * CARD_LINE_HEIGHT;
        let short = reference(9, true);
        assert!((short.title.3 - line).abs() < 0.01, "one line: {:.2}", short.title.3);
        let long = reference(400, true);
        assert!(
            (long.title.3 - line * TITLE_LINES).abs() < 0.01,
            "capped at {TITLE_LINES}: {:.2}",
            long.title.3
        );
        // A/B against the flat reservation this replaced: the blurb of a one-line title used to
        // begin three title lines down instead of one.
        let flat = short.title.1 + line * TITLE_LINES + 250.0 * CARD_PADDING * TITLE_GAP;
        assert!(short.blurb.1 < flat - line, "the blurb hangs off the title it actually has");
    }

    /// Both boxes are a whole number of their own lines, which is what keeps the last one inside
    /// the card. A box 2.4 lines tall draws a third line 40% in and the rest of the way out.
    #[test]
    fn both_boxes_hold_a_whole_number_of_lines_and_stay_inside_the_card() {
        let title_line = 13.0 * TITLE_SCALE * CARD_LINE_HEIGHT;
        let blurb_line = 13.0 * BLURB_SCALE * CARD_LINE_HEIGHT;
        for chars in [4usize, 30, 90, 400] {
            let laid = reference(chars, true);
            let titles = laid.title.3 / title_line;
            let blurbs = laid.blurb.3 / blurb_line;
            assert!((titles - titles.round()).abs() < 1e-6, "title is {titles:.4} lines");
            assert!((blurbs - blurbs.round()).abs() < 1e-6, "blurb is {blurbs:.4} lines");
            assert!(blurbs <= BLURB_LINES + 1e-6, "blurb capped: {blurbs:.2}");
            let foot = laid.blurb.1 + laid.blurb.3;
            assert!(foot <= 361.0, "the last line ends inside the card: {foot:.2}");
            // …with air under it, so the words do not rest on the edge.
            assert!(361.0 - foot >= blurb_line * BLURB_BOTTOM_AIR - 0.01, "air below the blurb");
        }
    }

    /// ⚠ The badge pushes the words aside **only when they share a row**, and the body has to
    /// yield as well as the provider row — the badge is 1.75 line-heights tall against a row of
    /// one, so on a card with no picture it reaches into the title's first line.
    #[test]
    fn the_words_yield_to_the_badge_only_where_it_actually_is() {
        let (w, h, size) = (250.0, 200.0, 13.0);
        let pad = w * CARD_PADDING;
        let side = size * 1.75;
        let badge = Some((w - pad - side, pad, side, side));

        // No picture: the words start at the top, where the badge is. Both rows yield.
        let bare = stack(w, h, size, None, badge, 60, true);
        let full = (w - pad * 2.0).max(1.0);
        assert!(bare.provider.2 < full, "the site name stops before the badge");
        assert!(bare.title.2 < full, "and so does the title's first line");

        // With a picture, the words start far below it and nothing is shortened. Clipping them
        // anyway is the version that makes every title on the board mysteriously narrow.
        let with_picture = stack(w, h, size, Some(h * 0.46), badge, 60, true);
        assert!((with_picture.provider.2 - full).abs() < 1e-9, "full width under the picture");
        assert!((with_picture.title.2 - full).abs() < 1e-9);

        // And a card with no address wears no badge, so nothing yields at either height.
        let unbadged = stack(w, h, size, None, None, 60, true);
        assert!((unbadged.provider.2 - full).abs() < 1e-9);
        assert!((unbadged.title.2 - full).abs() < 1e-9);
    }

    /// A card with no blurb — a video, by content rule — gets a zero-height box rather than a
    /// short one. `TextLayer` and `Painter::block` both read that as "draw nothing"; a one-unit
    /// box would shape a line and clip it, which is a grey smear under the title.
    #[test]
    fn a_card_with_nothing_to_say_gets_no_blurb_box() {
        assert_eq!(reference(30, false).blurb.3, 0.0);
        assert!(reference(30, true).blurb.3 > 0.0, "and one with a blurb does get a box");
    }

    /// A card dragged down to nothing must not produce inverted or negative boxes. Nothing here
    /// may abort, and nothing may hand a painter a box it has to guess about.
    #[test]
    fn a_degenerate_card_yields_boxes_rather_than_nonsense() {
        for (w, h) in [(0.0, 0.0), (1.0, 400.0), (600.0, 26.0), (-10.0, -10.0)] {
            let laid = stack(w, h, 13.0, None, None, 40, true);
            for (name, (_, _, bw, bh)) in
                [("provider", laid.provider), ("title", laid.title), ("blurb", laid.blurb)]
            {
                assert!(bw >= 0.0 && bw.is_finite(), "{name} width {bw} at {w}x{h}");
                assert!(bh >= 0.0 && bh.is_finite(), "{name} height {bh} at {w}x{h}");
            }
        }
    }
}
