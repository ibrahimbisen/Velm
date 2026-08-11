//! The legend: the dependable half of a chart's identity channel.
//!
//! A legend is present whenever there are two or more series, and absent for one.
//! That is not a stylistic default — colour alone must never be the only way to tell
//! two series apart, so the moment a chart has a second series it owes the reader a
//! key. A one-series chart owes nothing: there is one colour, and the chart's title
//! already says what it is. A box with a single swatch in it restates the title and
//! spends space to do it.
//!
//! Entries are laid out in **series order**, never sorted by value. A legend that
//! reorders itself when the data changes forces the reader to re-learn it every time
//! they look, and it is the same mistake as recolouring on filter: identity has to
//! be stable to be identity.
//!
//! What does not fit is dropped and counted, in [`Legend::omitted`]. A legend that
//! overflows its band and draws over the plot is worse than one that admits it ran
//! out of room — and the count gives the caller something to act on.

use crate::colour::Colour;
use crate::geom::Rect;
use crate::label::{Label, TextAlign};
use crate::palette::Palette;
use crate::style::{ChartStyle, LegendPosition};
use crate::text::{TextMetrics, truncate_to_width};
use serde::{Deserialize, Serialize};

/// One key: a colour swatch and the series name beside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegendEntry {
    pub series: usize,
    /// A filled square in the series colour — a small mark, standing for a mark.
    pub swatch: Rect,
    pub label: Label,
    pub colour: Colour,
}

/// A laid-out legend, and the band of the frame it occupies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Legend {
    pub position: LegendPosition,
    /// The strip taken out of the chart frame. The plot gets what is left.
    pub bounds: Rect,
    pub entries: Vec<LegendEntry>,
    /// Series that had no room. See the module docs.
    pub omitted: usize,
}

/// A legend may not eat the chart. Past this fraction of the frame the plot stops
/// being the point, and the remaining entries are dropped instead.
const MAX_BAND_FRACTION: f32 = 0.4;

impl Legend {
    /// Lays out a legend inside `frame`, or returns `None` when there is nothing to
    /// key: fewer than two series, no room, or the caller asked for none.
    pub fn build(
        names: &[String],
        slots: &[usize],
        position: LegendPosition,
        frame: Rect,
        style: &ChartStyle,
        palette: &Palette,
        metrics: &dyn TextMetrics,
    ) -> Option<Self> {
        if position == LegendPosition::None || names.len() < 2 || frame.is_empty() {
            return None;
        }
        let size = style.text_size;
        let line = metrics.line_height(size);
        let row_height = line.max(style.legend_swatch);
        let lead = style.legend_swatch + style.text_gap;
        // Two entries never touch: the gap between them is wider than the gap
        // between a swatch and its own name, so a name binds to the swatch on its
        // left rather than floating between two.
        let entry_gap = style.text_gap * 3.0;

        match position {
            LegendPosition::None => None,
            LegendPosition::Left | LegendPosition::Right => Self::column(
                names, slots, position, frame, style, palette, metrics, row_height, lead,
            ),
            LegendPosition::Top | LegendPosition::Bottom => Self::rows(
                names, slots, position, frame, style, palette, metrics, row_height, lead, entry_gap,
            ),
        }
    }

