//! Text as styled spans, backed by a Loro rich-text container.
//!
//! Stickies and text widgets store spans rather than a `String` from day one, and
//! that is a deliberate cost paid early. Miro's `sticker` and `text` widgets carry
//! rich-text HTML — `<strong>`, `<em>`, `<a href>`, colour spans — so a faithful
//! import needs somewhere to put that on the very first day the importer runs.
//! Adding it later would mean migrating every board file that already exists.
//!
//! Rendering rich runs is cheap; *editing* them is the expensive part. So the
//! model is complete now and the editor catches up later: [`Board::set_text`] takes
//! a whole [`StyledText`], which is all a plain-text editor needs, while the
//! underlying [`loro::LoroText`] is already a collaborative rich-text CRDT ready for
//! character-level editing and multiplayer.
//!
//! [`Board::set_text`]: crate::Board::set_text

use crate::geometry::Color;
use loro::{ExpandType, LoroText, LoroValue, StyleConfig, StyleConfigMap, TextDelta};

/// Mark keys written into the Loro text container.
///
/// These names *are* the file format. Loro rejects a mark whose key has no
/// configured expand behaviour, so every key here must also appear in
/// [`style_config`] — the two lists are kept adjacent for that reason.
mod mark {
    pub const BOLD: &str = "bold";
    pub const ITALIC: &str = "italic";
    pub const UNDERLINE: &str = "underline";
    pub const STRIKE: &str = "strike";
    pub const LINK: &str = "link";
    pub const COLOR: &str = "color";

    /// Block-level keys. Quill — and therefore Miro's `structured_document` — hangs
    /// these off the newline that *terminates* a block. Vellum marks the whole line
    /// including that newline instead: the delta then carries a block's attributes
    /// on every character of it, so reading one back is a scan rather than a hunt
    /// for delimiters, and a block whose text is edited cannot lose its attributes
    /// by having its terminator replaced.
    pub const HEADING: &str = "heading";
    pub const LIST: &str = "list";
    pub const INDENT: &str = "indent";
    pub const CHECKED: &str = "checked";

    /// Character marks that carry on when you keep typing.
    pub const CONTINUING: [&str; 5] = [BOLD, ITALIC, UNDERLINE, STRIKE, COLOR];
    pub const BLOCK: [&str; 4] = [HEADING, LIST, INDENT, CHECKED];
}

/// The mark configuration a Vellum document is read and written with.
///
/// Loro's own `default_rich_text_config` is close but not sufficient: it has no
/// `strike` and no `color`, and marking with an unconfigured key is a hard error.
/// Declaring the whole set here also means the expand behaviour is versioned with
/// the format instead of inherited from whatever Loro's defaults happen to be.
///
/// *Expand* decides what happens to text typed at a mark's boundary. Character
/// formatting continues when you keep typing (`After`); a link does not, because
/// typing after a URL should not silently extend the link target.
///
/// Block marks expand **backwards** (`Before`), which is the only choice that makes
/// a line boundary unambiguous. A block's mark runs through its own newline, so the
/// position where one line ends is exactly the position where the next begins; with
/// `After` both lines would claim text typed there. With `Before` only the following
/// line does — and text typed at the start of a line does belong to that line.
pub(crate) fn style_config() -> StyleConfigMap {
    let mut map = StyleConfigMap::new();
    for key in mark::CONTINUING {
        map.insert(key.into(), StyleConfig { expand: ExpandType::After });
    }
    map.insert(mark::LINK.into(), StyleConfig { expand: ExpandType::None });
    for key in mark::BLOCK {
        map.insert(key.into(), StyleConfig { expand: ExpandType::Before });
    }
    map
}

/// Character-level formatting. Everything here can vary *within* one text run.
///
/// Font family, size, alignment and line height are deliberately absent: Miro
/// carries them as widget-level style keys (`ffn`, `fs`, `ta`, `lh`), and so does
/// Vellum — see [`Style`](crate::Style). Duplicating them per span would create two
/// sources of truth for the same property.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// Target of a hyperlink covering this span.
    pub link: Option<String>,
    /// Overrides the item's text colour for this span.
    pub color: Option<Color>,
}

impl SpanStyle {
    /// Unformatted text.
    pub fn plain() -> Self {
        Self::default()
    }

