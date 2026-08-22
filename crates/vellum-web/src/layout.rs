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
use vellum_render::Rgba;
use vellum_scene::WorldRect;

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
            let size = frame_title_size(h);
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

/// Re-exported so this module reads as one place, while the values live in one place.
///
/// ⚠ These are **not** defined here. `vellum_project::look` owns them and the desktop
/// painter reads the same constants — which is the point: a browser board that insets a
/// sticky differently from the Mac app is two derivations of one measurement, and this
/// repository has paid for that three times.
pub use vellum_project::look::{
    CARD_PADDING, STICKY_PADDING, cover_uv, frame_title_size,
};
