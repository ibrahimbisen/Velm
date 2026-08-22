//! Every item's words, including the four whose text is inside an opaque token.
//!
//! [`vellum_doc::ItemKind::text`] answers `Some` for a sticky, a text item, a shape's label and
//! a frame's title, and `None` for a table, a chart, a mind map and a kanban board. That `None`
//! is honest — those four store their model as a **JSON token** the document layer deliberately
//! cannot parse, because `vellum-doc` depends on `loro` and `thiserror` and nothing else — but
//! it had a consequence nobody chose: **a search for a word in a table cell found nothing**,
//! while the same word on a sticky was found.
//!
//! # Why this lives here rather than in `vellum-app`
//!
//! It was in `vellum-app`, reaching six of that crate's own modules for the decoders. The
//! browser now draws all four widgets, so a word on a kanban card is on screen in a tab — and a
//! search that could not find it would be the two applications disagreeing about what a board
//! contains. `look`, `runs`, `frame` and `card` all made this move for the same reason: a rule
//! both front ends must answer identically belongs in the crate both of them read.
//!
//! # ⚠ An unreadable token yields **no words**, not a default board's
//!
//! `vellum-app`'s decoders answer with a fabricated default — a three-column *Sprint* board, a
//! chart of invented numbers — because the painter has to draw *something* and an empty box
//! reads as a bug. That is right for drawing and wrong here: indexing the default kanban's
//! *"To do / Doing / Done"* would let a search match words that are on nobody's board, and a
//! result you cannot find when you get there is worse than no result. So every decode is
//! `ok()`, and a token this build cannot read is simply an item with nothing to say.
//!
//! # The `MindMapModel` shape is defined once, here
//!
//! It was hand-copied in two places — `vellum-app` and the browser's widget painter — and an
//! audit named the copies as a drift risk with teeth: the field names and the two
//! `#[serde(default)]` attributes are load-bearing, and a rename on either side silently
//! degrades every mind map on every board to the default layout, with nothing failing to say
//! so. One definition, re-exported, cannot drift.

use serde::{Deserialize, Serialize};
use vellum_chart::ChartSpec;
use vellum_doc::ItemKind;
use vellum_flow::Kanban;
use vellum_mindmap::{ConnectorShape, LayoutKind, MindMap};
use vellum_table::Table;

/// A mind map plus the two choices that are about how it is *drawn* rather than what it says.
///
/// ⚠ Both `#[serde(default)]` attributes are load-bearing: a map written before either field
/// existed still opens, which is the rule every token type in this application follows. A board
/// that stops parsing is a board that is gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MindMapModel {
    pub map: MindMap,
    /// Tree, balanced or radial.
    #[serde(default)]
    pub kind: LayoutKind,
    /// Straight, elbow or curve.
    #[serde(default)]
    pub connectors: ConnectorShape,
}

/// Every word an item carries, in the order a reader would meet them.
///
/// ⚠ **Labels stay separate rather than joined into one string.** A query cannot then match
/// across the seam between two of them — a search for *"torque nm"* should not be satisfied by
/// a cell reading *"torque"* beside one reading *"Nm"*, because that match names a phrase
/// nobody wrote. The desktop's index makes the same choice.
///
/// Empty strings are dropped: a table of mostly blank cells would otherwise index dozens of
/// nothings, and an item with no words at all should not appear to have some.
pub fn of(kind: &ItemKind) -> Vec<String> {
    let mut out = match kind {
        // The kinds the document can already answer for. `text()` covers a sticky, a text item,
        // a shape's label, a frame's title, an agent's role and a note's title.
        _ if kind.text().is_some() => {
            kind.text().map(|text| vec![text.to_plain()]).unwrap_or_default()
        }
        ItemKind::Table { model } => table_words(model),
        ItemKind::Chart { spec } => chart_words(spec),
        ItemKind::MindMap { model } => mindmap_words(model),
        ItemKind::Kanban { board } => kanban_words(board),
        // ⚠ A link card's scraped title is deliberately absent, and this is the same rule that
        // keeps an agent's transcript out of a search: what is indexed is what the *user*
        // wrote. A board of 91 cards would otherwise return a page's marketing copy for half
        // the queries somebody types.
        _ => Vec::new(),
    };
    out.retain(|word| !word.trim().is_empty());
    out
}

/// A table's cells, in reading order.
///
/// Through `anchors`, which is row-major and skips covered positions — so a merged cell answers
/// **once**, through its top-left, rather than once per coordinate it spans. That is also
/// exactly the set the painter lays out, which keeps "what is indexed" and "what is drawn" one
/// list rather than two that can disagree.
fn table_words(token: &str) -> Vec<String> {
    let Ok(table) = serde_json::from_str::<Table>(token) else { return Vec::new() };
    table.grid().anchors().map(|(_, cell)| cell.content().to_plain()).collect()
}

