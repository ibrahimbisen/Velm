//! What the properties panel shows, derived from what is selected.
//!
//! This is the chrome's one real state machine, and it is deliberately separated
//! from the widgets that draw it: given a slice of [`SelectionItem`]s it produces a
//! [`PanelModel`] with no `egui` involvement at all, so the rules — which sections
//! appear, which values are shared, which read *Mixed* — are testable without a
//! window.
//!
//! # The mixed-value rule
//!
//! Miro shows a property's value when every selected object agrees and a placeholder
//! when they do not, and editing a mixed property assigns to all of them. [`Field`]
//! is that rule, made explicit rather than reconstructed at each control:
//!
//! - [`Field::Absent`] — no selected object *has* this property. The control is not
//!   drawn. A selection of pen strokes has no font size.
//! - [`Field::Uniform`] — every object that has the property agrees.
//! - [`Field::Mixed`] — they disagree.
//!
//! Objects that do not have a property are skipped rather than counted as
//! disagreement: selecting a sticky and an ink stroke still shows one fill colour,
//! because the stroke never had one to conflict with.

use vellum_connect::{AnchorSide, Arrowhead, LineStyle, RoutingMode};
use vellum_doc::{Align, CardMode, Color, ItemId, Placement};

/// A property's value across a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Field<T> {
    /// Nothing in the selection carries this property.
    #[default]
    Absent,
    /// Everything that carries it agrees.
    Uniform(T),
    /// They disagree. The panel shows a placeholder, and an edit assigns to all.
    Mixed,
}

impl<T> Field<T> {
    /// Folds the values of every object that has the property.
    pub fn collect(values: impl IntoIterator<Item = T>) -> Self
    where
        T: PartialEq,
    {
        let mut iter = values.into_iter();
        let Some(first) = iter.next() else { return Self::Absent };
        if iter.all(|v| v == first) { Self::Uniform(first) } else { Self::Mixed }
    }

    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Uniform(v) => Some(v),
            Self::Absent | Self::Mixed => None,
        }
    }

    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub const fn is_mixed(&self) -> bool {
        matches!(self, Self::Mixed)
    }

    /// The value to show in a control that must display *something*, falling back to
    /// `default` when the field is mixed or absent. Editing from that fallback still
    /// assigns to the whole selection, which is exactly Miro's behaviour.
    pub fn or(&self, default: T) -> T
    where
        T: Clone,
    {
        self.value().cloned().unwrap_or(default)
    }
}

/// What an item is, for the purpose of deciding which controls apply.
///
/// There is no comment facet: `docs/features/README.md` §11 cuts comment threads with
/// the rest of collaboration, so nothing on a board can be one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemFacet {
    Sticky,
    Text,
    Shape,
    /// A link or oEmbed card. Its own facet rather than [`ItemFacet::Image`] — which is what
    /// it used to report, so the panel called a link "Image" and offered it a picture's
    /// controls. A card's editable properties are its display mode and where it points.
    Link,
    Table,
    Chart,
    MindMap,
    Kanban,
    Ink,
    Image,
    Frame,
    Connector,
    Group,
}

impl ItemFacet {
    /// The singular noun used in the panel header.
    pub const fn noun(self) -> &'static str {
        match self {
            Self::Sticky => "Sticky note",
            Self::Text => "Text",
            Self::Shape => "Shape",
            Self::Link => "Link",
            Self::Table => "Table",
            Self::Chart => "Chart",
            Self::MindMap => "Mind map",
            Self::Kanban => "Kanban board",
            Self::Ink => "Drawing",
            Self::Image => "Image",
            Self::Frame => "Frame",
            Self::Connector => "Connector",
            Self::Group => "Group",
        }
    }
}

/// How a shape or sticky's outline is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Border {
    pub color: Color,
    /// Width in world units.
    pub width: f64,
    pub style: LineStyle,
}