    /// The name of series `index`, or a positional stand-in. An unnamed series still
    /// needs a key, or its colour means nothing.
    fn name_of(names: &[String], index: usize) -> String {
        match names.get(index) {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => format!("Series {}", index + 1),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rows(
        names: &[String],
        slots: &[usize],
        position: LegendPosition,
        frame: Rect,
        style: &ChartStyle,
        palette: &Palette,
        metrics: &dyn TextMetrics,
        row_height: f32,
        lead: f32,
        entry_gap: f32,
    ) -> Option<Self> {
        let size = style.text_size;
        let max_height = frame.height * MAX_BAND_FRACTION;
        // No forced minimum: a frame with room for less than one row gets no legend
        // at all, rather than a band taller than the chart it is keying.
        let max_rows = (max_height / row_height).floor() as usize;
        if max_rows == 0 {
            return None;
        }

        let mut entries = Vec::with_capacity(names.len());
        let mut omitted = 0;
        // `row` is where the *next* entry would go, which is one past the last used
        // row once the band fills up. The band's height follows `used_rows`, the
        // rows that actually carry an entry.
        let (mut x, mut row, mut used_rows) = (frame.x, 0_usize, 0_usize);

        for index in 0..names.len() {
            let budget = frame.width - lead;
            let Some(text) = truncate_to_width(&Self::name_of(names, index), size, budget, metrics)
            else {
                omitted += 1;
                continue;
            };
            let width = lead + metrics.width(&text, size);
            // Wrap when the entry would run past the frame, except at the start of a
            // row, where wrapping would loop forever on an entry wider than the band.
            if x > frame.x && x + width > frame.right() {
                row += 1;
                x = frame.x;
            }
            if row >= max_rows {
                omitted += names.len() - index;
                break;
            }
            let top = frame.y + row as f32 * row_height;
            entries.push(Self::entry(
                index, slot_of(slots, index), text, x, top, row_height, size, style, palette, metrics,
            ));
            used_rows = row + 1;
            x += width + entry_gap;
        }

        if entries.is_empty() {
            return None;
        }
        let height = used_rows as f32 * row_height;
        let y = match position {
            LegendPosition::Bottom => frame.bottom() - height,
            _ => frame.y,
        };
        // Rows were laid out from the frame's top; a bottom legend slides down.
        let shift = y - frame.y;
        for entry in &mut entries {
            entry.swatch = entry.swatch.translated(0.0, shift);
            entry.label.rect = entry.label.rect.translated(0.0, shift);
        }
        Some(Self {
            position,
            bounds: Rect::new(frame.x, y, frame.width, height),
            entries,
            omitted,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn column(
        names: &[String],
        slots: &[usize],
        position: LegendPosition,
        frame: Rect,
        style: &ChartStyle,
        palette: &Palette,
        metrics: &dyn TextMetrics,
        row_height: f32,
        lead: f32,
    ) -> Option<Self> {
        let size = style.text_size;
        let max_width = frame.width * MAX_BAND_FRACTION;
        let rows = (frame.height / row_height).floor() as usize;
        if rows == 0 {
            return None;
        }

        let mut texts = Vec::with_capacity(names.len());
        let mut omitted = names.len().saturating_sub(rows);
        for index in 0..names.len().min(rows) {
            match truncate_to_width(&Self::name_of(names, index), size, max_width - lead, metrics) {
                Some(text) => texts.push((index, text)),
                None => omitted += 1,
            }
        }
        if texts.is_empty() {
            return None;
        }
        let width = texts
            .iter()
            .map(|(_, text)| lead + metrics.width(text, size))
            .fold(0.0_f32, f32::max);
        let x = match position {
            LegendPosition::Right => frame.right() - width,
            _ => frame.x,
        };
        let entries = texts
            .into_iter()
            .enumerate()
            .map(|(row, (index, text))| {
                let top = frame.y + row as f32 * row_height;
                Self::entry(
                    index, slot_of(slots, index), text, x, top, row_height, size, style, palette,
                    metrics,
                )
            })
            .collect::<Vec<_>>();
        let height = entries.len() as f32 * row_height;
        Some(Self {
            position,
            bounds: Rect::new(x, frame.y, width, height),
            entries,
            omitted,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(
        index: usize,
        slot: usize,
        text: String,
        x: f32,
        top: f32,
        row_height: f32,
        size: f32,
        style: &ChartStyle,
        palette: &Palette,
        metrics: &dyn TextMetrics,
    ) -> LegendEntry {
        let swatch_size = style.legend_swatch;
        let swatch = Rect::new(
            x,
            top + (row_height - swatch_size) * 0.5,
            swatch_size,
            swatch_size,
        );
        let line = metrics.line_height(size);
        let label = Label::new(
            text.clone(),
            Rect::new(
                x + swatch_size + style.text_gap,
                top + (row_height - line) * 0.5,
                metrics.width(&text, size),
                line,
            ),
            TextAlign::Start,
            size,
            // The key's text is muted text, never the series colour: a pale hue is
            // illegible as type, and the swatch beside it already carries identity.
            palette.text_muted,
        );
        LegendEntry { series: index, swatch, label, colour: palette.series(slot) }
    }
}

/// The palette slot for series `index`: its pin, or its position. Kept here so the
/// legend and the marks cannot disagree about which colour a series wears.
fn slot_of(slots: &[usize], index: usize) -> usize {
    slots.get(index).copied().unwrap_or(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::Theme;
    use crate::text::MonoMetrics;

    fn names(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("Series {}", i + 1)).collect()
    }

    fn build(names: &[String], position: LegendPosition, frame: Rect) -> Option<Legend> {
        let slots: Vec<usize> = (0..names.len()).collect();
        Legend::build(
            names,
            &slots,
            position,
            frame,
            &ChartStyle::default(),
            &Palette::new(Theme::Light),
            &MonoMetrics::default(),
        )
    }

    #[test]
    fn one_series_gets_no_legend_because_the_title_already_names_it() {
        assert!(build(&names(1), LegendPosition::Top, Rect::new(0.0, 0.0, 400.0, 300.0)).is_none());
        assert!(build(&names(0), LegendPosition::Top, Rect::new(0.0, 0.0, 400.0, 300.0)).is_none());
    }

    #[test]
    fn two_series_always_get_one() {
        let legend = build(&names(2), LegendPosition::Top, Rect::new(0.0, 0.0, 400.0, 300.0)).unwrap();
        assert_eq!(legend.entries.len(), 2);
        assert_eq!(legend.omitted, 0);
    }

    #[test]
    fn entries_keep_series_order_and_series_colours() {
        let palette = Palette::new(Theme::Light);
        let legend = build(&names(4), LegendPosition::Top, Rect::new(0.0, 0.0, 600.0, 300.0)).unwrap();
        for (position, entry) in legend.entries.iter().enumerate() {
            assert_eq!(entry.series, position);
            assert_eq!(entry.colour, palette.series(position));
        }
    }

    #[test]
    fn entries_do_not_overlap_within_a_row_or_between_rows() {
        let legend = build(&names(8), LegendPosition::Top, Rect::new(0.0, 0.0, 300.0, 300.0)).unwrap();
        let boxes: Vec<Rect> = legend
            .entries
            .iter()
            .map(|e| Rect::from_edges(
                e.swatch.left(),
                e.swatch.top().min(e.label.rect.top()),
                e.label.rect.right(),
                e.swatch.bottom().max(e.label.rect.bottom()),
            ))
            .collect();
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_narrow_frame_wraps_into_rows_and_the_band_grows() {
        let wide = build(&names(6), LegendPosition::Top, Rect::new(0.0, 0.0, 900.0, 300.0)).unwrap();
        let narrow = build(&names(6), LegendPosition::Top, Rect::new(0.0, 0.0, 200.0, 300.0)).unwrap();
        assert!(narrow.bounds.height > wide.bounds.height);
        assert!(narrow.entries.len() <= 6);
    }

    #[test]
    fn a_bottom_legend_sits_at_the_bottom_of_the_frame() {
        let frame = Rect::new(10.0, 20.0, 400.0, 300.0);
        let legend = build(&names(3), LegendPosition::Bottom, frame).unwrap();
        assert!((legend.bounds.bottom() - frame.bottom()).abs() < 1e-4);
        for entry in &legend.entries {
            assert!(entry.swatch.is_inside(&legend.bounds), "{:?}", entry.swatch);
        }
    }

    #[test]
    fn a_right_legend_is_a_column_flush_to_the_right_edge() {
        let frame = Rect::new(0.0, 0.0, 400.0, 300.0);
        let legend = build(&names(3), LegendPosition::Right, frame).unwrap();
        assert!((legend.bounds.right() - frame.right()).abs() < 1e-4);
        assert!(legend.bounds.width < frame.width * MAX_BAND_FRACTION + 1.0);
        // One entry per row, in order, no overlap.
        for pair in legend.entries.windows(2) {
            assert!(pair[1].swatch.top() >= pair[0].swatch.bottom());
        }
    }

    #[test]
    fn a_legend_never_takes_more_than_its_share_of_the_frame() {
        let frame = Rect::new(0.0, 0.0, 120.0, 120.0);
        let legend = build(&names(30), LegendPosition::Top, frame).unwrap();
        assert!(legend.bounds.height <= frame.height * MAX_BAND_FRACTION + 1e-3);
        assert!(legend.omitted > 0, "the entries that did not fit must be reported");
        assert_eq!(legend.entries.len() + legend.omitted, 30);
    }

    #[test]
    fn an_unnamed_series_still_gets_a_key() {
        let unnamed = vec![String::new(), "  ".to_string()];
        let legend = build(&unnamed, LegendPosition::Top, Rect::new(0.0, 0.0, 400.0, 300.0)).unwrap();
        assert_eq!(legend.entries[0].label.text, "Series 1");
        assert_eq!(legend.entries[1].label.text, "Series 2");
    }

    #[test]
    fn a_frame_with_no_room_yields_no_legend_rather_than_geometry_outside_it() {
        assert!(build(&names(3), LegendPosition::Top, Rect::ZERO).is_none());
        assert!(build(&names(3), LegendPosition::Right, Rect::new(0.0, 0.0, 6.0, 300.0)).is_none());
    }

    #[test]
    fn legend_text_wears_a_text_token_not_the_series_colour() {
        let palette = Palette::new(Theme::Light);
        let legend = build(&names(3), LegendPosition::Top, Rect::new(0.0, 0.0, 400.0, 300.0)).unwrap();
        for entry in &legend.entries {
            assert_eq!(entry.label.colour, palette.text_muted);
        }
    }
}
