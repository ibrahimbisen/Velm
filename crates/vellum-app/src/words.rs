//! Every item's words, including the four whose text is inside an opaque token.
//!
//! `vellum_doc::ItemKind::text` answers `Some` for a sticky, a text item, a shape's label
//! and a frame's title, and `None` for a table, a chart, a mind map and a kanban board.
//! That `None` is honest — those four store their model as a **JSON token** the document
//! layer deliberately cannot parse, because `vellum-doc` depends on `loro` and `thiserror`
//! and nothing else — but it had a consequence nobody chose: **a search for a word in a
//! table cell found nothing**, while the same word on a sticky was found.
//!
//! `crate::actions`'s find bar is the caller today. The board library's card previews and
//! the CSV export still read `ItemKind::text` directly and so still skip these four: both
//! are one call from being fixed and neither is done here, because a vector export wants
//! the words *drawn where they belong* rather than dumped as one block over the table, and
//! that is the font-stack work `CLAUDE.md` records against those two paths.
//!
//! This module is the missing decoder, and it lives here because here is the only place
//! that can have one: `vellum-app` owns the token format for all four widgets, so
//! `crate::table`, `crate::chart`, `crate::mindmap` and `crate::kanban` each answer for
//! their own model and this dispatches between them.
//!
//! # Why a `Vec<String>` and not one string
//!
//! Each element is one *label* — a cell, a card, a node, a category. A caller that wants a
//! blob joins them, and one that wants to match whole labels does not have to guess where
//! one ended. Joining first would also invent adjacencies: a search for "todo measure"
//! would match a column called "To do" followed by a card called "Measure the atlas",
//! which is a phrase that appears nowhere on the board.
//!
//! Decoding on demand rather than caching: the token is parsed by `serde_json` and a
//! search over the reference board's 596 items runs once per keystroke in the find bar, so
//! this is measured against a substring scan that was already walking every item.

use vellum_doc::ItemKind;

/// Every label the item carries, in reading order. Empty for an item with no words.
///
/// The plain kinds come through `ItemKind::text` exactly as they always did, so this is a
/// superset of the old behaviour rather than a second answer to the same question.
pub fn of(kind: &ItemKind) -> Vec<String> {
    if let Some(text) = kind.text() {
        let plain = text.to_plain();
        return if plain.trim().is_empty() { Vec::new() } else { vec![plain] };
    }
    match kind {
        ItemKind::Table { model } => crate::table::words(&crate::table::decode(model)),
        ItemKind::Chart { spec } => crate::chart::words(&crate::chart::decode(spec)),
        ItemKind::MindMap { model } => crate::mindmap::words(&crate::mindmap::decode(model)),
        ItemKind::Kanban { board } => crate::kanban::words(&crate::kanban::decode(board)),
        // ⚠ `ItemKind::{Agent, AgentNote, FileTree, Browser}` land here and index nothing,
        // and that is deliberate rather than an oversight.
        //
        // **The two that carry the user's own words are already handled above.** An agent's
        // role and a note's title are ordinary `StyledText` on the item — they live *beside*
        // the opaque token, the way a shape's label lives beside its form — so `ItemKind::text`
        // answers `Some` for both and they returned through the plain path at the top of this
        // function. A board that holds them is still searchable by them, which is the point:
        // this build no longer offers a way to *make* one of these items, and a board that
        // already has one must not quietly stop answering for words the user typed into it.
        //
        // The other two keep their text *inside* the token — a browser node's address, a file
        // tree's root directory — and nothing in this crate parses those tokens any more, so
        // there is no honest way to report their contents. Answering with nothing is the right
        // failure: a search that silently missed them would be indistinguishable from a search
        // that found them empty, and inventing a decoder here to read a format nothing else in
        // the application understands would be a second source of truth for it.
        _ => Vec::new(),
    }
}

/// The item's words as one lowercase haystack, for a substring search.
///
/// Labels are joined with `\n` rather than a space so a query cannot match across the seam
/// between two of them — see the module note.
pub fn haystack(kind: &ItemKind) -> String {
    of(kind).join("\n").to_lowercase()
}

