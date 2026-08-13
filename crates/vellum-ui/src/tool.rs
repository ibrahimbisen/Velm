//! The tools in the left palette, and what each one puts on the board.

use crate::icon::Icon;
use egui::Key;
use vellum_doc::Color;
use vellum_shapes::Shape;

/// A canvas tool.
///
/// The order is the palette's order, and it groups by what the tool produces:
/// navigation first, then the four content tools, then drawing, then structure.
///
/// **Miro's Comment tool is absent by design.** `docs/features/README.md` §11 cuts
/// collaboration entirely — no comment threads, no mentions, no reactions — and
/// `docs/04-ui-reference.md` §1's *↑improve* is explicit that the collaboration
/// entries are omitted rather than stubbed: no dead buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Tool {
    #[default]
    Select,
    Hand,
    Sticky,
    Text,
    Shape,
    Pen,
    Eraser,
    Connector,
    Frame,
    Table,
    Chart,
    MindMap,
    Kanban,
    Image,
    /// Places a live agent node. The Agent Canvas layer's headline tool, and the one of
    /// the four that is on the palette itself rather than behind **More** — see
    /// [`Tool::OCCASIONAL`].
    Agent,
    /// Places a note node: a markdown file on disk that agents read and write.
    Note,
    /// Places a file-tree node, scoped to one agent.
    FileTree,
    /// Places a browser node. Opt-in and RAM-gated; the tool exists whether or not an
    /// engine is enabled, because the node is legible either way.
    Browser,
}

impl Tool {
    pub const ALL: [Self; 18] = [
        Self::Select,
        Self::Hand,
        Self::Sticky,
        Self::Text,
        Self::Shape,
        Self::Pen,
        Self::Eraser,
        Self::Connector,
        Self::Frame,
        Self::Table,
        Self::Chart,
        Self::MindMap,
        Self::Kanban,
        Self::Image,
        Self::Agent,
        Self::Note,
        Self::FileTree,
        Self::Browser,
    ];

    /// The tools folded behind the palette's **More** button.
    ///
    /// *"put table charts kanabn and mindmap image and the connector into a smaller menu
    /// in this bar i just dont use those enough"* — the supplied list, in their order,
    /// and the rationale is theirs too. These are not lesser tools; they are the ones
    /// this user reaches for occasionally, and a palette is a ranking of *frequency*, not
    /// of importance.
    ///
    /// Every one keeps its keyboard shortcut, so nothing here got slower for anyone who
    /// knows the key — folding changes the palette, not the bindings.
    /// **The agent tool itself is deliberately not here.** Three of the Agent Canvas tools
    /// are — a note, a file tree and a browser are things you place occasionally, around
    /// the agents — but placing an agent is the layer's whole verb, and a headline feature
    /// folded behind a **More** button is one nobody discovers.
    pub const OCCASIONAL: [Self; 9] = [
        Self::Table,
        Self::Chart,
        Self::Kanban,
        Self::MindMap,
        Self::Image,
        Self::Connector,
        Self::Note,
        Self::FileTree,
        Self::Browser,
    ];