    pub fn bold() -> Self {
        Self { bold: true, ..Self::default() }
    }

    pub fn link(url: impl Into<String>) -> Self {
        Self { link: Some(url.into()), ..Self::default() }
    }

    /// True when nothing needs to be written to the document for this span.
    pub fn is_plain(&self) -> bool {
        self == &Self::default()
    }
}

/// Which kind of list a block belongs to.
///
/// Miro's `structured_document` carries Quill's `list` attribute, whose values are
/// `"bullet"`, `"ordered"` and `"checked"`/`"unchecked"` — the tick state is folded
/// into the list kind there and split out into [`BlockStyle::checked`] here, so that
/// ticking a box is one field changing rather than the block changing type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListKind {
    Bulleted,
    Numbered,
    Checklist,
}

impl ListKind {
    /// The token stored in the document, spelled out rather than borrowing Quill's
    /// vocabulary: this is Vellum's format, and a readable document debugs faster
    /// than a compact one.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Bulleted => "bulleted",
            Self::Numbered => "numbered",
            Self::Checklist => "checklist",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "bulleted" => Self::Bulleted,
            "numbered" => Self::Numbered,
            "checklist" => Self::Checklist,
            _ => return None,
        })
    }
}

/// Formatting that belongs to a whole line rather than to a run of characters.
///
/// The split mirrors the one between [`SpanStyle`] and [`Style`](crate::Style): a
/// heading level or a bullet cannot vary within a line, so modelling it per span
/// would allow documents that describe half a line as a heading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BlockStyle {
    /// Heading level, Quill's `{header: n}`. Levels run 1–6; anything else is not
    /// representable and reads back as body text.
    pub heading: Option<u8>,
    pub list: Option<ListKind>,
    /// Nesting depth of a list item — Quill's `indent`. `0` is top level.
    pub indent: u8,
    /// Whether a [`ListKind::Checklist`] item is ticked. Meaningless on any other
    /// block, and preserved rather than cleared so that toggling a list back and
    /// forth does not lose which boxes were checked.
    pub checked: bool,
}

/// Heading levels Quill, Miro and HTML all agree on.
const HEADING_LEVELS: std::ops::RangeInclusive<i64> = 1..=6;

impl BlockStyle {
    /// Body text — no heading, no list.
    pub fn plain() -> Self {
        Self::default()
    }

    pub fn heading(level: u8) -> Self {
        Self { heading: Some(level), ..Self::default() }
    }

    pub fn list(kind: ListKind) -> Self {
        Self { list: Some(kind), ..Self::default() }
    }

    /// True when nothing needs to be written to the document for this block.
    pub fn is_plain(&self) -> bool {
        self == &Self::default()
    }
}

/// A run of text sharing one [`SpanStyle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSpan {
    pub text: String,
    pub style: SpanStyle,
}

impl TextSpan {
    pub fn new(text: impl Into<String>, style: SpanStyle) -> Self {
        Self { text: text.into(), style }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: SpanStyle::default() }
    }
}

/// A styled text value: an ordered, normalised list of spans, plus the block
/// structure of the lines they span.
///
/// Normalisation (empty spans dropped, adjacent same-style spans merged, trailing
/// plain blocks trimmed) happens on construction so that two `StyledText`s
/// representing the same visible text are always `==`. Without it, a save/load
/// round-trip could produce a value that looks identical but compares unequal, which
/// would make every round-trip test a lie.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledText {
    spans: Vec<TextSpan>,
    /// Block style per line, entry *i* applying to line *i*. Lines past the end are
    /// [`BlockStyle::default`], so plain prose — which is nearly all board text —
    /// carries an empty vector and costs nothing.
    blocks: Vec<BlockStyle>,
}

impl StyledText {
    /// Unformatted text — the common case, since nearly all board text carries no runs.
    pub fn plain(text: impl Into<String>) -> Self {
        Self::from_spans([TextSpan::plain(text)])
    }

    pub fn from_spans(spans: impl IntoIterator<Item = TextSpan>) -> Self {
        let mut out: Vec<TextSpan> = Vec::new();
        for span in spans {
            if span.text.is_empty() {
                continue;
            }
            match out.last_mut() {
                Some(prev) if prev.style == span.style => prev.text.push_str(&span.text),
                _ => out.push(span),
            }
        }
        Self { spans: out, blocks: Vec::new() }
    }