/// Whether any of the item's labels contains `needle`, which must already be lowercase.
pub fn contains(kind: &ItemKind, needle: &str) -> bool {
    of(kind).iter().any(|label| label.to_lowercase().contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::StyledText;

    #[test]
    fn a_plain_kind_answers_with_its_own_words() {
        let sticky = ItemKind::Sticky { text: StyledText::plain("Cooling loop"), background: None };
        assert_eq!(of(&sticky), vec!["Cooling loop".to_owned()]);
        assert!(contains(&sticky, "cooling"));
        assert!(!contains(&sticky, "heating"));
    }

    /// An empty sticky must not index as one empty label: `contains` with an empty needle
    /// would then match it, and the find bar's readout would count blank items as hits.
    #[test]
    fn an_item_with_no_words_indexes_nothing() {
        let blank = ItemKind::Sticky { text: StyledText::plain("   "), background: None };
        assert!(of(&blank).is_empty());
        assert!(of(&ItemKind::Ink { points: vec![], color: None, thickness: 1.0 }).is_empty());
        assert!(of(&ItemKind::Group).is_empty());
    }

    /// The gap this module exists to close: the four structured widgets were invisible to
    /// search because their words are inside a token `vellum-doc` cannot read.
    #[test]
    fn the_structured_widgets_are_searchable() {
        let table = ItemKind::Table { model: crate::table::encode(&{
            let mut table = crate::table::default_table();
            table
                .cell_mut(vellum_table::CellRef::new(0, 0))
                .unwrap()
                .set_content(vellum_table::StyledText::plain("Torque"));
            table
        }) };
        assert!(contains(&table, "torque"), "{:?}", of(&table));

        let kanban = ItemKind::Kanban { board: crate::kanban::encode(&crate::kanban::default_kanban()) };
        let labels = of(&kanban);
        assert!(labels.contains(&"Sprint".to_owned()), "the board's own title: {labels:?}");
        assert!(labels.contains(&"To do".to_owned()), "a column header: {labels:?}");
        assert!(contains(&kanban, "measure the atlas"), "a card's label: {labels:?}");

        let mindmap =
            ItemKind::MindMap { model: crate::mindmap::encode(&crate::mindmap::default_mindmap()) };
        assert!(contains(&mindmap, "central idea"));
        assert!(contains(&mindmap, "branch three"), "a leaf deep in the tree");

        let chart = ItemKind::Chart { spec: crate::chart::encode(&crate::chart::default_chart()) };
        assert!(contains(&chart, "q3"), "a category");
        assert!(contains(&chart, "target"), "a series name");
        assert!(!contains(&chart, "19"), "a value is not a word anybody searches by");
    }

    /// A board that already holds one of the four token-backed kinds is still searchable
    /// by the words the *user* typed into it.
    ///
    /// RULE ZERO's line, at this layer: the document still understands these kinds, so a
    /// board saved when they could be created must keep behaving like a board. The role and
    /// the title survive because they were never inside the token — they are ordinary
    /// `StyledText` beside it, so they come back through `ItemKind::text` and this module
    /// never has to parse anything. A build that answered `Vec::new()` for all four would
    /// make a find on such a board silently miss items that are plainly on screen.
    #[test]
    fn a_board_that_still_holds_a_token_backed_kind_keeps_its_own_words() {
        let agent = ItemKind::Agent {
            model: "{}".to_owned(),
            label: StyledText::plain("Cooling Reviewer"),
        };
        assert_eq!(of(&agent), vec!["Cooling Reviewer".to_owned()]);
        assert!(contains(&agent, "reviewer"));

        let note =
            ItemKind::AgentNote { model: "{}".to_owned(), title: StyledText::plain("Plan") };
        assert!(contains(&note, "plan"), "{:?}", of(&note));

        // These two keep their text inside the token and nothing here decodes one, so they
        // index nothing. Asserted rather than left implicit: it is a real gap on a board
        // that has them, and a silent `_ => Vec::new()` is exactly the kind of arm somebody
        // later mistakes for an oversight and "fixes" with a second token parser.
        assert!(of(&ItemKind::Browser { model: "{}".to_owned() }).is_empty());
        assert!(of(&ItemKind::FileTree { model: "{}".to_owned() }).is_empty());
    }

    /// A folded branch is hidden, not deleted. Answering "not on this board" for a word
    /// that is merely folded away is the wrong answer.
    #[test]
    fn a_collapsed_mind_map_branch_is_still_searchable() {
        let mut model = crate::mindmap::default_mindmap();
        let root = model.map.root();
        let branch = model.map.children(root)[0];
        model.map.get_mut(branch).unwrap().collapsed = true;

        let kind = ItemKind::MindMap { model: crate::mindmap::encode(&model) };
        assert!(contains(&kind, "detail"), "a leaf under a folded branch: {:?}", of(&kind));
    }

    /// Labels are separate so a query cannot match across the seam between two of them.
    #[test]
    fn a_query_cannot_match_across_two_labels() {
        let kanban =
            ItemKind::Kanban { board: crate::kanban::encode(&crate::kanban::default_kanban()) };
        assert!(contains(&kanban, "to do"));
        assert!(
            !contains(&kanban, "to do measure"),
            "that phrase appears nowhere on the board; joining the labels would invent it"
        );
        assert!(haystack(&kanban).contains('\n'), "the seam has to be un-matchable");
    }
}
