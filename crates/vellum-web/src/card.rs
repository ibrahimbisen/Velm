//! A link card's words: which blocks of text go where, in world units.
//!
//! # Why this module exists at all
//!
//! A card was drawing as a **blank white rectangle with a ↗ in the corner**. Everything else
//! about it worked — the picture band, the badge, the plate — and the words were simply never
//! queued, because the frame loop's text pass is driven by `projected.item.kind.text()` and
//! [`vellum_doc::ItemKind::text`] answers `None` for `LinkPreview`, `Embed` and `Document`. That
//! is deliberate on the document's side and it is right: a card's title is metadata scraped from
//! someone else's page, not text the user wrote, and folding it into `text()` would make a
//! search for *"Cooling"* match a video nobody on this board named. But it means a card's words
//! have to be *asked for*, and until this module nothing asked. `crate::layout::text_slot`'s
//! card arm had been written, reasoned about at length, and never once called — this
//! repository's signature defect, and on the reference board it accounted for **91 items**.
//!
//! # Three blocks, and it cannot be fewer
//!
//! `DrawList::push_layout` takes one colour *and* one size for a whole run, and
//! `vellum_text::SpanStyle` has no size field at all. Miro's card is a muted site name, a bold
//! near-black title and a smaller grey blurb — three tones *and* three sizes — so it is three
//! queued blocks. Doing it in one would need per-span colour and per-span size in the glyph
//! pass, which is a renderer change for a card. `vellum-app`'s painter reaches the identical
//! conclusion for the identical reason.
//!
//! # Where the decisions live
//!
//! The **arithmetic** is [`vellum_project::card`], not here: this crate is
//! `#![cfg(target_arch = "wasm32")]`, so `cargo test` compiles it to an empty crate and nothing
//! in it can have a runnable test. Two of the string functions a card needs have aborted the
//! application on real board data, and `panic = "abort"` means an abort on the paint path takes
//! the tab rather than raising anything. What is left here is the part that genuinely is about
//! this front end: reading the document's fields, and putting the shared stack's boxes into
//! world coordinates.

use vellum_doc::ItemKind;
use vellum_project::card::{
    BLURB_SCALE, PROVIDER_SCALE, TITLE_SCALE, ellipsise, line_budget, says_the_same_as, stack,
    strip_site_affix,
};
use vellum_project::project::Projected;
use vellum_render::Rgba;
use vellum_scene::{WorldPoint, WorldRect};
use vellum_text::{SpanStyle, StyledText, TextSpan};

/// Which of a card's three labels a block is.
///
/// ⚠ **A slot is what makes two blocks on one item distinguishable.** `TextLayer`'s cache key is
/// `(item, slot, generation, size, width)`, so two blocks of the same width at the same size
/// would otherwise resolve to one entry and the second would draw the first one's words. Here
/// the three *are* different sizes, so a missing slot would not show today — which is exactly
/// the kind of latent key collision that surfaces the day somebody makes two of the scales
/// equal. `crate::widgets::WidgetText` carries the same field for the same reason.
///
/// Numbered from zero, and that is safe rather than lucky: the generic `kind.text()` path in the
/// frame loop also uses slot 0, and it is **unreachable for these three kinds** because
/// `ItemKind::text()` answers `None` for all of them — which is the very gap this module fills.
const PROVIDER_SLOT: u16 = 0;
const TITLE_SLOT: u16 = 1;
const BLURB_SLOT: u16 = 2;

/// One of a card's voices, ready for `TextLayer::queue`.
///
/// Deliberately the same shape as [`crate::widgets::WidgetText`], field for field: the frame
/// loop queues both through the same call, so a gratuitous difference between them is friction
/// at the one place a reader is comparing them.
///
/// Owned throughout — the strings are decoded, stripped and clipped copies of the document's
/// fields rather than borrows of them, so handing out references would keep the projection
/// borrowed across a call that needs the text engine as well.
pub struct CardBlock {
    /// Distinguishes this block from the other two on the same card. See above.
    pub slot: u16,
    /// The box the words go in, in absolute world coordinates.
    pub rect: WorldRect,
    /// Already decoded, already stripped of the site's own name, already clipped to the box.
    pub text: StyledText,
    /// Explicit, never auto-fitted. A card's text is a *label* at a fixed scale; auto-fit asks
    /// "how big can this be and still fit", which for a collapsed row holding six words answers
    /// forty points and fills the card.
    pub font_size: f32,
    pub color: Rgba,
    pub anchor: crate::layout::Anchor,
}