    /// Attaches block structure, one entry per line from the top.
    ///
    /// Entries past the last line are dropped and trailing plain ones are trimmed,
    /// so `with_blocks` is idempotent and two values that describe the same document
    /// compare equal however the caller padded them.
    pub fn with_blocks(mut self, blocks: impl IntoIterator<Item = BlockStyle>) -> Self {
        let mut blocks: Vec<_> = blocks.into_iter().take(self.line_count()).collect();
        while blocks.last().is_some_and(BlockStyle::is_plain) {
            blocks.pop();
        }
        self.blocks = blocks;
        self
    }

    pub fn spans(&self) -> &[TextSpan] {
        &self.spans
    }

    /// Block styles from the first line, truncated at the last non-plain one. Use
    /// [`StyledText::block`] to ask about a specific line without bounds-checking.
    pub fn blocks(&self) -> &[BlockStyle] {
        &self.blocks
    }

    /// The block style of line `line`, counting from zero. Lines with no stored
    /// entry — including every line of plain prose — are body text.
    pub fn block(&self, line: usize) -> BlockStyle {
        self.blocks.get(line).copied().unwrap_or_default()
    }

    /// True when at least one line carries block structure. The renderer uses it to
    /// skip list and heading layout entirely for the common case.
    pub fn has_block_structure(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// Lines, counting a trailing newline as opening an empty last line. Empty text
    /// has no lines at all, which keeps [`StyledText::with_blocks`] from inventing
    /// a block for content that does not exist.
    pub fn line_count(&self) -> usize {
        if self.spans.is_empty() {
            return 0;
        }
        1 + self.spans.iter().map(|s| s.text.matches('\n').count()).sum::<usize>()
    }

    /// The text with all formatting dropped — what a plain-text editor and a
    /// search index both want.
    pub fn to_plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Length in Unicode scalar values, which is the unit Loro indexes text by.
    pub fn char_len(&self) -> usize {
        self.spans.iter().map(|s| s.text.chars().count()).sum()
    }
}

impl From<&str> for StyledText {
    fn from(s: &str) -> Self {
        Self::plain(s)
    }
}

impl From<String> for StyledText {
    fn from(s: String) -> Self {
        Self::plain(s)
    }
}

/// Replaces the container's contents with `styled`.
///
/// Whole-value replacement, not a diff. The caret does produce styled values now — it
/// splices each keystroke into the item's own runs rather than flattening them — but it
/// hands over a whole `StyledText` each time, so there is still no character-level edit
/// here to preserve. The container itself is reused rather than recreated so the document
/// does not accumulate an orphaned text container per edit.
pub(crate) fn write(target: &LoroText, styled: &StyledText) -> loro::LoroResult<()> {
    let existing = target.len_unicode();
    if existing > 0 {
        target.delete(0, existing)?;
    }
    if styled.is_empty() {
        return Ok(());
    }

    let plain = styled.to_plain();
    target.insert(0, &plain)?;

    let mut start = 0usize;
    for span in &styled.spans {
        let end = start + span.text.chars().count();
        // Loro rejects an empty mark range, and a plain span has nothing to record.
        if !span.style.is_plain() {
            apply_marks(target, start..end, &span.style)?;
        }
        start = end;
    }
    apply_block_marks(target, styled, &plain)
}

/// Marks each line's characters, its own newline included, with its block style.
///
/// A block style on a *trailing empty line* — text ending in `\n` — is the one thing
/// that cannot be stored: the line has no characters, and Loro rejects an empty mark
/// range. It is also invisible on the canvas, and normalisation trims it, so the
/// only way to hit it is to ask for a bulleted line with nothing in it.
fn apply_block_marks(
    target: &LoroText,
    styled: &StyledText,
    plain: &str,
) -> loro::LoroResult<()> {
    if !styled.has_block_structure() {
        return Ok(());
    }

    let (mut line_start, mut line) = (0usize, 0usize);
    let mut at = 0usize;
    for ch in plain.chars() {
        at += 1;
        if ch == '\n' {
            mark_block(target, line_start..at, styled.block(line))?;
            line_start = at;
            line += 1;
        }
    }
    if at > line_start {
        mark_block(target, line_start..at, styled.block(line))?;
    }
    Ok(())
}

fn mark_block(
    target: &LoroText,
    range: std::ops::Range<usize>,
    block: BlockStyle,
) -> loro::LoroResult<()> {
    if let Some(level) = block.heading.filter(|l| HEADING_LEVELS.contains(&i64::from(*l))) {
        target.mark(range.clone(), mark::HEADING, i64::from(level))?;
    }
    if let Some(kind) = block.list {
        target.mark(range.clone(), mark::LIST, kind.tag())?;
    }
    if block.indent > 0 {
        target.mark(range.clone(), mark::INDENT, i64::from(block.indent))?;
    }
    if block.checked {
        target.mark(range, mark::CHECKED, true)?;
    }
    Ok(())
}

fn apply_marks(
    target: &LoroText,
    range: std::ops::Range<usize>,
    style: &SpanStyle,
) -> loro::LoroResult<()> {
    for (key, on) in [
        (mark::BOLD, style.bold),
        (mark::ITALIC, style.italic),
        (mark::UNDERLINE, style.underline),
        (mark::STRIKE, style.strikethrough),
    ] {
        if on {
            target.mark(range.clone(), key, true)?;
        }
    }
    if let Some(url) = &style.link {
        target.mark(range.clone(), mark::LINK, url.as_str())?;
    }
    if let Some(color) = style.color {
        target.mark(range, mark::COLOR, color.to_packed())?;
    }
    Ok(())
}

/// Reads a Loro text container back into spans and block structure.
///
/// Loro's delta is already span-shaped, so the character half is attribute
/// translation. The block half is a scan: the delta merges neighbouring runs that
/// share attributes, so a chunk can cover several lines at once and the line index
/// has to be tracked across it rather than read off a chunk boundary.
///
/// Unknown attribute keys are ignored rather than rejected: a board written by a
/// newer build must still open, minus the formatting this build cannot render.
pub(crate) fn read(source: &LoroText) -> StyledText {
    let mut spans = Vec::new();
    let mut blocks: Vec<BlockStyle> = Vec::new();
    let mut line = 0usize;

    for delta in source.to_delta() {
        // `to_delta` on a whole container only ever yields inserts; retains and
        // deletes belong to change events.
        let TextDelta::Insert { insert, attributes } = delta else { continue };

        let mut style = SpanStyle::default();
        let mut block = BlockStyle::default();
        for (key, value) in attributes.iter().flat_map(|a| a.iter()) {
            match key.as_str() {
                mark::BOLD => style.bold = is_set(value),
                mark::ITALIC => style.italic = is_set(value),
                mark::UNDERLINE => style.underline = is_set(value),
                mark::STRIKE => style.strikethrough = is_set(value),
                mark::LINK => {
                    style.link = value.as_string().map(|s| s.as_str().to_owned());
                }
                mark::COLOR => {
                    style.color = value.as_i64().copied().and_then(Color::from_packed);
                }
                mark::HEADING => {
                    block.heading = value
                        .as_i64()
                        .copied()
                        .filter(|l| HEADING_LEVELS.contains(l))
                        .map(|l| l as u8);
                }
                mark::LIST => {
                    block.list = value.as_string().and_then(|s| ListKind::from_tag(s.as_str()));
                }
                mark::INDENT => {
                    block.indent = value.as_i64().copied().unwrap_or(0).clamp(0, 255) as u8;
                }
                mark::CHECKED => block.checked = is_set(value),
                _ => {}
            }
        }

        record_blocks(&mut blocks, &mut line, &insert, block);
        spans.push(TextSpan { text: insert, style });
    }

    StyledText::from_spans(spans).with_blocks(blocks)
}

/// Assigns `block` to every line this chunk contributes characters to, and advances
/// `line` past the newlines it contains.
///
/// A character belongs to the line holding the newlines *before* it, so a chunk
/// ending in `\n` contributes nothing to the line that newline opens — `"a\n"`
/// covers one line while `"a\nb"` covers two.
fn record_blocks(blocks: &mut Vec<BlockStyle>, line: &mut usize, chunk: &str, block: BlockStyle) {
    let breaks = chunk.matches('\n').count();
    let last = *line + breaks - usize::from(chunk.ends_with('\n'));
    if blocks.len() <= last {
        blocks.resize(last + 1, BlockStyle::default());
    }
    blocks[*line..=last].fill(block);
    *line += breaks;
}

/// A boolean mark is "on" only when explicitly true. Unmarking leaves the key
/// behind with a `null`/`false` value, which must read as absent.
fn is_set(value: &LoroValue) -> bool {
    matches!(value, LoroValue::Bool(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro::LoroDoc;

    fn doc_with_text() -> (LoroDoc, LoroText) {
        let doc = LoroDoc::new();
        doc.config_text_style(style_config());
        let text = doc.get_text("t");
        (doc, text)
    }

    /// Normalisation is what makes `StyledText` equality meaningful, so it is
    /// tested directly rather than only through a round-trip.
    #[test]
    fn adjacent_spans_with_equal_style_merge() {
        let t = StyledText::from_spans([
            TextSpan::plain("Hello, "),
            TextSpan::plain("world"),
            TextSpan::new("!", SpanStyle::bold()),
        ]);
        assert_eq!(t.spans().len(), 2);
        assert_eq!(t.spans()[0].text, "Hello, world");
        assert_eq!(t.to_plain(), "Hello, world!");
    }

    #[test]
    fn empty_spans_are_dropped() {
        let t = StyledText::from_spans([
            TextSpan::plain(""),
            TextSpan::new("", SpanStyle::bold()),
            TextSpan::plain("x"),
        ]);
        assert_eq!(t.spans().len(), 1);
        assert!(!t.is_empty());
        assert!(StyledText::plain("").is_empty());
    }

    /// Loro indexes text by Unicode scalar value, so mark ranges are computed in
    /// chars. Getting this wrong only shows up on non-ASCII text.
    #[test]
    fn char_len_counts_scalars_not_bytes() {
        let t = StyledText::plain("héllo wörld");
        assert_eq!(t.char_len(), 11);
        assert_eq!(t.to_plain().len(), 13);
    }

    #[test]
    fn round_trips_through_loro_with_every_mark() {
        let (_doc, target) = doc_with_text();
        let original = StyledText::from_spans([
            TextSpan::new("bold", SpanStyle::bold()),
            TextSpan::plain(" plain "),
            TextSpan::new(
                "fancy",
                SpanStyle {
                    italic: true,
                    underline: true,
                    strikethrough: true,
                    color: Some(Color::rgb(0xFF, 0xF7, 0x9E)),
                    ..SpanStyle::default()
                },
            ),
            TextSpan::new(" link", SpanStyle::link("https://vellum.app")),
        ]);

        write(&target, &original).unwrap();
        assert_eq!(read(&target), original);
    }

    /// The offsets used for marks must be char-based end to end, so a multibyte
    /// prefix must not shift the styling of what follows it.
    #[test]
    fn marks_land_on_the_right_characters_after_multibyte_text() {
        let (_doc, target) = doc_with_text();
        let original = StyledText::from_spans([
            TextSpan::plain("héllö "),
            TextSpan::new("wörld", SpanStyle::bold()),
        ]);
        write(&target, &original).unwrap();

        let back = read(&target);
        assert_eq!(back, original);
        assert_eq!(back.spans()[1].text, "wörld");
        assert!(back.spans()[1].style.bold);
    }

    /// A second write must not leave the previous run's formatting behind, which
    /// is the failure mode of clearing text without clearing its marks.
    #[test]
    fn rewriting_clears_the_previous_styling() {
        let (_doc, target) = doc_with_text();
        write(&target, &StyledText::from_spans([TextSpan::new("aaa", SpanStyle::bold())])).unwrap();
        write(&target, &StyledText::plain("bbb")).unwrap();

        let back = read(&target);
        assert_eq!(back, StyledText::plain("bbb"));
        assert!(!back.spans()[0].style.bold);
    }

    #[test]
    fn empty_text_round_trips_as_empty() {
        let (_doc, target) = doc_with_text();
        write(&target, &StyledText::plain("something")).unwrap();
        write(&target, &StyledText::default()).unwrap();
        assert!(read(&target).is_empty());
    }

    /// Every key in [`mark`] must be configured, or writing it fails at runtime.
    /// This asserts the two lists have not drifted apart.
    #[test]
    fn every_mark_key_is_configured() {
        let (_doc, target) = doc_with_text();
        target.insert(0, "xx").unwrap();
        for key in mark::CONTINUING.iter().chain(&[mark::LINK]).chain(&mark::BLOCK) {
            target.mark(0..2, key, true).unwrap_or_else(|e| panic!("mark {key:?}: {e}"));
        }
    }

    // ----- block structure ------------------------------------------------

    #[test]
    fn list_kind_tags_round_trip() {
        for kind in [ListKind::Bulleted, ListKind::Numbered, ListKind::Checklist] {
            assert_eq!(ListKind::from_tag(kind.tag()), Some(kind));
        }
        assert_eq!(ListKind::from_tag("bullet"), None);
    }

    #[test]
    fn lines_count_a_trailing_newline_as_opening_an_empty_one() {
        assert_eq!(StyledText::default().line_count(), 0);
        assert_eq!(StyledText::plain("one").line_count(), 1);
        assert_eq!(StyledText::plain("one\ntwo").line_count(), 2);
        assert_eq!(StyledText::plain("one\n").line_count(), 2);
        assert_eq!(StyledText::plain("\n\n").line_count(), 3);
    }

    /// Normalisation is what makes block structure comparable: padding a value with
    /// plain blocks, or with blocks for lines that do not exist, must not change it.
    #[test]
    fn block_lists_are_trimmed_and_truncated_on_construction() {
        let heading = BlockStyle::heading(1);
        let text = StyledText::plain("title\nbody");

        let padded = text.clone().with_blocks([heading, BlockStyle::plain(), BlockStyle::plain()]);
        assert_eq!(padded.blocks(), &[heading]);
        assert_eq!(padded, text.clone().with_blocks([heading]));

        // Blocks past the last line are dropped rather than kept as phantom lines.
        let overflowing =
            text.clone().with_blocks([BlockStyle::plain(), BlockStyle::plain(), heading]);
        assert!(!overflowing.has_block_structure());

        assert_eq!(padded.block(0), heading);
        assert_eq!(padded.block(1), BlockStyle::default());
        assert_eq!(padded.block(99), BlockStyle::default(), "past the end is body text");
    }

    /// A whole structured document: a heading, two bullets, a nested checklist and
    /// prose, with character formatting crossing the block boundaries.
    #[test]
    fn block_structure_round_trips_through_loro() {
        let (_doc, target) = doc_with_text();
        let original = StyledText::from_spans([
            TextSpan::new("Cooling", SpanStyle::bold()),
            TextSpan::plain("\nfan\nradiator\n"),
            TextSpan::new("bleed the system", SpanStyle::link("https://vellum.app")),
            TextSpan::plain("\nplain prose"),
        ])
        .with_blocks([
            BlockStyle::heading(2),
            BlockStyle::list(ListKind::Bulleted),
            BlockStyle::list(ListKind::Bulleted),
            BlockStyle { list: Some(ListKind::Checklist), indent: 1, checked: true, heading: None },
            BlockStyle::plain(),
        ]);

        write(&target, &original).unwrap();
        let back = read(&target);

        assert_eq!(back, original);
        assert_eq!(back.to_plain(), original.to_plain());
        assert_eq!(back.block(0).heading, Some(2));
        assert_eq!(back.block(2).list, Some(ListKind::Bulleted));
        assert!(back.block(3).checked);
        assert_eq!(back.block(3).indent, 1);
        assert!(!back.block(4).checked, "prose picked up the checklist's tick");
    }

    /// Block marks run through each line's own newline, so they must not leak into
    /// the next line — the failure that would make a one-line heading style a whole
    /// document.
    #[test]
    fn a_block_style_stops_at_its_own_line() {
        let (_doc, target) = doc_with_text();
        let original = StyledText::plain("heading\nbody\nmore body")
            .with_blocks([BlockStyle::heading(1)]);

        write(&target, &original).unwrap();
        let back = read(&target);

        assert_eq!(back.blocks(), &[BlockStyle::heading(1)]);
        assert_eq!(back.block(1), BlockStyle::default());
        assert_eq!(back.block(2), BlockStyle::default());
    }

    /// The delta merges neighbouring runs that share attributes, so two consecutive
    /// bullets arrive as one chunk covering two lines. Reading has to count the
    /// newlines inside a chunk rather than trusting chunk boundaries to be lines.
    #[test]
    fn one_delta_chunk_spanning_several_lines_still_yields_one_block_each() {
        let (_doc, target) = doc_with_text();
        let bullets = BlockStyle::list(ListKind::Numbered);
        let original = StyledText::plain("first\nsecond\nthird")
            .with_blocks([bullets, bullets, bullets]);

        write(&target, &original).unwrap();
        // One chunk, because every character carries the same attributes.
        assert_eq!(target.to_delta().len(), 1);
        assert_eq!(read(&target).blocks(), &[bullets, bullets, bullets]);
    }

    /// Character and block formatting live in the same container and must not
    /// interfere: a bold word inside a heading stays bold, and the heading does not
    /// become bold because one of its words is.
    #[test]
    fn character_marks_survive_inside_a_styled_block() {
        let (_doc, target) = doc_with_text();
        let original = StyledText::from_spans([
            TextSpan::plain("check the "),
            TextSpan::new("fan", SpanStyle::bold()),
            TextSpan::plain(" belt"),
        ])
        .with_blocks([BlockStyle::list(ListKind::Checklist)]);

        write(&target, &original).unwrap();
        let back = read(&target);

        assert_eq!(back, original);
        assert_eq!(back.spans().len(), 3);
        assert!(back.spans()[1].style.bold);
        assert!(!back.spans()[0].style.bold);
    }

    /// Rewriting must clear block structure as thoroughly as it clears marks, or a
    /// bulleted note edited into prose stays bulleted.
    #[test]
    fn rewriting_clears_the_previous_block_structure() {
        let (_doc, target) = doc_with_text();
        write(
            &target,
            &StyledText::plain("was a heading").with_blocks([BlockStyle::heading(3)]),
        )
        .unwrap();
        write(&target, &StyledText::plain("now prose")).unwrap();

        let back = read(&target);
        assert!(!back.has_block_structure(), "{:?}", back.blocks());
        assert_eq!(back, StyledText::plain("now prose"));
    }

    /// Quill, Miro and HTML all stop at `h6`. A level outside that has nothing to
    /// render as, so it reads back as body text rather than as a heading nobody can
    /// draw — the same choice [`Color::from_packed`] makes for an out-of-range int.
    #[test]
    fn a_heading_level_outside_one_to_six_reads_as_body_text() {
        let (_doc, target) = doc_with_text();
        for level in [0u8, 7, 200] {
            write(&target, &StyledText::plain("x").with_blocks([BlockStyle::heading(level)]))
                .unwrap();
            assert_eq!(read(&target).block(0).heading, None, "level {level}");
        }
        write(&target, &StyledText::plain("x").with_blocks([BlockStyle::heading(6)])).unwrap();
        assert_eq!(read(&target).block(0).heading, Some(6));
    }

    /// The one thing block structure cannot store, pinned so it is a known gap
    /// rather than a surprise: a trailing empty line has no characters to mark.
    #[test]
    fn a_block_style_on_a_trailing_empty_line_is_not_preserved() {
        let (_doc, target) = doc_with_text();
        let original = StyledText::plain("item\n")
            .with_blocks([BlockStyle::list(ListKind::Bulleted), BlockStyle::heading(1)]);
        assert_eq!(original.blocks().len(), 2);

        write(&target, &original).unwrap();
        let back = read(&target);
        assert_eq!(back.blocks(), &[BlockStyle::list(ListKind::Bulleted)]);
    }

    #[test]
    fn plain_text_carries_no_block_structure() {
        let (_doc, target) = doc_with_text();
        write(&target, &StyledText::plain("just prose\nover two lines")).unwrap();
        let back = read(&target);
        assert!(!back.has_block_structure());
        assert!(back.blocks().is_empty());
    }

    /// Formatting this build does not understand must survive as unstyled text
    /// rather than failing the read.
    #[test]
    fn unknown_marks_are_ignored_not_fatal() {
        let doc = LoroDoc::new();
        let mut config = style_config();
        config.insert("vellum-future".into(), StyleConfig { expand: ExpandType::After });
        doc.config_text_style(config);
        let target = doc.get_text("t");
        target.insert(0, "abc").unwrap();
        target.mark(0..3, "vellum-future", true).unwrap();

        assert_eq!(read(&target), StyledText::plain("abc"));
    }
}
