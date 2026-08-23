//! Note nodes on the board: the document token, and where the title and body sit.
//!
//! A note's **content is not in the document and not in this module** — it is a `.md` file
//! on disk, which is the whole feature. `vellum_agent::notes` owns reading and writing it;
//! this module owns the token that says *which* file, and the layout of the box it is drawn
//! in.
//!
//! # Why this is not a sticky
//!
//! A sticky's words are document content: they live in a Loro rich-text container, they
//! merge, and they are undone by `⌘Z`. A note's words are a file this board points at, and
//! an editor or an agent may have changed them since the last frame. The two look similar
//! on screen and are completely different underneath, which is why they are different kinds
//! rather than a flag on one.

use vellum_agent::{NoteModel, NoteScope};

pub use crate::agent::Rect;

/// The token stored in the document for a note node.
pub fn encode(model: &NoteModel) -> String {
    serde_json::to_string(model).unwrap_or_else(|error| {
        log::warn!("a note node would not encode ({error}); storing nothing");
        String::new()
    })
}

/// The note a token names, or an empty one when it cannot be read.
///
/// Never fails, for the reason [`crate::agent::decode`] gives: an unreadable token must
/// cost the node's settings, not the node.
///
/// **A decoded note with an empty path is not an error either.** It is a note that has not
/// been given a file yet, and it draws as one asking to be named — which is a better answer
/// than an item that cannot be drawn.
pub fn decode(token: &str) -> NoteModel {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable note node ({error}); drawing an empty note");
        }
        NoteModel::default()
    })
}

/// What the note tool places, in world units. Portrait, like a page.
pub const DEFAULT_SIZE: (f64, f64) = (320.0, 380.0);

/// Below this only the title is drawn.
pub const MIN_SIZE: (f64, f64) = (120.0, 64.0);

const PAD: f64 = 12.0;
const TITLE_HEIGHT: f64 = 26.0;
/// The scope chip's height, in the footer beside the file's name.
const FOOTER_HEIGHT: f64 = 18.0;

/// Where the pieces of a note node are, in the item's own space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteLayout {
    pub bounds: Rect,
    /// The note's title — the item's own `StyledText`, editable on the canvas.
    pub title: Rect,
    /// The markdown body, read from the file.
    pub body: Rect,
    /// The footer: the file's name and the scope chip. Present only when there is room,
    /// because a note whose footer ate its body would be a filename with no note under it.
    pub footer: Rect,
    pub too_small: bool,
}

impl NoteLayout {
    pub fn hit(&self, x: f64, y: f64) -> Option<NotePart> {
        if self.too_small {
            return None;
        }
        for (rect, part) in [
            (self.title, NotePart::Title),
            (self.footer, NotePart::Footer),
            (self.body, NotePart::Body),
        ] {
            if !rect.is_empty() && rect.contains(x, y) {
                return Some(part);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotePart {
    Title,
    Body,
    /// The file name and scope chip. Pressing it reveals the file.
    Footer,
}

pub fn layout(width: f64, height: f64) -> NoteLayout {
    let bounds = Rect::new(0.0, 0.0, width.max(0.0), height.max(0.0));
    if width < MIN_SIZE.0 || height < MIN_SIZE.1 {
        return NoteLayout {
            bounds,
            title: bounds.inset(PAD / 2.0),
            body: Rect::default(),
            footer: Rect::default(),
            too_small: true,
        };
    }

    let inner = bounds.inset(PAD);
    let title = Rect::new(inner.x, inner.y, inner.width, TITLE_HEIGHT);

    // The footer is dropped rather than shrunk when there is not room for it *and* a
    // readable body. A footer that squeezed the body to nothing would leave a node showing
    // its own filename and none of its contents, which is the wrong thing to keep.
    let after_title = title.y + title.height + PAD / 2.0;
    let available = inner.y + inner.height - after_title;
    let (footer, body_height) = if available > FOOTER_HEIGHT + 24.0 {
        let footer = Rect::new(
            inner.x,
            inner.y + inner.height - FOOTER_HEIGHT,
            inner.width,
            FOOTER_HEIGHT,
        );
        (footer, (footer.y - PAD / 2.0 - after_title).max(0.0))
    } else {
        (Rect::default(), available.max(0.0))
    };

    NoteLayout {
        bounds,
        title,
        body: Rect::new(inner.x, after_title, inner.width, body_height),
        footer,
        too_small: false,
    }
}

/// The short label for a note's scope, for the footer chip.
pub const fn scope_label(scope: &NoteScope) -> &'static str {
    match scope {
        NoteScope::Shared => "Shared",
        NoteScope::Private { .. } => "Private",
    }
}

/// The file's own name, for the footer.
///
/// The **last path component**, taken with `rsplit` rather than by byte-slicing at a found
/// index — a note path can carry any character a filesystem allows, and this codebase has
/// aborted twice on exactly that pattern.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_token_degrades_to_an_empty_note() {
        assert_eq!(decode("garbage"), NoteModel::default());
        assert_eq!(decode(""), NoteModel::default());

        let note = NoteModel { path: "a/b.md".into(), ..NoteModel::default() };
        assert_eq!(decode(&encode(&note)), note);
    }

    #[test]
    fn every_part_is_hittable_and_stays_inside_the_node() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        assert!(!l.too_small);
        for (rect, part) in
            [(l.title, NotePart::Title), (l.body, NotePart::Body), (l.footer, NotePart::Footer)]
        {
            assert!(!rect.is_empty(), "{part:?} laid out empty");
            assert_eq!(l.hit(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0), Some(part));
            assert!(rect.x + rect.width <= DEFAULT_SIZE.0 + 0.001, "{part:?} escaped");
            assert!(rect.y + rect.height <= DEFAULT_SIZE.1 + 0.001, "{part:?} escaped");
        }
        assert!(l.title.y + l.title.height <= l.body.y, "the title overlaps the body");
        assert!(l.body.y + l.body.height <= l.footer.y, "the body overlaps the footer");
    }

    /// The footer is dropped, not squeezed, when the body would otherwise vanish. A node
    /// showing its own filename and none of its contents keeps the wrong half.
    #[test]
    fn a_short_note_drops_its_footer_rather_than_its_body() {
        let short = layout(240.0, 70.0);
        assert!(!short.too_small);
        assert!(short.footer.is_empty(), "a 70-unit note kept a footer");
        assert!(short.body.height > 0.0, "the body was squeezed out instead");

        let tall = layout(240.0, 300.0);
        assert!(!tall.footer.is_empty(), "a tall note lost its footer");
    }

    #[test]
    fn a_file_name_survives_any_path() {
        assert_eq!(file_name(".velm/notes/plan.md"), "plan.md");
        assert_eq!(file_name("plan.md"), "plan.md");
        assert_eq!(file_name(""), "");
        // Non-ASCII, and a trailing separator, neither of which may panic.
        assert_eq!(file_name("notes/計画.md"), "計画.md");
        assert_eq!(file_name("notes/"), "");
    }

    #[test]
    fn the_scope_chip_says_which_scope_it_is() {
        assert_eq!(scope_label(&NoteScope::Shared), "Shared");
        assert_eq!(scope_label(&NoteScope::Private { agent: "1@2".into() }), "Private");
    }
}