/// Every block a card draws, in paint order. Empty for anything that is not a card.
///
/// # What is deliberately *not* honoured, and why
///
/// **`CardMode`.** `vellum-app` draws three forms — a collapsed one-line `Link`, a `Card` and a
/// picture-led `Large` — and this draws one. Two reasons, and the first is the binding one:
/// [`crate::layout::picture`] draws a card's thumbnail band with no reference to the mode at all,
/// so words laid out for a *different* form would float above or below the picture that is
/// actually on screen. There is no mode control in a read-only viewer to make the choice
/// meaningful either. The cost is bounded and known: a `Link`-mode card draws as a full card
/// rather than as one row — and `draw.rs` records that **import never produces `Link`**, because
/// Miro stores a collapsed link as a *text* widget rather than as a card, which is why 46 of the
/// reference board's text items are bare URLs.
///
/// **The video poster layout.** On the desktop a video card measures its caption and gives the
/// picture everything left over. Same reason: the band here is drawn at a fixed fraction. The
/// half of that behaviour which is a *content* rule rather than a layout one — a video's blurb is
/// the uploader's sponsor read and is worth no room — is honoured, because it costs no geometry.
///
/// **The favicon.** Nothing in this client draws one, so the provider row is not indented for it.
/// Indenting beside nothing is a hole in a row whose whole job is to be scannable. If favicons
/// are ever drawn here, the indent belongs in [`stack`] as a parameter, exactly as
/// `vellum-app`'s layout takes it — not as a second adjustment applied afterwards.
pub fn blocks(projected: &Projected, text: Rgba, muted: Rgba) -> Vec<CardBlock> {
    // ⚠ **The gate, and it is deliberately somebody else's list.** `crate::layout::card_font_size`
    // answers `Some` for exactly the kinds `layout::is_card` calls cards, and `None` otherwise —
    // so this cannot come to disagree with the module that decides where a card's picture goes.
    // Writing `matches!(kind, LinkPreview | Embed | Document)` here would be a second list, and
    // this repository has paid three times for the day one copy is updated and the other is not.
    let Some(size) = crate::layout::card_font_size(projected) else { return Vec::new() };
    let size = f64::from(size);
    let bounds = projected.bounds;
    let (width, height) = (bounds.width(), bounds.height());

    let fields = card_fields(&projected.item.kind);

    // ⚠ **Measured, never derived.** The picture band's bottom comes from the slot the painter
    // is actually filling, so the words cannot start above or below the picture they caption.
    // Computing a fraction here instead would be a second answer to a question `layout::picture`
    // has already answered — and it answers `None` for an `Embed` with a thumbnail, which is a
    // real gap in that module rather than in this one. Reading it means a card whose picture is
    // not drawn correctly starts its words at the top instead of below empty surface.
    let picture_bottom =
        crate::layout::picture(projected).map(|slot| slot.rect.max.y - bounds.min.y);
    let badge = crate::badges::badge(projected)
        .map(|r| (r.min.x - bounds.min.x, r.min.y - bounds.min.y, r.width(), r.height()));

    // The three strings, resolved *before* any geometry, because two of the boxes depend on
    // them: the title's reservation is taken from the title's own drawn length, and the blurb's
    // box is zero when there is nothing to put in it.
    let title = drawn_title(&fields);
    let blurb = blurb_text(&fields);
    let stacked = stack(
        width,
        height,
        size,
        picture_bottom,
        badge,
        // ⚠ **The drawn length, not the stored one.** The painter decodes entities and strips
        // the site affix before it shapes, so `&#39;` is five stored characters and one drawn
        // one, and *"Amazon.com : Superbat…"* loses its first eleven. Counting the stored string
        // over-reserves, which is a band of empty card under a one-line title.
        title.as_deref().map_or(0, |t| t.chars().count()),
        blurb.is_some(),
    );

    let mut out = Vec::with_capacity(3);
    // The site name, muted and smallest. Its own row, so it can be a different colour *and* a
    // different size from the title — which is the whole reason a card is three blocks.
    if let Some(provider) = fields.provider.as_deref().filter(|p| !p.trim().is_empty()) {
        push(&mut out, PROVIDER_SLOT, bounds, stacked.provider, size * PROVIDER_SCALE, muted, |b| {
            StyledText::plain(ellipsise(provider, b))
        });
    }
    // The title: the card's own ink, and **bold**. Two doc comments in the desktop painter
    // described "a large bold title" for a year while both of its arms wrote a plain span, so
    // the hierarchy rested on colour alone and the card read as one paragraph. Real weight
    // rather than a different face — Inter Bold is bundled and is what `sans-serif` resolves to,
    // so `family_has_bold` no longer drops the request and silently leaves the family (trap 10).
    if let Some(title) = title.as_deref() {
        push(&mut out, TITLE_SLOT, bounds, stacked.title, size * TITLE_SCALE, text, |b| {
            StyledText::from_spans([TextSpan::new(ellipsise(title, b), SpanStyle::bold())])
        });
    }
    if let Some(blurb) = blurb.as_deref() {
        push(&mut out, BLURB_SLOT, bounds, stacked.blurb, size * BLURB_SCALE, muted, |b| {
            StyledText::plain(ellipsise(blurb, b))
        });
    }
    out
}

