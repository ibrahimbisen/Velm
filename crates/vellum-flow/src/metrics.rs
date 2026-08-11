//! Every number a container's layout depends on, in one place.
//!
//! These are the only constants in the crate, and none of them is a colour. The
//! design language (`docs/05-design-language.md` §3) puts spacing on a **4px grid**
//! and corners at **4px**, so every default here is a multiple of four and the
//! radius is stated once rather than being re-guessed by each widget. Colour
//! resolves through a token in `vellum-ui`; a layout crate that hard-coded a hex
//! literal would be exactly the thing §1 of that document forbids.
//!
//! The values are per-container rather than global because the three containers are
//! not the same density: a kanban card is a sticky-sized object with text in it, a
//! story map cell holds several smaller ones, and a timeline bar is a label on a
//! rule. Sharing one "row height" between them would make two of the three wrong.
//!
//! Every field is public and every struct is `Copy`: a caller that wants a denser
//! board changes a field, and nothing in the layout code caches a metric.

use serde::{Deserialize, Serialize};

/// The spacing grid. Every default in this file is a multiple of it.
pub const GRID: f64 = 4.0;

/// Corner radius for container chrome and for the children inside it.
///
/// 4px, from the design language: *"corners are 4px, occasionally 6px. Not 16, not
/// fully rounded."* Carried here rather than in the renderer so that a hit-test and
/// the rounded rect it is testing against can never disagree about the shape.
pub const RADIUS: f64 = 4.0;

/// The measurements of a kanban board.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KanbanMetrics {
    /// The container's own title strip, across the top.
    pub title_height: f64,
    /// Between the container edge and the columns.
    pub padding: f64,
    /// Between one column and the next.
    pub column_gap: f64,
    /// A column's name, count and WIP badge.
    pub column_header_height: f64,
    /// Between a column's edge and its cards.
    pub column_padding: f64,
    /// Between one card and the next.
    pub card_gap: f64,
    /// A card whose height has not been measured. The caller measures text and sets
    /// [`Card::height`](crate::kanban::Card::height); until it does, every card is
    /// this tall, which is enough for a two-line sticky at 13px.
    pub card_height: f64,
    /// Below this a column has no room for a card, so the board overflows its area
    /// horizontally rather than shrinking further. Columns that do not fit are laid
    /// out past the right edge and the caller scrolls or clips — the alternative,
    /// squeezing twelve columns into 600px, produces a board nobody can use.
    pub min_column_width: f64,
}

impl Default for KanbanMetrics {
    fn default() -> Self {
        Self {
            title_height: 40.0,
            padding: 12.0,
            column_gap: 12.0,
            column_header_height: 32.0,
            column_padding: 8.0,
            card_gap: 8.0,
            card_height: 64.0,
            min_column_width: 180.0,
        }
    }
}

/// The measurements of a user story map.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoryMapMetrics {
    pub title_height: f64,
    pub padding: f64,
    /// The release rail down the left-hand side.
    pub rail_width: f64,
    /// The activity headers across the top — the backbone.
    pub header_height: f64,
    pub column_gap: f64,
    pub row_gap: f64,
    /// Between a cell's edge and the stories in it.
    pub cell_padding: f64,
    pub card_gap: f64,
    /// A story whose height has not been measured.
    pub card_height: f64,
    pub min_column_width: f64,
    /// A release row is as tall as its fullest cell, but never shorter than this —
    /// an empty release still has to be a drop target you can hit.
    pub min_row_height: f64,
}

impl Default for StoryMapMetrics {
    fn default() -> Self {
        Self {
            title_height: 40.0,
            padding: 12.0,
            rail_width: 140.0,
            header_height: 32.0,
            column_gap: 8.0,
            row_gap: 8.0,
            cell_padding: 8.0,
            card_gap: 8.0,
            card_height: 56.0,
            min_column_width: 160.0,
            min_row_height: 88.0,
        }
    }
}

/// The measurements of a timeline.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimelineMetrics {
    pub title_height: f64,
    /// The date scale across the top.
    pub axis_height: f64,
    pub padding: f64,
    /// One packed lane, including the space around its bar.
    pub lane_height: f64,
    pub lane_gap: f64,
    /// The bar itself, centred in its lane.
    pub bar_height: f64,
    /// Pixels per day, or `None` to fit the whole span into the area given.
    ///
    /// Fitting is the default because a roadmap is read as a shape — where things
    /// sit relative to each other — far more often than it is measured. A caller
    /// that wants a fixed scale, so that scrolling a long plan feels like scrolling
    /// rather than zooming, sets this.
    pub day_width: Option<f64>,
    /// A day narrower than this stops the axis shrinking and lets the content
    /// overflow instead. At 2px a decade still fits in 7,300px, which is a
    /// reasonable board object; below that the bars stop being distinguishable.
    pub min_day_width: f64,
    /// Ticks closer together than this make the axis choose a coarser unit — days
    /// become weeks, weeks become months. 56px is four grid steps wider than a
    /// `28 Jul` label at 11px, so labels never touch.
    pub min_tick_spacing: f64,
}

impl Default for TimelineMetrics {
    fn default() -> Self {
        Self {
            title_height: 40.0,
            axis_height: 28.0,
            padding: 12.0,
            lane_height: 36.0,
            lane_gap: 4.0,
            // 28 in a 36px lane leaves 4px above and below, so the margin is a grid
            // step and the selection outline has somewhere to sit without touching
            // the lane above.
            bar_height: 28.0,
            day_width: None,
            min_day_width: 2.0,
            min_tick_spacing: 56.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The design language's 4px grid, asserted rather than trusted. A metric that
    /// drifts off the grid is invisible in isolation and obvious once it sits next
    /// to a panel that did not.
    #[test]
    fn every_spacing_default_sits_on_the_grid() {
        let k = KanbanMetrics::default();
        let s = StoryMapMetrics::default();
        let t = TimelineMetrics::default();
        let spacings = [
            k.title_height,
            k.padding,
            k.column_gap,
            k.column_header_height,
            k.column_padding,
            k.card_gap,
            k.card_height,
            k.min_column_width,
            s.title_height,
            s.padding,
            s.rail_width,
            s.header_height,
            s.column_gap,
            s.row_gap,
            s.cell_padding,
            s.card_gap,
            s.card_height,
            s.min_column_width,
            s.min_row_height,
            t.title_height,
            t.axis_height,
            t.padding,
            t.lane_height,
            t.lane_gap,
            t.bar_height,
            RADIUS,
        ];
        for value in spacings {
            assert_eq!(value % GRID, 0.0, "{value} is not a multiple of the {GRID}px grid");
        }
    }

    /// A bar has to fit inside the lane that holds it, with room on both sides.
    #[test]
    fn a_bar_fits_inside_its_lane() {
        let t = TimelineMetrics::default();
        assert!(t.bar_height < t.lane_height);
        assert_eq!((t.lane_height - t.bar_height) % (GRID * 2.0), 0.0, "centred on the grid");
    }
}