/// Text weight, as offered in the panel.
///
/// Three steps rather than the full CSS 100–900 range: Miro offers exactly these,
/// and a weight with no matching face in the chosen family is a synthesised bold,
/// which looks worse than not offering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontWeight {
    #[default]
    Regular,
    Medium,
    Bold,
}

impl FontWeight {
    pub const ALL: [Self; 3] = [Self::Regular, Self::Medium, Self::Bold];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Regular => "Regular",
            Self::Medium => "Medium",
            Self::Bold => "Bold",
        }
    }

    /// The CSS numeric weight, which is what a font query needs.
    pub const fn css(self) -> u16 {
        match self {
            Self::Regular => 400,
            Self::Medium => 500,
            Self::Bold => 700,
        }
    }
}

/// Vertical placement of a text block inside its box.
///
/// `vellum-doc` models horizontal alignment only, because that is all Miro's `ta`
/// key carries; vertical alignment is a layout parameter the renderer applies, so it
/// lives here until the document grows a field for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VerticalAlign {
    Top,
    #[default]
    Middle,
    Bottom,
}

impl VerticalAlign {
    pub const ALL: [Self; 3] = [Self::Top, Self::Middle, Self::Bottom];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Top => "Top",
            Self::Middle => "Middle",
            Self::Bottom => "Bottom",
        }
    }
}

/// The text properties of one item.
#[derive(Debug, Clone, PartialEq)]
pub struct TextSummary {
    /// The words themselves, flattened to plain text.
    ///
    /// Plain rather than styled because the panel's own field is plain. The **canvas** caret
    /// is not — it preserves an item's runs across an edit — so this is what the panel shows
    /// and edits, not the whole of what the item can hold. Typing here still flattens nothing:
    /// the app splices this string into the item's existing runs, exactly as a keystroke on the
    /// canvas does.
    pub content: String,
    /// `None` is the board default rather than a named family.
    pub family: Option<String>,
    /// `None` is auto-fit — Miro's `fs: 0, fsa: 1`.
    pub size: Option<f64>,
    pub weight: FontWeight,
    pub color: Color,
    pub align: Align,
    pub vertical_align: VerticalAlign,
    /// A multiple of the font size, matching Miro's `lh`.
    pub line_height: f64,
}

/// The link properties of one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSummary {
    /// Where the card points. `None` for a card whose URL was never recorded — Miro serves
    /// a few — which is why Open is disabled rather than absent for those.
    pub url: Option<String>,
    /// The site's name, for the panel to show beside the mode.
    pub provider: Option<String>,
    pub mode: CardMode,
    /// Whether a preview image has been fetched. Decides whether *Large* has anything more to
    /// show than *Card*, which the panel says rather than leaving the user to guess.
    pub has_image: bool,
}

/// The connector properties of one item.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnectorSummary {
    pub routing: RoutingMode,
    pub start_arrow: Arrowhead,
    pub end_arrow: Arrowhead,
    /// Where each end attaches. See [`AnchorSide`].
    pub start_anchor: AnchorSide,
    pub end_anchor: AnchorSide,
}

/// One selected item, as the app describes it to the chrome.
///
/// A flattened view rather than a document handle: the chrome never reads the CRDT,
/// so a panel can be rendered from a test fixture and the document can change shape
/// without the UI following it.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionItem {
    pub id: ItemId,
    pub facet: ItemFacet,
    pub placement: Placement,
    pub locked: bool,
    /// `None` for kinds with no fill at all; `Some(None)` for "no fill", which is a
    /// choice a shape can make and a sticky cannot.
    pub fill: Option<Option<Color>>,
    pub border: Option<Border>,
    /// Whole-item opacity, 0.0–1.0.
    pub opacity: Option<f64>,
    pub text: Option<TextSummary>,
    pub connector: Option<ConnectorSummary>,
    /// Present for a link or embed card.
    pub link: Option<LinkSummary>,
}