/// Puts one block into world coordinates and clips its words to the room it has.
///
/// The text is built from a closure rather than passed in, so the **clip budget cannot be
/// computed anywhere but here**: it is a function of the box, and a caller that worked it out
/// from the item instead is how a title reserved for two lines gets cut to one.
///
/// A box with no height draws nothing, and that is a legitimate answer rather than a degenerate
/// one — a card too short for a blurb should lose the blurb rather than truncate the name of the
/// thing. Dropping the block here is cheaper than queueing one the text layer would reject, and
/// it is also **safer**: `FitBox` clamps a zero to one, so an empty box still shapes a line and
/// draws it clipped, which is a grey smear under the title rather than nothing.
fn push(
    out: &mut Vec<CardBlock>,
    slot: u16,
    bounds: WorldRect,
    (x, y, w, h): (f64, f64, f64, f64),
    font_size: f64,
    color: Rgba,
    build: impl FnOnce(usize) -> StyledText,
) {
    if w <= 0.0 || h <= 0.0 || font_size <= 0.0 {
        return;
    }
    let text = build(line_budget(w, h, font_size));
    if text.is_empty() {
        return;
    }
    // Cast out here rather than in the initialiser below: a lint attribute on a struct-literal
    // field is a place attributes are easy to get wrong, and the rest of this crate spells a
    // world-unit-to-shaper cast exactly this plainly.
    let font_size = font_size as f32;
    out.push(CardBlock {
        slot,
        // ⚠ **World units, and nothing here multiplies by the zoom.** The board view's clip
        // transform already carries it. Pre-multiplying was the worst bug of the whole port —
        // one wrong multiplication made every picture on the board invisible *and* blurry at
        // once, because the same number also tells the renderer how many texels to ask for.
        rect: WorldRect::from_origin_size(
            WorldPoint::new(bounds.min.x + x, bounds.min.y + y),
            w,
            h,
        ),
        text,
        font_size,
        color,
        // Top-left, never centred: a card is a stack of left-aligned labels, and centring the
        // title of a two-line card in a three-line box would put it where the reservation is
        // rather than where the picture above it ends.
        anchor: crate::layout::Anchor::TopLeft,
    });
}

