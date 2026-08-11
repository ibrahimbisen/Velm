//! Mind maps on the board: the document token, node measurement, and the laid-out map.
//!
//! `vellum-mindmap` owns the tree and the arithmetic that arranges it — Buchheim,
//! Jünger and Leipert's linear-time tidy layout in three forms, collapse, reparent,
//! stabilisation across an edit, and parent → child connector paths. Like
//! `vellum-table` and `vellum-chart` it has been complete and tested since before the
//! document had anywhere to put one.
//!
//! Three things live here.
//!
//! # The token
//!
//! An opaque string, as [`crate::shapes`], [`crate::table`] and [`crate::chart`] use,
//! for the layering reason those record: `vellum-doc` depends on `loro` and `thiserror`
//! and nothing else.
//!
//! What is stored is a [`MindMapModel`] rather than a bare `MindMap`, because the tree
//! alone does not say what shape the map is. Which of the three layout forms it uses
//! and which connector form it draws are the user's choices about *this* map, not a
//! view setting, so they belong on the item.
//!
//! # The measurement
//!
//! `vellum-mindmap` never measures text — its own docs say so, and that is what lets
//! every non-overlap claim in it be tested on a machine with no font stack.
//! [`Node::size`](vellum_mindmap::Node::size) is written by the caller. [`measured`] is
//! that caller: it shapes each visible label through the same `vellum-text` engine the
//! canvas draws with and sizes the node's box around it.
//!
//! # The layout
//!
//! [`layout`] runs the tidy pass and then slides the result so its bounding box starts
//! at the origin, which makes a node rectangle an offset from the item's top-left — the
//! same convention `vellum-table`'s cell rectangles already use, so the drawing path
//! reads the same way for both.
//!
//! # Honest limits
//!
//! - **A mind map cannot be edited on the board.** There is no way to add a node, type
//!   into one, fold a branch or drag a subtree to a new parent; every one of those verbs
//!   exists and is tested in `vellum-mindmap`, and none of them has an input path. What
//!   is placeable is the default map.
//! - **Resizing the item scales the map**; it does not re-flow it. Node boxes come from
//!   shaped text, so the map has one natural size, and the item's box is fitted to it.
//!   Re-flowing to a dragged width is not a thing a tidy tree can do — its extent is
//!   determined by the tree, not chosen.

use serde::{Deserialize, Serialize};
use vellum_mindmap::{
    Color, ConnectorOptions, ConnectorPath, ConnectorShape, Layout, LayoutKind, LayoutOptions,
    MindMap, Node, NodeStyle, Size, Vec2,
};
use vellum_text::{LayoutParams, SpanStyle, StyledText, TextEngine, TextSpan};

/// A mind map as the document stores it: the tree, plus the two choices that decide
/// what shape it is drawn in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MindMapModel {
    pub map: MindMap,
    /// Tree, balanced or radial. `#[serde(default)]` so a map written before a field
    /// existed still opens — the same rule the rest of the token types follow.
    #[serde(default)]
    pub kind: LayoutKind,
    /// Straight, elbow or curve.
    #[serde(default)]
    pub connectors: ConnectorShape,
}

/// The token stored in the document for a mind map.
pub fn encode(model: &MindMapModel) -> String {
    serde_json::to_string(model).unwrap_or_else(|error| {
        log::warn!("a mind map would not encode ({error}); storing nothing");
        String::new()
    })
}

/// The mind map a token names, or the default one when it cannot be read.
///
/// An unreadable token becomes a map rather than an error and rather than a missing
/// item: a board written by a later build still opens, and the item keeps its size and
/// its place.
pub fn decode(token: &str) -> MindMapModel {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable mind map ({error}); drawing the default one");
        }
        default_mindmap()
    })
}

/// How much a map of this natural extent has to shrink to sit inside `size`.
///
/// Uniform, and the smaller of the two ratios: a map stretched to a dragged box would put
/// its text at one aspect and its branches at another. Resizing a mind map therefore scales
/// it, which is this module's honest-limits note — a tidy tree's extent is determined by the
/// tree, not chosen.
///
/// A free function rather than a method on the painter's cache because the **press** path
/// needs the same number: a click has to be converted into map space to find the node under
/// it, and a second copy of this arithmetic would put the caret on the wrong node at every
/// zoom but 1:1.
pub fn fit_scale(natural: (f64, f64), size: (f64, f64)) -> f64 {
    let fit = (size.0 / natural.0).min(size.1 / natural.1);
    if fit.is_finite() && fit > 0.0 { fit } else { 1.0 }
}