    /// Whether this tool lives behind **More** rather than on the palette itself.
    pub fn is_occasional(self) -> bool {
        Self::OCCASIONAL.contains(&self)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Hand => "Hand",
            Self::Sticky => "Sticky note",
            Self::Text => "Text",
            Self::Shape => "Shape",
            Self::Pen => "Pen",
            Self::Eraser => "Eraser",
            Self::Connector => "Connector",
            Self::Frame => "Frame",
            Self::Table => "Table",
            Self::Chart => "Chart",
            Self::MindMap => "Mind map",
            Self::Kanban => "Kanban board",
            Self::Image => "Image",
            Self::Agent => "Agent",
            Self::Note => "Note",
            Self::FileTree => "File tree",
            Self::Browser => "Browser",
        }
    }

    /// The single-key binding, matching Miro's so muscle memory transfers —
    /// `docs/features/README.md` §9. Image has no key in Miro either.
    pub const fn shortcut(self) -> Option<Key> {
        Some(match self {
            Self::Select => Key::V,
            Self::Hand => Key::H,
            Self::Sticky => Key::N,
            Self::Text => Key::T,
            Self::Shape => Key::S,
            Self::Pen => Key::P,
            Self::Eraser => Key::E,
            Self::Connector => Key::C,
            Self::Frame => Key::F,
            // `A` for an agent. It is free — `⌘A` is Select all, and a bare letter is a
            // different binding from a chord, exactly as bare `V`, `N` and `T` already are.
            Self::Agent => Key::A,
            // Miro has no bare key for a table; `T` is text and already taken.
            Self::Table | Self::Chart | Self::MindMap | Self::Kanban => return None,
            // No bare key for the other three. Every remaining letter that reads as one of
            // these is taken by a tool the hand uses far more often — `N` is a sticky note,
            // `F` is a frame, `B` would be the obvious browser key and is one keystroke from
            // being pressed by accident while a caret is not up. A tool nobody places daily
            // does not earn a scarce single key.
            Self::Note | Self::FileTree | Self::Browser => return None,
            Self::Image => return None,
        })
    }

    /// A second key that also selects the tool.
    ///
    /// One tool has one. Miro's shape flyout binds `L` to *Line*, and a line between
    /// two points is a connector in this document model — `vellum-shapes` has no
    /// `Line` entry because a line is not a closed silhouette. So `L` reaches the
    /// connector tool rather than being a shortcut with nothing behind it.
    pub const fn alternate_shortcut(self) -> Option<Key> {
        match self {
            Self::Connector => Some(Key::L),
            _ => None,
        }
    }

    /// The letter shown in the tooltip. Derived from [`Tool::shortcut`] rather than
    /// written twice, so the hint cannot disagree with the binding.
    pub fn shortcut_hint(self) -> Option<&'static str> {
        self.shortcut().map(Key::name)
    }

    /// Whether the tool stays armed after it is used once.
    ///
    /// Every other create tool disarms, because a stray click with it makes an item
    /// nobody asked for. The drawing tools carry no such risk — a click that does not
    /// travel leaves no stroke to keep and erases nothing — and disarming them means
    /// re-arming between every mark, which is not how anyone draws.
    pub const fn is_continuous(self) -> bool {
        matches!(self, Self::Pen | Self::Eraser)
    }

    pub const fn icon(self) -> Icon {
        match self {
            Self::Select => Icon::Select,
            Self::Hand => Icon::Hand,
            Self::Sticky => Icon::Sticky,
            Self::Text => Icon::Text,
            Self::Shape => Icon::Shape,
            Self::Pen => Icon::Pen,
            Self::Eraser => Icon::Eraser,
            Self::Connector => Icon::Connector,
            Self::Frame => Icon::Frame,
            Self::Table => Icon::Table,
            Self::Chart => Icon::Chart,
            Self::MindMap => Icon::MindMap,
            Self::Kanban => Icon::Kanban,
            Self::Image => Icon::Image,
            Self::Agent => Icon::Agent,
            Self::Note => Icon::Note,
            Self::FileTree => Icon::FileTree,
            Self::Browser => Icon::Browser,
        }
    }

    /// Whether the palette button opens a picker beside it.
    pub const fn flyout(self) -> Option<Flyout> {
        match self {
            Self::Shape => Some(Flyout::Shape),
            Self::Pen => Some(Flyout::Pen),
            Self::Eraser => Some(Flyout::Eraser),
            Self::Sticky => Some(Flyout::Sticky),
            Self::Agent => Some(Flyout::Agent),
            _ => None,
        }
    }
}

/// Which picker a tool button opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flyout {
    /// The sticky's colour, before it is placed.
    ///
    /// *"when i press sticky notes, left menu on velm, i want to see a menu like this so that
    /// i can select which one i want to use in terms of colours."* Velm had no sticky flyout at
    /// all: a note came out yellow and was recoloured afterwards from the bar above it, which
    /// is one more step every time and is not what the hand expects from a tool that has a
    /// colour. Miro shows the pack before you place anything.
    Sticky,
    Shape,
    Pen,
    Eraser,
    /// Which of the three roles the agent tool places: worker, orchestrator or meta.
    ///
    /// A flyout rather than a conversion after the fact, for the sticky's reason: the three
    /// are configured differently from the moment they exist — an orchestrator wants a
    /// territory drawn on the board and a cap set — so placing a worker and changing it
    /// afterwards is an extra step every single time.
    Agent,
    /// The tools that are not everyday work — see [`Tool::OCCASIONAL`].
    More,
}