/// A chart's categories and series names.
///
/// Not its numbers: a search for `42` matching every chart with a bar that height is noise, and
/// a number is not a word somebody remembers writing.
fn chart_words(token: &str) -> Vec<String> {
    let Ok(spec) = serde_json::from_str::<ChartSpec>(token) else { return Vec::new() };
    use vellum_chart::ChartData;
    let mut out = Vec::new();
    match &spec.data {
        ChartData::Categorical(data) => {
            out.extend(data.categories().iter().cloned());
            out.extend(data.series().iter().map(|series| series.name.clone()));
        }
        // A scatter's points have no categories; only the series are named.
        ChartData::Points(points) => {
            out.extend(points.series().iter().map(|series| series.name.clone()));
        }
    }
    out
}

/// Every node's label.
///
/// ⚠ The walk carries a visited set for the reason `MindMap::visible_nodes` now does: `children`
/// round-trips through serde, so a token whose root lists itself as a child parses cleanly, and
/// a walk without one never returns. Here that would hang the *search*, on a board somebody was
/// only trying to look through.
fn mindmap_words(token: &str) -> Vec<String> {
    let Ok(model) = serde_json::from_str::<MindMapModel>(token) else { return Vec::new() };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![model.map.root()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(node) = model.map.get(id) {
            out.push(node.text.clone());
            stack.extend(node.children().iter().copied());
        }
    }
    out
}

/// The board's title, then each column's title and each card's label.
fn kanban_words(token: &str) -> Vec<String> {
    let Ok(board) = serde_json::from_str::<Kanban>(token) else { return Vec::new() };
    let mut out = vec![board.title().to_owned()];
    for column in board.columns() {
        out.push(column.title().to_owned());
        out.extend(column.cards().iter().map(|card| card.label().to_owned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⚠ **The whole reason this module exists**: a word in a table cell is findable, and a
    /// sticky's word was always findable, so the two agree.
    #[test]
    fn a_table_cell_carries_its_words() {
        let mut table = Table::new(2, 2);
        for (row, col, word) in [(0, 0, "Torque"), (0, 1, "Nm")] {
            table
                .set_content(
                    vellum_table::CellRef::new(row, col),
                    vellum_table::StyledText::plain(word),
                )
                .expect("a 2x2 table has that cell");
        }
        let kind = ItemKind::Table { model: serde_json::to_string(&table).unwrap() };
        let words = of(&kind);
        assert!(words.contains(&"Torque".to_owned()), "got {words:?}");
        assert!(words.contains(&"Nm".to_owned()), "got {words:?}");
        // ⚠ Separate entries, never joined — or a query for "torque nm" matches a phrase
        // nobody wrote.
        assert!(!words.iter().any(|w| w.contains("Torque Nm")), "labels were joined");
    }

    /// ⚠ An unreadable token indexes **nothing**, where the *painter* would draw a default.
    ///
    /// Indexing the default kanban's "To do / Doing / Done" would let a search match words that
    /// are on nobody's board — and a result you cannot find when you arrive is worse than no
    /// result at all.
    #[test]
    fn a_token_this_build_cannot_read_has_no_words_rather_than_invented_ones() {
        for kind in [
            ItemKind::Table { model: "{not json".to_owned() },
            ItemKind::Chart { spec: String::new() },
            ItemKind::MindMap { model: "[]".to_owned() },
            ItemKind::Kanban { board: "null".to_owned() },
        ] {
            assert!(of(&kind).is_empty(), "an unreadable token invented words: {kind:?}");
        }
    }

    /// A self-referential mind map terminates rather than hanging the search.
    #[test]
    fn a_mind_map_cycle_does_not_hang_the_index() {
        let map = MindMap::new("root");
        let mut json: serde_json::Value =
            serde_json::to_value(MindMapModel {
                map,
                kind: LayoutKind::default(),
                connectors: ConnectorShape::default(),
            })
            .unwrap();
        let root = json["map"]["root"].clone();
        json["map"]["slots"][0]["node"]["children"] = serde_json::Value::Array(vec![root]);
        let kind = ItemKind::MindMap { model: serde_json::to_string(&json).unwrap() };
        assert_eq!(of(&kind), vec!["root".to_owned()]);
    }

    /// A link card's scraped title is not the user's writing, so it is not indexed.
    #[test]
    fn a_scraped_card_title_is_not_searchable() {
        let kind = ItemKind::LinkPreview {
            title: Some("Superbat 3G/6G/12G SDI Cable".to_owned()),
            url: Some("https://example.com".to_owned()),
            description: None,
            thumbnail: None,
            provider: None,
            favicon: None,
            mode: vellum_doc::CardMode::Card,
        };
        assert!(of(&kind).is_empty());
    }
}