/// Every node's text, root first, for search and export.
///
/// **Collapsed branches are included.** A folded node is hidden, not deleted — searching
/// for a word and being told it is not on the board, when it is merely folded away, is
/// the wrong answer. Compare `visible_children`, which is what the *painter* walks.
pub fn words(model: &MindMapModel) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![model.map.root()];
    while let Some(id) = stack.pop() {
        if let Some(node) = model.map.get(id) {
            if !node.text.trim().is_empty() {
                out.push(node.text.clone());
            }
            stack.extend(model.map.children(id).iter().rev().copied());
        }
    }
    out
}

/// Horizontal padding inside a node box, either side of the label.
pub const NODE_PADDING_X: f64 = 14.0;
/// Vertical padding inside a node box, above and below the label.
pub const NODE_PADDING_Y: f64 = 8.0;
/// The narrowest a node box gets, so an empty node is still a target.
pub const MIN_NODE_WIDTH: f64 = 56.0;

/// The item's box when the mind-map tool places one.
///
/// Close to the default map's natural size so the first frame is drawn at about 1:1.
/// It does not have to be exact — the drawing path fits the map to whatever box the
/// item has — but a wildly wrong one would place a legible map at a silly scale.
pub const DEFAULT_SIZE: (f64, f64) = (520.0, 260.0);

// The design language's own values, spelled here because `vellum-mindmap` has no theme
// and must not grow one: it is a layout crate, and `docs/05-design-language.md` §1 is
// the source these come from.
//
// These end up in the *document*, so a map already on disk keeps whatever it was
// placed with — this only changes what a fresh one looks like.
const WHITE: Color = Color::rgb(0xFF, 0xFF, 0xFF);
const FROST: Color = Color::rgb(0xE5, 0xEA, 0xED);
/// The primary accent, which is `signal-teal` now rather than `xr-red`; the root node
/// wears it, and a map placed today should not be the one thing on the board still
/// ringed in coral.
const SIGNAL_TEAL: Color = Color::rgb(0x00, 0xA3, 0x8C);
const INK_MUTED: Color = Color::rgb(0x5C, 0x65, 0x6B);

/// What the mind-map tool places: a root and two levels under it.
///
/// Not an empty map with one node. A lone box is indistinguishable from a sticky that
/// failed to draw, and there is no way to add a second node yet — so what is placed has
/// to already look like a mind map for the feature to be worth anything.
pub fn default_mindmap() -> MindMapModel {
    let branch = NodeStyle::default()
        .with_fill(WHITE)
        .with_border(FROST, 1.0)
        .with_connector(INK_MUTED, 2.0);
    let root = branch.with_border(SIGNAL_TEAL, 2.0).with_font_size(18.0).bold(true);

    let mut map = MindMap::with_root(Node::new("Central idea").with_style(root));
    let centre = map.root();
    for (title, leaves) in [
        ("Branch one", ["Detail", "Another detail"].as_slice()),
        ("Branch two", ["Detail"].as_slice()),
        ("Branch three", ["Detail", "Another detail"].as_slice()),
    ] {
        let Ok(parent) = map.add_child(centre, Node::new(title).with_style(branch)) else {
            continue;
        };
        for leaf in leaves {
            let _ = map.add_child(parent, Node::new(*leaf).with_style(branch));
        }
    }
    MindMapModel { map, kind: LayoutKind::default(), connectors: ConnectorShape::default() }
}

/// The map with every visible node's box sized around its shaped label.
///
/// Returns a copy rather than mutating in place: the stored model is the user's, and a
/// measurement is a property of the font stack this process happens to have, not
/// something to write back to disk. Sizing an unreadable node is skipped rather than
/// defaulted — [`Node::size`](vellum_mindmap::Node::size) already carries a sensible
/// 160 × 44, and a node whose id vanished between the walk and the write is not a
/// situation to invent a box for.
pub fn measured(model: &MindMapModel, engine: &mut TextEngine) -> MindMap {
    let mut map = model.map.clone();
    for id in map.visible_nodes() {
        let Some(node) = map.get(id) else { continue };
        let style = node.style;
        let text = label(&node.text, &style);
        #[expect(clippy::cast_possible_truncation, reason = "a label's size is screen-scale")]
        let params = LayoutParams {
            font_size: style.font_size as f32,
            max_width: None,
            ..LayoutParams::default()
        };
        let extent = engine.measure(&text, &params);
        // At least one line tall, whatever the label. An empty node collapsing to its
        // padding would be a 28px sliver that cannot be read as a node.
        let line = style.font_size * f64::from(LayoutParams::default().line_height);
        let size = Size::new(
            (f64::from(extent.width) + NODE_PADDING_X * 2.0).max(MIN_NODE_WIDTH),
            f64::from(extent.height).max(line) + NODE_PADDING_Y * 2.0,
        );
        if let Some(node) = map.get_mut(id) {
            node.size = size;
        }
    }
    map
}