/// What the eraser takes: parts of strokes, or whole objects.
///
/// Miro's own eraser is a **mode** chosen in its flyout, and this was a bare ⇧ modifier
/// with nothing on screen to say so — reachable only by reading the shortcut sheet, which
/// is a poor place to hide a gesture that deletes things. The modifier still works and is
/// still documented: holding ⇧ takes objects while [`EraserMode::Stroke`] is selected and
/// strokes while [`EraserMode::Object`] is, so it is now a *temporary inversion* of the
/// mode rather than the only way in. That is the same relationship ⇧ has to every other
/// modal tool in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EraserMode {
    /// Cuts the parts of ink strokes it passes over, splitting rather than deleting.
    /// Miro's default, and this app's.
    #[default]
    Stroke,
    /// Removes whole items. Locked items survive it.
    Object,
}

impl EraserMode {
    pub const ALL: [Self; 2] = [Self::Stroke, Self::Object];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Stroke => "Strokes",
            Self::Object => "Objects",
        }
    }

    /// What the flyout says under the toggle, so the mode explains itself in place.
    pub const fn hint(self) -> &'static str {
        match self {
            Self::Stroke => "Cuts through drawings. Hold ⇧ to take whole objects.",
            Self::Object => "Removes whole items; locked ones survive. Hold ⇧ to cut strokes.",
        }
    }

    /// The mode a dab actually runs in, given whether ⇧ is held.
    ///
    /// One function so the inversion is defined once rather than at the call site and in
    /// the flyout's hint separately — the two disagreeing is precisely how a modifier ends
    /// up meaning different things in the tooltip and in the document.
    pub const fn with_shift(self, shift: bool) -> Self {
        match (self, shift) {
            (Self::Stroke, false) | (Self::Object, true) => Self::Stroke,
            (Self::Object, false) | (Self::Stroke, true) => Self::Object,
        }
    }
}

/// The bare keys Miro binds inside its shape flyout, from
/// `docs/04-ui-reference.md` §2.
///
/// Miro lists three — `L` line, `R` rectangle, `O` oval — and two of them name a
/// catalogue shape. The third is [`Tool::alternate_shortcut`], for the reason given
/// there. Pressing one of these both picks the shape and arms the shape tool, which
/// is what Miro does and what makes the key worth having: one keystroke from
/// "thinking about a rectangle" to "dragging one out".
pub const SHAPE_SHORTCUTS: [(Key, Shape); 2] =
    [(Key::R, Shape::Rectangle), (Key::O, Shape::Ellipse)];

/// The shape a bare key arms, if any.
pub fn shape_shortcut(key: Key) -> Option<Shape> {
    SHAPE_SHORTCUTS.iter().find(|(k, _)| *k == key).map(|(_, shape)| *shape)
}

/// How a freehand stroke is laid down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PenKind {
    /// Opaque, pressure-varying.
    #[default]
    Pen,
    /// Opaque and thick, no pressure response.
    Marker,
    /// Translucent, drawn under the strokes it crosses.
    Highlighter,
}

impl PenKind {
    pub const ALL: [Self; 3] = [Self::Pen, Self::Marker, Self::Highlighter];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pen => "Pen",
            Self::Marker => "Marker",
            Self::Highlighter => "Highlighter",
        }
    }

    /// Stroke alpha. A highlighter that is not translucent is a marker.
    pub const fn default_alpha(self) -> u8 {
        match self {
            Self::Pen | Self::Marker => 0xFF,
            Self::Highlighter => 0x66,
        }
    }

    pub const fn default_width(self) -> f32 {
        match self {
            Self::Pen => 4.0,
            Self::Marker => 10.0,
            Self::Highlighter => 24.0,
        }
    }
}