/// A card's four text fields, decoded.
///
/// ⚠ **Entities are decoded here, at paint time, and that is the third place it happens on
/// purpose.** The importer decodes what a paste brings in and `vellum-link` decodes what a fetch
/// brings back, and **neither touches text already written into the boards on disk** — a
/// YouTube card was photographed reading `wouldn&#39;t` on a build carrying both of those fixes.
/// Doing it at paint repairs every existing board with no migration and no write, which matters
/// more than the tidiness of a single decode point: a board is production data belonging to
/// someone else, and a display-time fix cannot corrupt one. It is idempotent — a decoded string
/// holds no entity for a second pass to find — and it is three short strings on the cards that
/// are on screen.
///
/// `vellum_link::decode_entities` rather than a copy: it is one left-to-right pass, which cannot
/// have the `&amp;lt;` ordering bug **by shape** rather than by order, because output is
/// appended and never re-examined so an `&` it produces is not a candidate for the next match.
/// It is already tested there.
struct CardFields {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    provider: Option<String>,
}

fn card_fields(kind: &ItemKind) -> CardFields {
    let (title, url, description, provider) = match kind {
        ItemKind::LinkPreview { title, url, description, provider, .. }
        | ItemKind::Embed { title, url, description, provider, .. } => {
            (title.clone(), url.clone(), description.clone(), provider.clone())
        }
        // A PDF has no page of its own to have been scraped from, so it names itself. Drawn as
        // an ordinary card, which is what a document placeholder should look like.
        ItemKind::Document { page_count, .. } => (
            Some(format!("PDF · {page_count} page{}", if *page_count == 1 { "" } else { "s" })),
            None,
            None,
            None,
        ),
        _ => (None, None, None, None),
    };
    let decode = |value: Option<String>| {
        value.map(|v| vellum_link::decode_entities(&v)).filter(|v| !v.trim().is_empty())
    };
    CardFields {
        title: decode(title),
        url: decode(url),
        description: decode(description),
        provider: decode(provider),
    }
}

/// The title as it will actually be drawn, or `None` for a card with no name at all.
///
/// The site's own name comes off the front or the back — see
/// [`vellum_project::card::strip_site_affix`], which is where every guard and both of its two
/// historical aborts are documented. A card with no title falls back to its **URL**, because
/// that is the only name it has; a card with neither draws nothing, and no line is reserved.
///
/// The URL is *not* used as a title when there is one, and is not repeated under it either: the
/// title and the site already say what the card is, and a wrapped address is three lines of
/// tracking parameters.
fn drawn_title(fields: &CardFields) -> Option<String> {
    match (&fields.title, &fields.url) {
        (Some(title), _) => {
            Some(strip_site_affix(title, fields.provider.as_deref()).trim().to_owned())
                .filter(|t| !t.is_empty())
        }
        (None, Some(url)) => Some(url.clone()),
        (None, None) => None,
    }
}

/// The blurb, or `None` when there is nothing worth the room.
///
/// Three rules, each of which removes text that is worse than white space:
///
/// - **A video has none.** A video's `og:description` is the uploader's sponsor read — *"Get the
///   sponsor's app here: …"* on a real card — which is the least useful text on the board.
/// - **A description that only repeats the title is dropped**, which is 18 of the 91 link cards
///   on the reference board. That is Miro's data rather than a bug, and drawing both makes a
///   card say one thing twice in two colours. Both shapes are caught — byte-identical, and the
///   title truncated and ellipsised — see [`says_the_same_as`].
/// - **Otherwise the address is worth the room**, but only when there *is* a title: without one
///   the URL is already the title, and this would draw it twice.
fn blurb_text(fields: &CardFields) -> Option<String> {
    if fields.url.as_deref().is_some_and(vellum_link::plays_video) {
        return None;
    }
    match fields.description.as_deref() {
        Some(description) if !says_the_same_as(fields.title.as_deref(), description) => {
            Some(description.to_owned())
        }
        _ if fields.title.is_some() => fields.url.clone(),
        _ => None,
    }
}
