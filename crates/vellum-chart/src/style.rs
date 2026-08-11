//! Every number a chart's appearance depends on, in one place with its reason.
//!
//! The defaults are not preferences. They come from `docs/05-design-language.md` —
//! the 4px spacing grid, 11px labels, 4px corner radii — and from the mark
//! specification a quiet chart is built on: thin marks, hairline grid, a 2px gap of
//! bare surface doing the separating rather than a stroke around every fill.
//!
//! The rule those spacers encode is worth stating, because it is the one most often
//! got wrong: **never draw a border around a mark to separate it from its
//! neighbour.** An outline adds ink that is not data, and at a glance it reads as a
//! second series. Two touching fills are separated by a 2px gap of surface, and a
//! dot crossing a line is separated by a 2px ring of surface. Both are subtraction,
//! not addition.

use crate::format::NumberStyle;
use crate::label::LabelPolicy;
use crate::palette::Theme;
use serde::{Deserialize, Serialize};

/// Where the legend sits relative to the plot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum LegendPosition {
    /// No legend. Correct for a single series — the chart's own title names it, and
    /// a box with one swatch in it restates the title and costs space.
    None,
    #[default]
    Top,
    Bottom,
    Left,
    Right,
}

/// The measurements a chart is drawn to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartStyle {
    /// Which colourway to resolve against.
    pub theme: Theme,
    /// Air inside the chart's frame, before anything is laid out. 8px — two grid
    /// units, enough that a label at the frame's edge does not touch it.
    pub padding: f32,
    /// Tick and legend text. 11px is the design language's label size.
    pub text_size: f32,
    /// Gap between a tick label and its axis, and between a swatch and its name.
    pub text_gap: f32,
    /// Space enforced between two labels before they count as colliding.
    pub label_padding: f32,
    /// Which marks carry a value label.
    pub labels: LabelPolicy,
    /// Gridlines across the plot at every value tick.
    pub gridlines: bool,
    pub legend: LegendPosition,
    /// The legend's colour swatch, square. 8px matches the minimum marker size, so
    /// the key and the mark it stands for read as the same weight.
    pub legend_swatch: f32,
    /// Bars are capped rather than filling their slot: past this a bar reads as a
    /// block of colour instead of a measured length, and the band's leftover is
    /// better spent as air.
    pub bar_max_thickness: f32,
    /// The 4px round on a bar's *value* end only. The baseline end stays square —
    /// rounding the end a bar is measured from would blur where zero is.
    pub bar_radius: f32,
    /// Bare surface between touching fills: stacked segments, and adjacent bars in
    /// a group. One width everywhere, so a stack reads as one object.
    pub surface_gap: f32,
    /// Line series stroke width.
    pub line_width: f32,
    /// Radius of a scatter dot or a line marker. 4px gives the 8px minimum diameter
    /// a mark needs to be seen and to be hit.
    pub dot_radius: f32,
    /// Surface-coloured ring around a dot, so dots stay legible where they cross a
    /// line or each other.
    pub dot_ring: f32,
    /// How much axis length one value tick wants, along a vertical axis. 48px keeps
    /// four or five labels on a typical widget: enough to read the scale, few enough
    /// that the axis stays quiet.
    pub vertical_tick_pitch: f32,
    /// The same for a horizontal axis, where labels are wide rather than tall and
    /// crowd each other much sooner.
    pub horizontal_tick_pitch: f32,
    /// Forced number style, or `None` to choose one from the axis magnitude.
    pub number_style: Option<NumberStyle>,
}

impl ChartStyle {
    pub fn with_theme(theme: Theme) -> Self {
        Self { theme, ..Self::default() }
    }

    pub fn with_labels(mut self, labels: LabelPolicy) -> Self {
        self.labels = labels;
        self
    }

    pub fn with_legend(mut self, legend: LegendPosition) -> Self {
        self.legend = legend;
        self
    }

    /// How many ticks an axis of `length` should aim for. Clamped to at least two,
    /// because an axis with one label has no scale.
    pub fn tick_target(&self, length: f32, vertical: bool) -> usize {
        let pitch = if vertical { self.vertical_tick_pitch } else { self.horizontal_tick_pitch };
        if !length.is_finite() || pitch <= 0.0 {
            return 2;
        }
        ((length / pitch).round() as usize).clamp(2, 12)
    }
}

impl Default for ChartStyle {
    fn default() -> Self {
        Self {
            theme: Theme::Light,
            padding: 8.0,
            text_size: 11.0,
            text_gap: 4.0,
            label_padding: 2.0,
            labels: LabelPolicy::None,
            gridlines: true,
            legend: LegendPosition::Top,
            legend_swatch: 8.0,
            bar_max_thickness: 24.0,
            bar_radius: 4.0,
            surface_gap: 2.0,
            line_width: 2.0,
            dot_radius: 4.0,
            dot_ring: 2.0,
            vertical_tick_pitch: 48.0,
            horizontal_tick_pitch: 80.0,
            number_style: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tick_target_follows_the_axis_length() {
        let style = ChartStyle::default();
        assert_eq!(style.tick_target(240.0, true), 5);
        assert_eq!(style.tick_target(480.0, false), 6);
        // A tiny plot still gets a scale, and a huge one does not get forty labels.
        assert_eq!(style.tick_target(10.0, true), 2);
        assert_eq!(style.tick_target(1e5, true), 12);
        assert_eq!(style.tick_target(f32::NAN, true), 2);
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let style = ChartStyle::default();
        assert_eq!(style.bar_radius, 4.0, "design language §2: corners are 4px");
        assert_eq!(style.surface_gap, 2.0, "one gap width everywhere");
        assert_eq!(style.dot_radius * 2.0, 8.0, "8px is the minimum readable marker");
        assert_eq!(style.text_size, 11.0, "design language §5: 11px labels");
    }
}