/// The pen tool's current settings, as shown in its flyout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenPreset {
    pub kind: PenKind,
    /// Stroke width in world units.
    pub width: f32,
    pub color: Color,
}

impl PenPreset {
    /// The widths offered in the flyout. Five steps rather than a slider, because a
    /// stroke width is chosen by eye and re-chosen constantly.
    pub const WIDTHS: [f32; 5] = [2.0, 4.0, 8.0, 16.0, 28.0];

    pub const fn new(kind: PenKind, color: Color) -> Self {
        Self { kind, width: kind.default_width(), color }
    }

    /// The colour a stroke is actually drawn in, with the kind's translucency folded
    /// into the alpha the way `vellum-doc` stores it — see [`Color::with_opacity`].
    pub fn stroke_color(self) -> Color {
        Color { a: self.kind.default_alpha(), ..self.color }
    }

    /// Switching kind re-derives the width, because the widths that suit a pen and a
    /// highlighter do not overlap.
    pub fn with_kind(self, kind: PenKind) -> Self {
        Self { kind, width: kind.default_width(), color: self.color }
    }
}

impl Default for PenPreset {
    fn default() -> Self {
        // The system's own ink, so a stroke laid down before the user picks a colour
        // is the same black the interface is written in rather than a fourth one.
        Self::new(PenKind::Pen, crate::color::INK)
    }
}

/// A display name for a catalogue shape.
///
/// `vellum-shapes` deliberately exposes no labels — it is a geometry crate, and a
/// user-facing string is a chrome concern. Anything not named here falls back to its
/// enum spelling, so adding a shape to the catalogue is never a compile error, only
/// a slightly ugly tooltip.
pub fn shape_label(shape: Shape) -> &'static str {
    use vellum_shapes::{ArrowForm, Direction};
    match shape {
        Shape::Rectangle => "Rectangle",
        Shape::RoundedRectangle { .. } => "Rounded rectangle",
        Shape::Ellipse => "Ellipse",
        Shape::RegularPolygon { sides: 3, .. } => "Triangle",
        Shape::RegularPolygon { sides: 5, .. } => "Pentagon",
        Shape::RegularPolygon { sides: 6, .. } => "Hexagon",
        Shape::RegularPolygon { sides: 8, .. } => "Octagon",
        Shape::RegularPolygon { .. } => "Polygon",
        Shape::RightTriangle => "Right triangle",
        Shape::Diamond => "Diamond",
        Shape::Parallelogram { .. } => "Parallelogram",
        Shape::Trapezoid { .. } => "Trapezoid",
        Shape::Star { .. } => "Star",
        Shape::Cross { .. } => "Cross",
        Shape::Arrow { form: ArrowForm::Simple, direction: Direction::Up, .. } => "Arrow up",
        Shape::Arrow { form: ArrowForm::Simple, .. } => "Arrow",
        Shape::Arrow { form: ArrowForm::Double, .. } => "Double arrow",
        Shape::Arrow { form: ArrowForm::Chevron, .. } => "Chevron",
        Shape::Arrow { form: ArrowForm::Notched, .. } => "Notched arrow",
        Shape::Arrow { form: ArrowForm::Bent, .. } => "Bent arrow",
        Shape::SpeechBubble { .. } => "Speech bubble",
        Shape::Cylinder { .. } => "Cylinder",
        Shape::Cloud => "Cloud",
        Shape::Heart => "Heart",
        Shape::Arc { .. } => "Arc",
        Shape::Wedge { .. } => "Wedge",
        Shape::Terminator => "Terminator",
        Shape::Document { .. } => "Document",
        Shape::MultiDocument { .. } => "Multi-document",
        Shape::ManualInput { .. } => "Manual input",
        Shape::ManualOperation { .. } => "Manual operation",
        Shape::PredefinedProcess { .. } => "Predefined process",
        Shape::InternalStorage { .. } => "Internal storage",
        Shape::DirectData { .. } => "Direct data",
        Shape::StoredData { .. } => "Stored data",
        Shape::Delay => "Delay",
        Shape::Display { .. } => "Display",
        Shape::Or => "Or",
        Shape::SummingJunction => "Summing junction",
        Shape::OffPageConnector { .. } => "Off-page connector",
        Shape::Database { .. } => "Database",
    }
}