/// A node's label as the text engine's spans.
///
/// Bold rides on the node's style in one model and on the span in the other, exactly as
/// it does for a table cell — so the flag is folded in here rather than at each of the
/// two call sites that would otherwise have to remember.
pub fn label(text: &str, style: &NodeStyle) -> StyledText {
    StyledText::from_spans(vec![TextSpan {
        text: text.to_owned(),
        style: SpanStyle { bold: style.bold, ..SpanStyle::default() },
    }])
}

/// The laid-out map, with its bounding box starting at the origin.
///
/// The translation is what makes a node rectangle an offset from the item's top-left,
/// which is the convention `vellum-table`'s cell rectangles already use. Without it a
/// tree layout starts at the root's centre and half the map has negative coordinates.
pub fn layout(model: &MindMapModel, engine: &mut TextEngine) -> Layout {
    let map = measured(model, engine);
    let options = LayoutOptions { kind: model.kind, ..LayoutOptions::default() };
    let mut out = map.layout(&options);
    if let Some(bounds) = out.bounds() {
        out.translate(Vec2::new(-bounds.min.x, -bounds.min.y));
    }
    out
}

/// The map's natural extent — what the item's box would have to be for it to draw 1:1.
///
/// Never zero in either axis: it is a divisor when the drawing path works out its fit
/// scale, and a map of one empty node is a legitimate thing to have on a board.
pub fn natural_size(layout: &Layout) -> (f64, f64) {
    layout
        .bounds()
        .map_or(DEFAULT_SIZE, |b| (b.width().max(1.0), b.height().max(1.0)))
}

/// The branches, in the model's own connector form.
pub fn connectors(model: &MindMapModel, layout: &Layout) -> Vec<ConnectorPath> {
    layout.connectors(&ConnectorOptions {
        shape: model.connectors,
        ..ConnectorOptions::default()
    })
}

#[cfg(test)]
mod tests {
    /// The two colours this file copies out of `vellum_ui::theme::swatch::light`, joined to
    /// their source.
    ///
    /// They are spelled here because `vellum-mindmap` is a layout crate with no theme and
    /// must not grow one — which is fine, and is exactly the arrangement that let
    /// `inspect.rs`'s `THEME_BORDER` go stale once already. A hand-copied constant in another
    /// crate is a join or it is a time bomb.
    #[test]
    fn the_hand_copied_colours_still_match_the_palette() {
        for (name, mine, theirs) in [
            ("frost", super::FROST, vellum_ui::theme::Palette::LIGHT.border),
            ("accent", super::SIGNAL_TEAL, vellum_ui::Accent::Teal.swatch()),
        ] {
            assert_eq!(
                [mine.r, mine.g, mine.b],
                [theirs.r(), theirs.g(), theirs.b()],
                "`{name}` has drifted from the palette"
            );
        }
    }

    use super::*;

    fn engine() -> TextEngine {
        TextEngine::new().expect("the test machine has fonts")
    }

    /// The round trip the document depends on.
    #[test]
    fn a_mind_map_survives_the_trip_to_the_document_and_back() {
        let model = default_mindmap();
        assert_eq!(decode(&encode(&model)), model);
    }

    /// A board written by a later build still opens; only the contents fall back.
    #[test]
    fn an_unreadable_token_becomes_the_default_map_rather_than_failing() {
        assert_eq!(decode("{ not json"), default_mindmap());
        assert_eq!(decode(""), default_mindmap());
    }

    /// The two view choices are `#[serde(default)]`, so a token that predates them —
    /// or one hand-written with only the tree in it — still opens.
    #[test]
    fn a_token_carrying_only_the_tree_still_opens() {
        let bare = serde_json::to_string(&serde_json::json!({
            "map": serde_json::to_value(&default_mindmap().map).unwrap()
        }))
        .unwrap();
        let back = decode(&bare);
        assert_eq!(back.kind, LayoutKind::default());
        assert_eq!(back.connectors, ConnectorShape::default());
        assert_eq!(back.map.node_count(), default_mindmap().map.node_count());
    }

    /// The placed map is a map, not a lone box. There is no way to add a node yet, so
    /// what is placed has to already read as a mind map.
    #[test]
    fn the_default_map_has_two_levels_under_its_root() {
        let model = default_mindmap();
        let root = model.map.root();
        assert_eq!(model.map.children(root).len(), 3);
        assert_eq!(model.map.node_count(), 1 + 3 + 5);
        assert!(model.map.validate().is_ok());
    }

