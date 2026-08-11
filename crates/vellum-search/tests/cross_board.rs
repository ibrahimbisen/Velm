//! The crate from outside: only the public API, only a foreign document type.
//!
//! The unit tests reach into `Index`'s internals and construct items with the
//! crate's own [`Item`]. This file does neither. It implements [`SearchItem`] on a
//! type this crate has never seen — which is what `vellum-doc` and `vellum-import`
//! will do — and drives the whole lifecycle through the public surface: build,
//! query, persist, reload, edit, re-import. If a re-export is missing or a method
//! is accidentally `pub(crate)`, this file stops compiling.

use vellum_search::{Colour, Index, MatchTier, Matching, Query, SearchItem, Update};

/// A stand-in for a board item, shaped the way `vellum_doc` holds one.
///
/// Text lives in styled spans, and kind is an enum — the two places where a real
/// document differs most from this crate's `Item` — so the trait implementation
/// below has to do the same flattening the real one will.
struct Widget {
    board: &'static str,
    id: String,
    kind: Kind,
    /// The styled spans, flattened once. A real document would flatten on demand;
    /// what matters here is that the trait hands back one `&str` and the span
    /// boundaries are gone by the time the indexer sees it.
    flattened: String,
    background: Option<Colour>,
    labels: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sticky,
    Frame,
    Ink,
    Image,
}

impl Kind {
    /// The tags `vellum_doc::ItemKind::tag` produces.
    fn tag(self) -> &'static str {
        match self {
            Self::Sticky => "sticky",
            Self::Frame => "frame",
            Self::Ink => "ink",
            Self::Image => "image",
        }
    }
}

impl Widget {
    fn new(board: &'static str, id: &str, kind: Kind, spans: &[(&'static str, bool)]) -> Self {
        let flattened = spans.iter().map(|(text, _)| *text).collect();
        Self {
            board,
            // `vellum_doc::ItemId` displays as `counter@peer`.
            id: format!("{id}@7f3a"),
            kind,
            flattened,
            background: None,
            labels: Vec::new(),
        }
    }

    fn coloured(mut self, hex: &str) -> Self {
        self.background = Colour::parse(hex);
        self
    }

    fn labelled(mut self, label: &str) -> Self {
        self.labels.push(label.to_owned());
        self
    }
}

impl SearchItem for Widget {
    fn board(&self) -> &str {
        self.board
    }

    fn item(&self) -> &str {
        &self.id
    }

    fn text(&self) -> &str {
        &self.flattened
    }

    fn kind(&self) -> &str {
        self.kind.tag()
    }

    fn colour(&self) -> Option<Colour> {
        self.background
    }

    fn tags(&self) -> &[String] {
        &self.labels
    }
}

const YELLOW: &str = "#fff79e";
const SALMON: &str = "#ff9e9e";

fn corpus() -> Vec<Widget> {
    vec![
        // A span boundary inside a word: "Cooling" is bold, "fan relay" is not, and
        // the whole thing must still index as three ordinary terms.
        Widget::new(
            "site-plan",
            "1",
            Kind::Sticky,
            &[("Cool", true), ("ing fan relay", false)],
        )
        .coloured(YELLOW)
        .labelled("review"),
        Widget::new("site-plan", "2", Kind::Sticky, &[("Water pump — replace at 120k", false)])
            .coloured(YELLOW),
        Widget::new("site-plan", "3", Kind::Sticky, &[("Coolant expansion tank", false)])
            .coloured(SALMON),
        Widget::new("site-plan", "4", Kind::Frame, &[("Engine bay", false)]),
        Widget::new("site-plan", "5", Kind::Ink, &[]),
        Widget::new("site-plan", "6", Kind::Image, &[]),
        Widget::new("keyboards", "1", Kind::Sticky, &[("Switch lubrication", false)])
            .coloured(YELLOW),
        Widget::new("keyboards", "2", Kind::Frame, &[("Build log", false)]),
    ]
}

fn ids(hits: &[vellum_search::Hit]) -> Vec<String> {
    hits.iter().map(|hit| format!("{}/{}", hit.board, hit.item)).collect()
}

fn temporary(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "vellum-search-it-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).expect("a temporary directory");
    directory.join("index.vsx")
}

#[test]
fn a_foreign_document_type_indexes_and_answers_the_queries_the_brief_names() {
    let index = Index::build(corpus());
    assert_eq!(index.len(), 8);
    assert_eq!(index.boards().collect::<Vec<_>>(), ["keyboards", "site-plan"]);
    assert_eq!(index.kinds().collect::<Vec<_>>(), ["frame", "image", "ink", "sticky"]);
    assert_eq!(index.tags().collect::<Vec<_>>(), ["review"]);

    // A word split across a styled-span boundary is one term.
    let hits = index.search(&Query::parse("cooling"));
    assert_eq!(ids(&hits), ["site-plan/1@7f3a"]);
    assert_eq!(hits[0].snippet.render("[", "]"), "[Cooling] fan relay");

    // Prefix, as the user types.
    assert_eq!(index.search(&Query::parse("cool")).len(), 2, "cooling and coolant");

    // The two queries the brief calls out by name.
    assert_eq!(index.search(&Query::parse("all yellow stickies")).len(), 3);
    assert_eq!(index.search(&Query::parse("every frame")).len(), 2);

    // And the items with no text at all are reachable by kind.
    assert_eq!(ids(&index.search(&Query::parse("kind:ink"))), ["site-plan/5@7f3a"]);
}