/// Where a catalogue shape belongs in the flyout.
///
/// Miro splits its picker the same way, and the split is by intent rather than by
/// geometry: a terminator is a rounded rectangle, but nobody reaches for it when
/// they want a rounded rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeGroup {
    Basic,
    Flowchart,
}

impl ShapeGroup {
    pub const ALL: [Self; 2] = [Self::Basic, Self::Flowchart];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Basic => "Basic",
            Self::Flowchart => "Flowchart",
        }
    }

    pub fn contains(self, shape: Shape) -> bool {
        let flowchart = matches!(
            shape,
            Shape::Terminator
                | Shape::Document { .. }
                | Shape::MultiDocument { .. }
                | Shape::ManualInput { .. }
                | Shape::ManualOperation { .. }
                | Shape::PredefinedProcess { .. }
                | Shape::InternalStorage { .. }
                | Shape::DirectData { .. }
                | Shape::StoredData { .. }
                | Shape::Delay
                | Shape::Display { .. }
                | Shape::Or
                | Shape::SummingJunction
                | Shape::OffPageConnector { .. }
                | Shape::Database { .. }
        );
        (self == Self::Flowchart) == flowchart
    }

    /// The catalogue entries in this group, in catalogue order.
    pub fn shapes(self) -> impl Iterator<Item = Shape> {
        vellum_shapes::CATALOGUE.iter().copied().filter(move |s| self.contains(*s))
    }

    /// Its index in [`ShapeGroup::ALL`], which is how the picker stores one set of
    /// colours per category without a map.
    pub const fn index(self) -> usize {
        match self {
            Self::Basic => 0,
            Self::Flowchart => 1,
        }
    }
}

/// The colours a newly placed shape takes, per category.
///
/// `docs/04-ui-reference.md` §3 records *Apply colors* beside each category heading in
/// Miro's picker, and it is per-category rather than global for a reason worth
/// keeping: a flowchart is drawn in one palette and a set of basic shapes in another,
/// and switching between them should not mean re-picking a colour every time.
///
/// This is a **default for what gets created**, not an edit to the selection.
/// Recolouring what is already on the board is [`StyleEdit`](crate::StyleEdit), and
/// conflating the two would make picking a colour in the flyout silently repaint the
/// board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeColors {
    /// `None` is an explicit "no fill", as it is everywhere else in the chrome.
    pub fill: Option<Color>,
    pub stroke: Color,
}

impl Default for ShapeColors {
    fn default() -> Self {
        // The board's own defaults rather than a third set: a shape placed before the
        // user has picked anything matches a sticky placed the same way.
        Self { fill: Some(crate::color::DEFAULT_FILL), stroke: crate::color::INK }
    }
}

/// Identifies one of the user's uploaded SVG shapes.
///
/// Opaque, and assigned by the app: the chrome never parses an SVG — it has no
/// rasteriser and `docs/01-architecture.md` keeps it that way — so it has nothing else
/// to name a custom shape by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CustomShapeId(pub u64);

