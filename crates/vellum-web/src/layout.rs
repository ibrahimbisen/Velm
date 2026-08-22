//! Where an item's words and its picture go inside its box.
//!
//! One module, and one function per question, because the answers differ per kind and every
//! one of them was wrong when the frame loop simply used the item's own rectangle for both:
//! a sticky's text ran to the very edge of the paper, a frame's name was set enormous *inside*
//! the frame instead of small above it, and a link card's thumbnail was stretched across the
//! whole card instead of filling a band at the top.
//!
//! This is `draw.rs`'s `block` and `card_layout` reduced to the kinds a reader needs. It is
//! not the painter — a card is three voices there and one here, and the 41 SDF shapes are a
//! quad here — but the *geometry* is the same geometry, taken from the same constants.

use vellum_doc::ItemKind;
use vellum_project::project::Projected;
use vellum_render::{Rgba, UvRect};
use vellum_scene::WorldRect;

/// Fraction of a sticky's box left as padding on each side.
///
/// Miro's own inset, and `vellum_app::text::STICKY_PADDING` is the same 0.08. Without it a
/// sticky's words touch its edges, which is the single thing that most makes a note look
/// wrong — paper has a margin.
const STICKY_PADDING: f64 = 0.08;

/// A frame's name: a fraction of the frame's height, clamped, drawn **above** it.
///
/// ⚠ Above, not inside. Miro puts a frame's name over its top edge, and so does the desktop
/// app (`Anchor::Above`). Drawing it inside makes the largest thing on the board wear a
/// caption across its own content — and because the size scales with the frame, a large frame
/// gets an enormous one.
const FRAME_TITLE_FRACTION: f64 = 0.03;
const FRAME_TITLE_MIN: f64 = 14.0;
const FRAME_TITLE_MAX: f64 = 96.0;

/// Padding inside a link card, as a fraction of its width.
const CARD_PADDING: f64 = 0.035;

/// How much of a card's height its picture takes.
///
/// The desktop app's two card modes, 46% and 62%. A reader has no mode control, so it takes
/// the smaller: a card that reserves 62% and has no picture draws its text more than halfway
/// down over empty surface, which reads as broken rather than as waiting.
const CARD_IMAGE_FRACTION: f64 = 0.46;

/// How a block sits in the box it was given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anchor {
    /// Centred both ways. A sticky, and a shape's label.
    Centred,
    /// The box's own top-left. A text widget, a card's words, a frame's name.
    TopLeft,
}

/// Where an item's words go, in **world** units.
pub struct TextSlot {
    pub rect: WorldRect,
    pub anchor: Anchor,
    pub color: Rgba,
    /// The style's own size, or `None` for auto-fit.
    pub font_size: Option<f32>,
}

/// Where an item's picture goes, in **world** units.
pub struct PictureSlot {
    pub hash: String,
    pub rect: WorldRect,
    /// Crop to the slot's aspect rather than stretching to it.
    pub cover: bool,
}

/// The box an item's text is set in, or `None` if it has none worth setting.
pub fn text_slot(projected: &Projected, text: Rgba, muted: Rgba) -> Option<TextSlot> {
    let bounds = projected.bounds;
    let (w, h) = (bounds.width(), bounds.height());
    let colour = projected.item.style.text_color.map_or(text, vellum_project::theme::convert);
    let explicit = projected.item.style.font_size.map(|s| s as f32);

    match &projected.item.kind {
        ItemKind::Sticky { .. } | ItemKind::Shape { .. } => Some(TextSlot {
            rect: inset(bounds, w * STICKY_PADDING, h * STICKY_PADDING),
            anchor: Anchor::Centred,
            color: colour,
            font_size: explicit,
        }),

        // ⚠ Above the frame, at a size derived from the frame rather than from the words —
        // and **never auto-fitted**, which is what made it enormous: auto-fit asks for the
        // largest size that fits the box, and a frame's box is the biggest thing on the board.
        ItemKind::Frame { title, .. } => {
            if title.is_empty() {
                return None;
            }
            let size = (h * FRAME_TITLE_FRACTION).clamp(FRAME_TITLE_MIN, FRAME_TITLE_MAX);
            Some(TextSlot {
                rect: WorldRect::from_origin_size(
                    vellum_scene::WorldPoint::new(bounds.min.x, bounds.min.y - size * 1.2),
                    w,
                    size * 1.2,
                ),
                anchor: Anchor::TopLeft,
                color: muted,
                font_size: Some(size as f32),
            })
        }

        // A card's words sit under its picture band, inset, at a size the card chooses —
        // **explicitly, not auto-fitted**. A card's text is a label at a fixed scale, and
        // auto-fitting it makes a short title enormous and a long one microscopic.
        ItemKind::LinkPreview { .. } | ItemKind::Embed { .. } | ItemKind::Document { .. } => {
            let pad = w * CARD_PADDING;
            let band = if has_picture(&projected.item.kind) { h * CARD_IMAGE_FRACTION } else { pad };
            let top = bounds.min.y + band + pad;
            let height = (bounds.max.y - pad - top).max(1.0);
            Some(TextSlot {
                rect: WorldRect::from_origin_size(
                    vellum_scene::WorldPoint::new(bounds.min.x + pad, top),
                    (w - pad * 2.0).max(1.0),
                    height,
                ),
                anchor: Anchor::TopLeft,
                color: colour,
                font_size: Some(explicit.unwrap_or((h * 0.075).clamp(9.0, 22.0) as f32)),
            })
        }

        _ => projected.item.kind.text().map(|_| TextSlot {
            rect: bounds,
            anchor: Anchor::TopLeft,
            color: colour,
            font_size: explicit,
        }),
    }
}