    /// The reason [`measured`] exists: `vellum-mindmap` never measures, and its 160 × 44
    /// default is the same for every label. Shaping has to tell "I" from "Wwwwwwwwww".
    #[test]
    fn shaping_sizes_a_node_to_its_own_label() {
        let mut engine = engine();
        let mut map = MindMap::new("I");
        let root = map.root();
        let wide = map.add_child(root, Node::new("Wwwwwwwwwwwwwwww")).unwrap();
        let model = MindMapModel {
            map,
            kind: LayoutKind::default(),
            connectors: ConnectorShape::default(),
        };

        let sized = measured(&model, &mut engine);
        let narrow = sized.get(root).unwrap().size;
        let broad = sized.get(wide).unwrap().size;
        assert!(broad.width > narrow.width, "{} was not wider than {}", broad.width, narrow.width);
        assert!(narrow.width >= MIN_NODE_WIDTH, "an empty-ish node shrank to {}", narrow.width);
    }

    /// An empty label is still a node, and still has a line of height.
    #[test]
    fn an_empty_node_keeps_a_line_of_height() {
        let mut engine = engine();
        let model = MindMapModel {
            map: MindMap::new(""),
            kind: LayoutKind::default(),
            connectors: ConnectorShape::default(),
        };
        let sized = measured(&model, &mut engine);
        let size = sized.get(sized.root()).unwrap().size;
        assert!(size.height >= NodeStyle::default().font_size, "collapsed to {}", size.height);
        assert!(size.width >= MIN_NODE_WIDTH);
    }

    /// The translation that makes a node rect an offset from the item's top-left. A
    /// tree layout starts at the root's centre, so without it half the map is negative.
    #[test]
    fn the_layout_is_slid_so_its_bounds_start_at_the_origin() {
        let mut engine = engine();
        let out = layout(&default_mindmap(), &mut engine);
        let bounds = out.bounds().expect("a nine-node map has bounds");
        assert!(bounds.min.x.abs() < 1e-6, "left edge at {}", bounds.min.x);
        assert!(bounds.min.y.abs() < 1e-6, "top edge at {}", bounds.min.y);
        assert!(out.placements().iter().all(|p| p.rect.min.x >= -1e-6 && p.rect.min.y >= -1e-6));
    }

    /// One branch per non-root node, whichever connector form is asked for.
    #[test]
    fn every_node_but_the_root_has_a_branch() {
        let mut engine = engine();
        for shape in [ConnectorShape::Straight, ConnectorShape::Elbow, ConnectorShape::Curve] {
            let model = MindMapModel { connectors: shape, ..default_mindmap() };
            let out = layout(&model, &mut engine);
            let links = connectors(&model, &out);
            assert_eq!(links.len(), out.len() - 1, "{shape:?} produced {} links", links.len());
            assert!(links.iter().all(|l| l.points.len() >= 2));
        }
    }

    /// The default item box is close enough to the natural size that a placed map draws
    /// at roughly 1:1. Loose on purpose — the drawing path fits it either way — but a
    /// figure that had drifted by 3× would place a legible map at a silly scale.
    #[test]
    fn the_default_item_box_is_about_the_natural_size() {
        let mut engine = engine();
        let (w, h) = natural_size(&layout(&default_mindmap(), &mut engine));
        let scale = (DEFAULT_SIZE.0 / w).min(DEFAULT_SIZE.1 / h);
        assert!((0.6..=1.6).contains(&scale), "a placed map would draw at {scale:.2}×");
    }

    /// Every layout form lays the same tree out without overlapping it — the property
    /// `vellum-mindmap` guarantees, checked here against boxes sized by real shaping
    /// rather than by its 160 × 44 default, which is where it could plausibly break.
    #[test]
    fn shaped_boxes_do_not_overlap_in_any_layout_form() {
        use vellum_mindmap::{Axis, Direction};
        let mut engine = engine();
        for kind in [
            LayoutKind::Tree { direction: Direction::Right },
            LayoutKind::Tree { direction: Direction::Down },
            LayoutKind::Balanced { axis: Axis::Horizontal },
            LayoutKind::Radial { start_angle: 0.0, sweep: std::f64::consts::TAU },
        ] {
            let out = layout(&MindMapModel { kind, ..default_mindmap() }, &mut engine);
            let rects: Vec<_> = out.placements().iter().map(|p| p.rect).collect();
            for (i, a) in rects.iter().enumerate() {
                for b in &rects[i + 1..] {
                    assert!(!a.intersects(*b), "{kind:?} overlapped {a:?} with {b:?}");
                }
            }
        }
    }
}