/// One of the reference shapes, as the picker lists it.
///
/// `docs/04-ui-reference.md` §3 puts *My Shapes — browse and upload SVG shapes* at the
/// top of Miro's picker and calls it a real feature worth having. The split of work is
/// the same as everywhere else in this crate: the app reads the file, rasterises a
/// preview and uploads it; the chrome shows the tile and reports the click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomShape {
    pub id: CustomShapeId,
    pub name: String,
    /// A preview the app has already uploaded to the GPU. Without one the tile draws
    /// a placeholder rather than nothing, so an upload whose preview has not been
    /// generated yet is still reachable.
    pub thumbnail: Option<crate::library::Thumbnail>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collaboration is cut, so the tool that only exists to start a conversation is
    /// not in the palette. `docs/04-ui-reference.md` §1: no dead buttons.
    #[test]
    fn the_palette_carries_no_collaboration_tool() {
        for tool in Tool::ALL {
            assert_ne!(tool.label(), "Comment", "collaboration is cut, not stubbed");
        }
    }

    #[test]
    fn tool_shortcuts_are_unique_and_match_the_documented_letters() {
        let hints: Vec<_> = Tool::ALL.iter().map(|t| t.shortcut_hint()).collect();
        assert_eq!(
            hints,
            vec![
                Some("V"),
                Some("H"),
                Some("N"),
                Some("T"),
                Some("S"),
                Some("P"),
                Some("E"),
                Some("C"),
                Some("F"),
                // Table, Chart, Mind map, Kanban, Image: Miro binds no bare key to any
                // of them, and every letter that would suit one is already a tool — `M`
                // would suit a mind map and Miro spends it on its own *Comment*, which
                // is cut here, so taking it would be inventing a binding rather than
                // matching one. `K` is free but Miro does not use it either.
                None,
                None,
                None,
                None,
                None,
            ]
        );
        let mut keys: Vec<_> = Tool::ALL.iter().filter_map(|t| t.shortcut()).collect();
        let count = keys.len();
        keys.sort_by_key(|k| k.name());
        keys.dedup();
        assert_eq!(keys.len(), count);
    }

    /// Every bare key the board screen claims lives in one namespace: a tool key, a
    /// tool's alternate, or a shape key. Two of them colliding means one silently
    /// never fires, and the collision is invisible until someone presses it.
    #[test]
    fn no_bare_key_is_claimed_twice_across_tools_and_shapes() {
        let mut claimed: Vec<Key> = Vec::new();
        for tool in Tool::ALL {
            claimed.extend(tool.shortcut());
            claimed.extend(tool.alternate_shortcut());
        }
        claimed.extend(SHAPE_SHORTCUTS.iter().map(|(key, _)| *key));

        let count = claimed.len();
        claimed.sort_by_key(|k| k.name());
        claimed.dedup();
        assert_eq!(claimed.len(), count, "a bare key is claimed twice");
    }

    /// `docs/04-ui-reference.md` §2's three flyout keys, all reachable.
    #[test]
    fn miros_three_shape_keys_all_land_somewhere() {
        assert_eq!(shape_shortcut(Key::R), Some(Shape::Rectangle));
        assert_eq!(shape_shortcut(Key::O), Some(Shape::Ellipse));
        assert_eq!(shape_shortcut(Key::Q), None);
        assert_eq!(Tool::Connector.alternate_shortcut(), Some(Key::L), "Line");

        // …and both shapes are really in the catalogue the picker draws from, so the
        // key cannot arm a shape the flyout has no tile for.
        for (_, shape) in SHAPE_SHORTCUTS {
            assert!(vellum_shapes::CATALOGUE.contains(&shape), "{shape:?} is not catalogued");
        }
    }

    /// The tool letters `docs/features/README.md` §9 commits to transferring.
    #[test]
    fn the_seven_documented_letters_reach_the_tools_miro_binds_them_to() {
        let of = |key: Key| Tool::ALL.iter().copied().find(|t| t.shortcut() == Some(key));
        assert_eq!(of(Key::V), Some(Tool::Select));
        assert_eq!(of(Key::N), Some(Tool::Sticky));
        assert_eq!(of(Key::T), Some(Tool::Text));
        assert_eq!(of(Key::S), Some(Tool::Shape));
        assert_eq!(of(Key::P), Some(Tool::Pen));
        assert_eq!(of(Key::C), Some(Tool::Connector));
        assert_eq!(of(Key::F), Some(Tool::Frame));
        assert_eq!(of(Key::H), Some(Tool::Hand));
    }

    /// Exactly the tools that have something to choose open a picker. A tool with a flyout
    /// draws a chevron and swallows a second click to toggle it, so an accidental arm here is
    /// a tool that stops feeling like a button.
    ///
    /// The eraser joined when its mode stopped being a bare ⇧ modifier; the **sticky** joined
    /// when the user asked for Miro's colour pack before placing one, rather than placing a
    /// yellow note and recolouring it afterwards.
    ///
    /// The negative half is the assertion: text, frame and connector have exactly one thing
    /// each to do, and a chevron on one of them would promise a choice that does not exist.
    #[test]
    fn only_the_tools_with_something_to_choose_open_a_flyout() {
        let with_flyout: Vec<_> =
            Tool::ALL.iter().copied().filter(|t| t.flyout().is_some()).collect();
        assert_eq!(with_flyout, vec![Tool::Sticky, Tool::Shape, Tool::Pen, Tool::Eraser]);
    }

    #[test]
    fn tool_icons_are_all_distinct() {
        let mut icons: Vec<_> = Tool::ALL.iter().map(|t| t.icon()).collect();
        icons.sort_by_key(|i| format!("{i:?}"));
        icons.dedup();
        assert_eq!(icons.len(), Tool::ALL.len());
    }

    #[test]
    fn the_two_shape_groups_partition_the_catalogue() {
        let basic = ShapeGroup::Basic.shapes().count();
        let flowchart = ShapeGroup::Flowchart.shapes().count();
        assert_eq!(basic + flowchart, vellum_shapes::CATALOGUE.len());
        assert!(basic > 0 && flowchart > 0);
    }

    /// A fallback label would be a silent regression: the picker would keep working
    /// while showing a shape called `RegularPolygon { sides: 7 }`.
    #[test]
    fn every_catalogue_shape_has_a_label_that_is_not_its_debug_spelling() {
        for shape in vellum_shapes::CATALOGUE {
            let label = shape_label(*shape);
            assert!(!label.contains('{'), "{shape:?} has no label");
            assert!(label.chars().next().is_some_and(char::is_uppercase), "{label}");
        }
    }

    /// Each category keeps its own colours, and the index is what makes that a plain
    /// array rather than a map. A group whose index escapes `ALL` would silently share
    /// another group's colours.
    #[test]
    fn every_shape_group_indexes_itself_inside_the_list() {
        for (expected, group) in ShapeGroup::ALL.into_iter().enumerate() {
            assert_eq!(group.index(), expected, "{group:?}");
        }
    }

    /// A shape placed before the user touches *Apply colors* matches a sticky placed
    /// the same way, rather than introducing a third default.
    #[test]
    fn the_default_shape_colours_are_the_boards_own() {
        let colors = ShapeColors::default();
        assert_eq!(colors.fill, Some(crate::color::DEFAULT_FILL));
        assert_eq!(colors.stroke, crate::color::INK);
    }

    /// ⇧ inverts the eraser's mode rather than naming one of them.
    ///
    /// It used to *be* the object eraser — the only way in — so `Object` had no way to be
    /// the resting state and ⇧ had no meaning once it was. Inverting keeps the modifier
    /// useful in both modes: whichever eraser is selected, ⇧ reaches the other one for the
    /// length of one sweep, which is what ⇧ does everywhere else in the app.
    #[test]
    fn shift_inverts_the_eraser_rather_than_naming_a_mode() {
        assert_eq!(EraserMode::Stroke.with_shift(false), EraserMode::Stroke);
        assert_eq!(EraserMode::Stroke.with_shift(true), EraserMode::Object);
        assert_eq!(EraserMode::Object.with_shift(false), EraserMode::Object);
        assert_eq!(
            EraserMode::Object.with_shift(true),
            EraserMode::Stroke,
            "the object eraser's ⇧ has to reach the stroke eraser, or it means nothing"
        );
        assert_eq!(EraserMode::default(), EraserMode::Stroke, "Miro's default, and ours");
    }

    #[test]
    fn switching_pen_kind_re_derives_the_width_and_the_highlighter_is_translucent() {
        let pen = PenPreset::default();
        assert_eq!(pen.width, PenKind::Pen.default_width());
        assert_eq!(pen.stroke_color().a, 0xFF);

        let highlighter = pen.with_kind(PenKind::Highlighter);
        assert_eq!(highlighter.width, PenKind::Highlighter.default_width());
        assert!(highlighter.stroke_color().a < 0xFF);
        assert_eq!(highlighter.color.a, pen.color.a, "the chosen colour is not mutated");
    }
}