impl SelectionItem {
    /// A minimal item, for tests and for kinds whose properties the app has not
    /// filled in yet.
    pub fn new(id: ItemId, facet: ItemFacet, placement: Placement) -> Self {
        Self {
            id,
            facet,
            placement,
            locked: false,
            fill: None,
            border: None,
            opacity: None,
            text: None,
            connector: None,
            link: None,
        }
    }
}

/// An axis-aligned rectangle in world units, as shown in the position fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The properties panel's contents for a given selection.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelModel {
    pub count: usize,
    /// "Sticky note", "3 shapes", "4 objects".
    pub headline: String,
    pub fill: Field<Option<Color>>,
    pub border_color: Field<Color>,
    pub border_width: Field<f64>,
    pub border_style: Field<LineStyle>,
    pub opacity: Field<f64>,
    pub font_family: Field<Option<String>>,
    pub font_size: Field<Option<f64>>,
    pub font_weight: Field<FontWeight>,
    pub text_color: Field<Color>,
    pub align: Field<Align>,
    pub vertical_align: Field<VerticalAlign>,
    pub line_height: Field<f64>,
    pub routing: Field<RoutingMode>,
    pub start_arrow: Field<Arrowhead>,
    pub end_arrow: Field<Arrowhead>,
    pub start_anchor: Field<AnchorSide>,
    pub end_anchor: Field<AnchorSide>,
    /// The display mode of every selected card, folded. Absent when nothing selected is one.
    pub card_mode: Field<CardMode>,
    /// The one selected card's URL, when exactly one card is selected. Single-selection only,
    /// because *Open* acts on one page and opening forty tabs is not a thing anybody means.
    pub link_url: Option<String>,
    /// Whether every selected card already has a preview image.
    pub link_has_image: Field<bool>,
    /// Absent only when nothing is selected — every item has a lock state.
    pub locked: Field<bool>,
    pub rotation: Field<f64>,
    /// The selection's bounding box, or `None` when nothing is selected.
    pub bounds: Option<Bounds>,
    /// Whether width and height may be typed into.
    ///
    /// False for a multi-selection: a single number cannot describe several
    /// differently-sized items, and silently resizing them all to it is a data-loss
    /// gesture disguised as a text field. Position stays editable because moving a
    /// group by its bounding box has one obvious meaning.
    pub size_editable: bool,
    /// The one selected item's id, when exactly one is selected. What
    /// [`Self::text_content`] belongs to.
    pub single_id: Option<ItemId>,
    /// The words of the one selected item, when exactly one is selected and it holds
    /// text at all.
    ///
    /// Single-selection only, and that is the whole rule rather than a `Field`:
    /// typing one paragraph into forty stickies at once is not an edit anybody makes
    /// on purpose, and offering it is how you lose forty notes with one keystroke.
    pub text_content: Option<String>,
}

impl PanelModel {
    pub fn empty() -> Self {
        Self::derive(&[])
    }