#[test]
fn every_filter_axis_works_from_outside_the_crate() {
    let index = Index::build(corpus());
    assert_eq!(index.search(&Query::parse("board:keyboards")).len(), 2);
    assert_eq!(index.search(&Query::parse("kind:sticky")).len(), 4);
    assert_eq!(index.search(&Query::parse("colour:red")).len(), 1, "the one salmon sticky");
    assert_eq!(index.search(&Query::parse("colour:pink")).len(), 1, "which is also pink");
    assert_eq!(index.search(&Query::parse(&format!("color:{YELLOW}"))).len(), 3);
    assert_eq!(index.search(&Query::parse("tag:review")).len(), 1);

    // OR within an axis, AND across axes.
    assert_eq!(index.search(&Query::parse("kind:frame kind:ink")).len(), 3);
    assert_eq!(index.search(&Query::parse("kind:sticky board:keyboards")).len(), 1);
    assert!(index.search(&Query::parse("kind:frame colour:red")).is_empty());
}

#[test]
fn a_hit_carries_everything_needed_to_open_the_board_and_select_the_item() {
    let index = Index::build(corpus());
    let hits = index.search(&Query::parse("expansion"));
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];

    assert_eq!(hit.board, "site-plan");
    assert_eq!(hit.item, "3@7f3a");
    assert_eq!(hit.kind, "sticky");
    assert_eq!(hit.colour, Colour::parse(SALMON));
    assert_eq!(hit.score.tier, MatchTier::Exact);
    assert_eq!(hit.snippet.render("<mark>", "</mark>"), "Coolant <mark>expansion</mark> tank");
    assert_eq!(&hit.snippet.text[hit.snippet.spans[0].range()], "expansion");
}

#[test]
fn matching_modes_are_selectable_and_do_what_they_say() {
    let index = Index::build(corpus());

    let exact = Query::parse("cool").with_matching(Matching::EXACT);
    assert!(index.search(&exact).is_empty(), "no item's text is the word 'cool'");

    assert_eq!(index.search(&Query::parse("cool")).len(), 2, "prefix is the default");

    // `coolent` is one edit from `coolant` and two from `cooling`, and both come
    // back — nearest guess first.
    let typo = Query::parse("coolent").with_matching(Matching::forgiving());
    let hits = index.search(&typo);
    assert_eq!(ids(&hits), ["site-plan/3@7f3a", "site-plan/1@7f3a"]);
    assert_eq!(hits[0].score.tier, MatchTier::Fuzzy { edits: 1 });
    assert!(hits[1].score.tier.is_fuzzy());

    assert_eq!(index.search(&Query::parse("\"water pump\"")).len(), 1);
    assert!(index.search(&Query::parse("\"pump water\"")).is_empty(), "order matters");
}

#[test]
fn the_full_lifecycle_survives_a_save_and_reload() {
    let path = temporary("lifecycle");
    let mut index = Index::build(corpus());
    let before = index.search(&Query::parse("cool"));

    index.save(&path).expect("saving");
    assert!(!index.is_dirty());

    let mut reloaded = Index::load(&path).expect("loading");
    assert_eq!(reloaded.stats(), index.stats());
    assert_eq!(reloaded.search(&Query::parse("cool")), before);
    assert!(!reloaded.is_dirty());

    // Editing one item after a reload behaves exactly as it did before one.
    let edited = Widget::new(
        "site-plan",
        "2",
        Kind::Sticky,
        &[("Thermostat housing", false)],
    )
    .coloured(YELLOW);
    assert_eq!(reloaded.upsert(&edited), Update::Replaced);
    assert!(reloaded.is_dirty());
    assert!(reloaded.search(&Query::parse("water")).is_empty());
    assert_eq!(reloaded.search(&Query::parse("thermostat")).len(), 1);

    // A whole board can be re-imported without touching the other.
    let revision: Vec<Widget> = corpus().into_iter().filter(|w| w.board == "keyboards").collect();
    reloaded.replace_board("keyboards", revision);
    assert_eq!(reloaded.len(), 8);
    assert_eq!(reloaded.search(&Query::parse("thermostat")).len(), 1, "the first board is untouched");

    // And a deleted board takes its terms with it.
    assert_eq!(reloaded.remove_board("keyboards"), 2);
    assert!(reloaded.search(&Query::parse("lubrication")).is_empty());
    assert_eq!(reloaded.boards().collect::<Vec<_>>(), ["site-plan"]);

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn identical_queries_return_identical_sequences_across_a_reload() {
    let path = temporary("determinism");
    let mut index = Index::build(corpus());
    index.save(&path).expect("saving");
    let reloaded = Index::load(&path).expect("loading");

    for text in ["cool", "kind:sticky", "all yellow stickies", "every frame", "\"water pump\""] {
        let query = Query::parse(text).with_limit(usize::MAX);
        let expected = index.search(&query);
        assert!(!expected.is_empty(), "query {text:?} matched nothing");
        assert_eq!(reloaded.search(&query), expected, "query {text:?}");
    }

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn a_corrupt_index_file_is_reported_rather_than_loaded() {
    let path = temporary("corrupt");
    let mut index = Index::build(corpus());
    index.save(&path).expect("saving");

    let mut bytes = std::fs::read(&path).expect("reading back");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("writing the damaged file");

    let error = Index::load(&path).expect_err("a damaged index must not load");
    assert!(
        matches!(error, vellum_search::SearchError::ChecksumMismatch { .. }),
        "{error}",
    );

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}