/// The box an item's picture goes in, or `None` if it has none.
pub fn picture(projected: &Projected) -> Option<PictureSlot> {
    let bounds = projected.bounds;
    match &projected.item.kind {
        // An image item's box *is* its picture's box: the importer sizes one from the other,
        // so the aspects already agree and stretching is not a risk. `UvRect::FULL` is what
        // the desktop app uses here too.
        ItemKind::Image { asset_id, .. } => Some(PictureSlot {
            hash: asset_id.clone(),
            rect: bounds,
            cover: false,
        }),

        // ⚠ A card's picture fills a **band**, and its aspect has nothing to do with the
        // band's — a banner is 1.90 and a product shot is 1.00 against a band near 1.47. So
        // it is cropped to fit rather than scaled to fit, which is `object-fit: cover` and is
        // feedback 23's fix. Stretching each axis by a different amount is what made every
        // picture on the board look smeared.
        ItemKind::LinkPreview { thumbnail: Some(hash), .. } => {
            let pad = bounds.width() * CARD_PADDING;
            Some(PictureSlot {
                hash: hash.clone(),
                rect: WorldRect::from_origin_size(
                    vellum_scene::WorldPoint::new(bounds.min.x + pad, bounds.min.y + pad),
                    (bounds.width() - pad * 2.0).max(1.0),
                    (bounds.height() * CARD_IMAGE_FRACTION - pad).max(1.0),
                ),
                cover: true,
            })
        }
        _ => None,
    }
}

fn has_picture(kind: &ItemKind) -> bool {
    matches!(kind, ItemKind::LinkPreview { thumbnail: Some(_), .. })
}

fn inset(rect: WorldRect, x: f64, y: f64) -> WorldRect {
    let (w, h) = ((rect.width() - x * 2.0).max(1.0), (rect.height() - y * 2.0).max(1.0));
    WorldRect::from_origin_size(vellum_scene::WorldPoint::new(rect.min.x + x, rect.min.y + y), w, h)
}

/// The part of a texture that fills `into` without distorting it — `object-fit: cover`.
///
/// Trims the *longer* axis equally at both ends, so a centred subject stays centred. Cover
/// rather than contain: both fix the distortion, and `contain` leaves empty bands in a slot
/// that was sized for a picture, which reads as a layout bug. `draw.rs`'s `cover_uv`, same
/// arithmetic.
pub fn cover_uv(source: (u32, u32), into: (f64, f64)) -> UvRect {
    let (sw, sh) = (f64::from(source.0).max(1.0), f64::from(source.1).max(1.0));
    let (iw, ih) = (into.0.max(1.0), into.1.max(1.0));
    let source_aspect = sw / sh;
    let target_aspect = iw / ih;
    if source_aspect > target_aspect {
        // Wider than the slot: keep full height, trim the sides.
        let keep = (target_aspect / source_aspect) as f32;
        let margin = (1.0 - keep) / 2.0;
        UvRect::new([margin, 0.0], [margin + keep, 1.0])
    } else {
        let keep = (source_aspect / target_aspect) as f32;
        let margin = (1.0 - keep) / 2.0;
        UvRect::new([0.0, margin], [1.0, margin + keep])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The assertion is about the **sampled aspect in texels**, not the UV numbers.
    ///
    /// `UvRect::FULL` happens to have the slot's aspect whenever the source already does,
    /// which is the case that used to pass by accident. Measuring what is sampled is what
    /// tells a working crop from a lucky one.
    #[test]
    fn a_picture_is_cropped_to_its_slot_rather_than_stretched_into_it() {
        for (source, into) in
            [((1920u32, 1080u32), (300.0, 200.0)), ((600, 800), (300.0, 200.0)), ((1, 4000), (300.0, 200.0))]
        {
            let uv = cover_uv(source, into);
            let texels = (
                (uv.max[0] - uv.min[0]) as f64 * f64::from(source.0),
                (uv.max[1] - uv.min[1]) as f64 * f64::from(source.1),
            );
            let sampled = texels.0 / texels.1;
            let wanted = into.0 / into.1;
            assert!(
                (sampled / wanted - 1.0).abs() < 0.01,
                "{source:?} into {into:?} sampled {sampled:.3} against {wanted:.3}"
            );
        }
    }

    #[test]
    fn a_full_crop_is_what_a_matching_aspect_produces() {
        let uv = cover_uv((300, 200), (600.0, 400.0));
        assert!(uv.min[0].abs() < 0.001 && (uv.max[0] - 1.0).abs() < 0.001);
        assert!(uv.min[1].abs() < 0.001 && (uv.max[1] - 1.0).abs() < 0.001);
    }
}