    /// Folds a selection into everything the panel needs.
    pub fn derive(selection: &[SelectionItem]) -> Self {
        let text = || selection.iter().filter_map(|i| i.text.as_ref());
        let border = || selection.iter().filter_map(|i| i.border.as_ref());
        let connector = || selection.iter().filter_map(|i| i.connector.as_ref());
        let link = || selection.iter().filter_map(|i| i.link.as_ref());

        Self {
            count: selection.len(),
            headline: headline(selection),
            fill: Field::collect(selection.iter().filter_map(|i| i.fill)),
            border_color: Field::collect(border().map(|b| b.color)),
            border_width: Field::collect(border().map(|b| b.width)),
            border_style: Field::collect(border().map(|b| b.style)),
            opacity: Field::collect(selection.iter().filter_map(|i| i.opacity)),
            font_family: Field::collect(text().map(|t| t.family.clone())),
            font_size: Field::collect(text().map(|t| t.size)),
            font_weight: Field::collect(text().map(|t| t.weight)),
            text_color: Field::collect(text().map(|t| t.color)),
            align: Field::collect(text().map(|t| t.align)),
            vertical_align: Field::collect(text().map(|t| t.vertical_align)),
            line_height: Field::collect(text().map(|t| t.line_height)),
            routing: Field::collect(connector().map(|c| c.routing)),
            start_arrow: Field::collect(connector().map(|c| c.start_arrow)),
            end_arrow: Field::collect(connector().map(|c| c.end_arrow)),
            start_anchor: Field::collect(connector().map(|c| c.start_anchor)),
            end_anchor: Field::collect(connector().map(|c| c.end_anchor)),
            card_mode: Field::collect(link().map(|l| l.mode)),
            link_url: match selection {
                [only] => only.link.as_ref().and_then(|l| l.url.clone()),
                _ => None,
            },
            link_has_image: Field::collect(link().map(|l| l.has_image)),
            locked: Field::collect(selection.iter().map(|i| i.locked)),
            rotation: Field::collect(selection.iter().map(|i| i.placement.rotation)),
            bounds: bounding_box(selection),
            size_editable: selection.len() == 1,
            single_id: match selection {
                [one] => Some(one.id),
                _ => None,
            },
            text_content: match selection {
                [one] => one.text.as_ref().map(|t| t.content.clone()),
                _ => None,
            },
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether the fill / border / opacity block has anything to show.
    pub const fn has_appearance(&self) -> bool {
        !(self.fill.is_absent()
            && self.border_color.is_absent()
            && self.opacity.is_absent())
    }

    /// Whether the typography block has anything to show.
    pub const fn has_text(&self) -> bool {
        !self.font_family.is_absent()
    }

    /// Whether the selection holds a link or embed card.
    pub const fn has_link(&self) -> bool {
        !self.card_mode.is_absent()
    }

    pub const fn has_connector(&self) -> bool {
        !self.routing.is_absent()
    }

    /// True when every selected item is locked, which is what flips the panel's lock
    /// control and the Object menu's Lock/Unlock pair.
    pub fn all_locked(&self) -> bool {
        matches!(self.locked, Field::Uniform(true))
    }

    pub fn any_locked(&self) -> bool {
        matches!(self.locked, Field::Uniform(true) | Field::Mixed)
    }
}

/// "Sticky note" for one, "3 sticky notes" when they agree, "4 objects" when they
/// do not — the same escalation Miro uses, and the reason the panel header is worth
/// deriving rather than hard-coding to "Selection".
fn headline(selection: &[SelectionItem]) -> String {
    match selection {
        [] => "Nothing selected".to_owned(),
        [one] => one.facet.noun().to_owned(),
        many => match Field::collect(many.iter().map(|i| i.facet)) {
            // Lower-cased because the count now leads the phrase: "2 sticky notes",
            // not "2 Sticky notes".
            Field::Uniform(facet) => {
                format!("{} {}", many.len(), plural(facet.noun()).to_lowercase())
            }
            _ => format!("{} objects", many.len()),
        },
    }
}

/// English plurals for the nine nouns in [`ItemFacet`]. None of them is irregular,
/// so a suffix rule is enough and a table would only be a place for them to drift.
fn plural(noun: &str) -> String {
    if noun.ends_with('s') || noun.ends_with("ch") || noun.ends_with('x') {
        format!("{noun}es")
    } else {
        format!("{noun}s")
    }
}

/// The union of every item's placement, ignoring rotation.
///
/// Rotation is deliberately not accounted for: the numeric fields edit an item's
/// *stored* x/y/width/height, and expanding the box to cover a rotated item's corners
/// would mean typing a width that the item does not then have.
fn bounding_box(selection: &[SelectionItem]) -> Option<Bounds> {
    let mut items = selection.iter();
    let first = items.next()?;
    let (w, h) = first.placement.scaled_size();
    let (mut min_x, mut min_y) = (first.placement.x, first.placement.y);
    let (mut max_x, mut max_y) = (min_x + w, min_y + h);

    for item in items {
        let (w, h) = item.placement.scaled_size();
        min_x = min_x.min(item.placement.x);
        min_y = min_y.min(item.placement.y);
        max_x = max_x.max(item.placement.x + w);
        max_y = max_y.max(item.placement.y + h);
    }

    Some(Bounds { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: i32) -> ItemId {
        format!("{n}@1").parse().expect("well-formed item id")
    }

    fn sticky(n: i32, fill: Color) -> SelectionItem {
        SelectionItem {
            fill: Some(Some(fill)),
            opacity: Some(1.0),
            text: Some(TextSummary {
                content: "radiator fan".to_owned(),
                family: Some("Noto Sans".to_owned()),
                size: Some(14.0),
                weight: FontWeight::Regular,
                color: Color::rgb(0, 0, 0),
                align: Align::Center,
                vertical_align: VerticalAlign::Middle,
                line_height: 1.2,
            }),
            ..SelectionItem::new(
                id(n),
                ItemFacet::Sticky,
                Placement::new(f64::from(n) * 100.0, 0.0, 200.0, 200.0),
            )
        }
    }

    fn ink(n: i32) -> SelectionItem {
        SelectionItem::new(id(n), ItemFacet::Ink, Placement::new(0.0, 0.0, 50.0, 50.0))
    }

    #[test]
    fn an_empty_selection_has_no_bounds_and_no_sections() {
        let model = PanelModel::empty();
        assert!(model.is_empty());
        assert_eq!(model.headline, "Nothing selected");
        assert!(model.bounds.is_none());
        assert!(!model.has_appearance());
        assert!(!model.has_text());
        assert!(!model.has_connector());
        assert!(model.locked.is_absent());
    }

    #[test]
    fn a_single_item_reports_every_value_as_uniform() {
        let yellow = Color::rgb(0xFF, 0xF7, 0x9E);
        let model = PanelModel::derive(&[sticky(1, yellow)]);
        assert_eq!(model.headline, "Sticky note");
        assert_eq!(model.fill, Field::Uniform(Some(yellow)));
        assert_eq!(model.align, Field::Uniform(Align::Center));
        assert!(model.size_editable);
    }

    #[test]
    fn disagreeing_values_read_as_mixed_while_agreeing_ones_do_not() {
        let model = PanelModel::derive(&[
            sticky(1, Color::rgb(0xFF, 0xF7, 0x9E)),
            sticky(2, Color::rgb(0xFF, 0x9E, 0x9E)),
        ]);
        assert!(model.fill.is_mixed());
        assert_eq!(model.font_size, Field::Uniform(Some(14.0)));
        assert_eq!(model.headline, "2 sticky notes");
    }

    /// The rule that makes mixed selections usable: an object without a property
    /// abstains rather than voting against.
    #[test]
    fn items_lacking_a_property_do_not_make_it_mixed() {
        let yellow = Color::rgb(0xFF, 0xF7, 0x9E);
        let model = PanelModel::derive(&[sticky(1, yellow), ink(2)]);
        assert_eq!(model.fill, Field::Uniform(Some(yellow)));
        assert!(model.has_text(), "the sticky still contributes typography");
        assert_eq!(model.headline, "2 objects");
    }

    #[test]
    fn a_selection_of_only_ink_offers_no_fill_and_no_typography() {
        let model = PanelModel::derive(&[ink(1), ink(2)]);
        assert!(model.fill.is_absent());
        assert!(!model.has_text());
        assert_eq!(model.headline, "2 drawings");
    }

    #[test]
    fn no_fill_is_a_value_that_can_be_shared_rather_than_an_absence() {
        let mut a = sticky(1, Color::rgb(0, 0, 0));
        let mut b = sticky(2, Color::rgb(0, 0, 0));
        a.fill = Some(None);
        b.fill = Some(None);
        let model = PanelModel::derive(&[a, b]);
        assert_eq!(model.fill, Field::Uniform(None), "both agree on 'no fill'");
        assert!(model.has_appearance());
    }

    #[test]
    fn bounds_are_the_union_and_size_is_locked_for_a_multi_selection() {
        let yellow = Color::rgb(0xFF, 0xF7, 0x9E);
        let model = PanelModel::derive(&[sticky(1, yellow), sticky(3, yellow)]);
        let bounds = model.bounds.expect("two items have bounds");
        assert_eq!(bounds.x, 100.0);
        assert_eq!(bounds.width, 400.0, "100..500");
        assert_eq!(bounds.height, 200.0);
        assert!(!model.size_editable);
    }

    /// `Placement::scale` multiplies the stored size, so the box has to use the
    /// scaled one or a scaled item hangs outside its own selection rectangle.
    #[test]
    fn bounds_account_for_placement_scale() {
        let mut item = sticky(0, Color::rgb(0, 0, 0));
        item.placement.scale = 2.0;
        let bounds = PanelModel::derive(&[item]).bounds.unwrap();
        assert_eq!(bounds.width, 400.0);
    }

    #[test]
    fn lock_state_drives_the_all_locked_flag() {
        let mut a = sticky(1, Color::rgb(0, 0, 0));
        let mut b = sticky(2, Color::rgb(0, 0, 0));
        assert!(!PanelModel::derive(&[a.clone(), b.clone()]).all_locked());

        a.locked = true;
        let half = PanelModel::derive(&[a.clone(), b.clone()]);
        assert!(!half.all_locked());
        assert!(half.any_locked());
        assert!(half.locked.is_mixed());

        b.locked = true;
        assert!(PanelModel::derive(&[a, b]).all_locked());
    }

    #[test]
    fn connector_properties_appear_only_for_connectors() {
        let mut wire = SelectionItem::new(
            id(9),
            ItemFacet::Connector,
            Placement::new(0.0, 0.0, 10.0, 10.0),
        );
        wire.connector = Some(ConnectorSummary {
            routing: RoutingMode::Straight,
            start_arrow: Arrowhead::None,
            end_arrow: Arrowhead::FilledTriangle,
            start_anchor: AnchorSide::Right,
            end_anchor: AnchorSide::Left,
        });
        wire.border = Some(Border {
            color: Color::rgb(0x33, 0x33, 0x33),
            width: 2.0,
            style: LineStyle::Solid,
        });

        let model = PanelModel::derive(&[wire]);
        assert!(model.has_connector());
        assert_eq!(model.end_arrow, Field::Uniform(Arrowhead::FilledTriangle));
        assert_eq!(model.border_width, Field::Uniform(2.0));
        assert!(!model.has_text());
        // The anchor picker's two rows come through the same fold as the arrowheads.
        assert_eq!(model.start_anchor, Field::Uniform(AnchorSide::Right));
        assert_eq!(model.end_anchor, Field::Uniform(AnchorSide::Left));

        let drawing = ink(1);
        let drawing = std::slice::from_ref(&drawing);
        assert!(!PanelModel::derive(drawing).has_connector());
        assert!(
            PanelModel::derive(drawing).start_anchor.is_absent(),
            "a drawing has no ends to attach, so the rows are not drawn at all"
        );
    }

    #[test]
    fn field_falls_back_only_when_it_has_no_agreed_value() {
        assert_eq!(Field::Uniform(3_u8).or(9), 3);
        assert_eq!(Field::<u8>::Mixed.or(9), 9);
        assert_eq!(Field::<u8>::Absent.or(9), 9);
    }

    #[test]
    fn font_weights_map_onto_the_numbers_a_font_query_needs() {
        assert_eq!(
            FontWeight::ALL.map(FontWeight::css),
            [400, 500, 700],
            "the panel offers three weights, in order"
        );
        assert!(FontWeight::ALL.iter().all(|w| !w.label().is_empty()));
        assert_eq!(FontWeight::default(), FontWeight::Regular);
    }

    #[test]
    fn plurals_cover_every_facet_noun() {
        assert_eq!(plural("Sticky note"), "Sticky notes");
        assert_eq!(plural("Text"), "Texts");
        assert_eq!(plural("Box"), "Boxes");
        assert_eq!(plural("Sketch"), "Sketches");
    }
}
