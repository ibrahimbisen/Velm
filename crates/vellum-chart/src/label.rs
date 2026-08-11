//! Text placed on a chart, and the rule that stops two pieces of it overlapping.
//!
//! # Why labels need an algorithm at all
//!
//! Every other piece of chart geometry has one correct position. A label has
//! several — above the point, below it, inside the bar, past its end — and which one
//! is right depends on what is already there. Two overlapping numbers are worse than
//! one number, because the reader cannot trust either; a label clipped by its own
//! bar is worse still, because it silently drops digits and `1,204` becomes `1,2`.
//!
//! So placement is a search: each label proposes candidate boxes in order of
//! preference, and [`Placer`] takes the first that fits inside the plot and misses
//! everything already placed. If none fits, the label is **dropped**, counted, and
//! the value stays reachable from the axis, the legend and the caller's own table
//! view. Dropping is a real answer, not a failure — a chart that labels every point
//! is unreadable anyway.
//!
//! # Labelling selectively
//!
//! [`LabelPolicy`] defaults to labelling nothing on multi-point series. A number
//! beside every mark is noise: the eye reads a *few* numbers and skips a field of
//! them, so direct labels only work while they are sparing. The endpoint, the
//! extreme, or the one series the chart is about — the axis carries the rest.

use crate::colour::Colour;
use crate::geom::{Point, Rect, Segment};
use serde::{Deserialize, Serialize};

/// How the glyphs sit horizontally inside [`Label::rect`].
///
/// The rect is already the measured size of the text, so alignment looks redundant —
/// it is not. A renderer with a real shaper may measure a proportional string a
/// pixel or two differently from whatever [`crate::text::TextMetrics`] the layout
/// used, and this says which edge must stay put when that happens. On a left axis
/// the labels are flush to the axis, so the *right* edge is the fixed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextAlign {
    Start,
    Centre,
    End,
}

/// A positioned run of text.
///
/// `rect` is the tight box: its width is the measured advance and its height is one
/// line height, with the glyphs vertically centred in it. Nothing here carries a
/// font — the size and the colour are what a renderer needs on top of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub text: String,
    pub rect: Rect,
    pub align: TextAlign,
    pub size: f32,
    pub colour: Colour,
}

impl Label {
    pub fn new(text: String, rect: Rect, align: TextAlign, size: f32, colour: Colour) -> Self {
        Self { text, rect, align, size, colour }
    }
}

/// Which of a mark's candidate positions a value label ended up in. Renderers do not
/// need this; it is here because it is exactly what a layout test wants to assert,
/// and because a label that had to move outside its bar is worth being able to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Placement {
    /// Inside the mark, against its value end — the tidiest, when it fits.
    InsideEnd,
    /// Just past the mark's value end.
    OutsideEnd,
    Above,
    Below,
    Left,
    Right,
}

/// A value label: the number, where it went, and how it stays attached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueLabel {
    pub series: usize,
    pub index: usize,
    pub value: f64,
    pub label: Label,
    pub placement: Placement,
    /// The point on the mark this label belongs to. A renderer needs it for
    /// hit-testing; the placer needs it to draw a leader when the label had to move.
    pub anchor: Point,
    /// Drawn only when the label was pushed far enough from its mark that the
    /// association would otherwise be guesswork. Leaders, rather than stacking
    /// labels in a column, are what keeps a converging line chart readable.
    pub leader: Option<Segment>,
}

/// Which marks get a value label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum LabelPolicy {
    /// The axis carries every number. The right default for a dense chart.
    #[default]
    None,
    /// The last point of each series — where a line ends and the eye already is.
    Endpoints,
    /// The largest and smallest value of each series.
    Extremes,
    /// Every mark. Correct for a short bar chart, noise for anything else; the
    /// placer will drop what does not fit rather than let it collide.
    All,
}

impl LabelPolicy {
    /// Whether index `index` of a series of `len` values is labelled.
    pub fn labels(self, index: usize, len: usize, extremes: Option<(usize, usize)>) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Endpoints => len > 0 && index + 1 == len,
            Self::Extremes => extremes.is_some_and(|(lo, hi)| index == lo || index == hi),
        }
    }
}

/// Greedy first-fit placement with collision avoidance.
///
/// Greedy rather than optimal on purpose. Finding the arrangement that places the
/// most labels is a packing problem, and solving it would make placement depend on
/// labels the reader has not looked at yet — nudging one number could move six
/// others. First-fit in a fixed order is stable: adding a series never moves the
/// labels already placed, so a chart does not reshuffle itself as data arrives.
///
/// The order is therefore part of the contract, and callers pass the important
/// labels first: extremes before ordinary points, earlier series before later ones.
#[derive(Debug, Clone)]
pub struct Placer {
    clip: Rect,
    padding: f32,
    placed: Vec<Rect>,
}

impl Placer {
    /// `clip` is the region a label must stay inside — the whole chart frame, not
    /// the plot: a bar's value sits just outside the plot area by design, and
    /// clipping it to the plot would drop exactly the labels that fit best.
    ///
    /// `padding` is the breathing room enforced between two labels. Two numbers
    /// separated by a hairline are legible but read as one string.
    pub fn new(clip: Rect, padding: f32) -> Self {
        Self { clip, padding, placed: Vec::new() }
    }

    /// Takes the first candidate that fits, or `None` when none does.
    ///
    /// A `None` return means the label is dropped — see the module docs on why that
    /// is an answer rather than a failure.
    pub fn place(&mut self, candidates: &[(Rect, Placement)]) -> Option<(Rect, Placement)> {
        for &(rect, placement) in candidates {
            if !rect.is_finite() || rect.is_empty() || !rect.is_inside(&self.clip) {
                continue;
            }
            let padded = rect.inset(-self.padding, -self.padding);
            if self.placed.iter().any(|other| other.intersects(&padded)) {
                continue;
            }
            self.placed.push(rect);
            return Some((rect, placement));
        }
        None
    }

    /// Reserves a box without proposing alternatives — for text that is not
    /// negotiable, such as axis tick labels, which value labels must then avoid.
    pub fn reserve(&mut self, rect: Rect) {
        self.placed.push(rect);
    }

    pub fn placed_count(&self) -> usize {
        self.placed.len()
    }
}

/// Chooses how many ticks to skip so that a run of labels stops overlapping.
///
/// Uniform skipping, not greedy dropping: an axis labelled `0, 20, 40, 60` reads as
/// a scale, while `0, 20, 60` reads as missing data. Strides that divide the gap
/// count are tried first so that both ends of the axis keep their label, which is
/// where a reader looks for the range.
///
/// `extents` are the label boxes in axis order, and `spacing` is the minimum gap
/// required between two of them. Returns a stride of at least 1.
///
/// The extents are ordered by position before the gaps are measured, because a
/// **vertical value axis runs downwards**: y grows towards the axis's *minimum*, so
/// its ticks arrive in descending order. Measuring `next.start - previous.end`
/// on that order gives a negative gap for every pair, no stride ever fits, and the
/// axis silently loses every label but the first. Ticks are uniformly spaced, so
/// which end the stride is anchored at does not change which of them fit.
pub fn choose_stride(extents: &[(f32, f32)], spacing: f32) -> usize {
    let count = extents.len();
    if count < 2 {
        return 1;
    }
    let mut ordered: Vec<(f32, f32)> = extents.to_vec();
    ordered.sort_by(|a, b| a.0.total_cmp(&b.0));
    let fits = |stride: usize| {
        ordered
            .iter()
            .step_by(stride)
            .collect::<Vec<_>>()
            .windows(2)
            .all(|pair| pair[1].0 - pair[0].1 >= spacing)
    };
    let divisors = (1..=count - 1).filter(|stride| (count - 1).is_multiple_of(*stride));
    for stride in divisors {
        if fits(stride) {
            return stride;
        }
    }
    (1..count).find(|stride| fits(*stride)).unwrap_or(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::pt;

    fn box_at(x: f32, y: f32) -> Rect {
        Rect::new(x, y, 20.0, 10.0)
    }

    #[test]
    fn the_first_candidate_that_fits_wins() {
        let mut placer = Placer::new(Rect::new(0.0, 0.0, 100.0, 100.0), 2.0);
        let first = placer.place(&[(box_at(0.0, 0.0), Placement::OutsideEnd)]);
        assert_eq!(first.map(|(_, p)| p), Some(Placement::OutsideEnd));

        // The preferred position now collides, so the second candidate is taken.
        let second = placer.place(&[
            (box_at(5.0, 5.0), Placement::OutsideEnd),
            (box_at(50.0, 50.0), Placement::InsideEnd),
        ]);
        assert_eq!(second.map(|(r, p)| (r.x, p)), Some((50.0, Placement::InsideEnd)));
    }

    #[test]
    fn a_label_that_fits_nowhere_is_dropped_rather_than_overlapped() {
        let mut placer = Placer::new(Rect::new(0.0, 0.0, 100.0, 100.0), 2.0);
        placer.place(&[(box_at(0.0, 0.0), Placement::Above)]).unwrap();
        assert_eq!(placer.place(&[(box_at(1.0, 1.0), Placement::Above)]), None);
        assert_eq!(placer.placed_count(), 1);
    }

    #[test]
    fn a_label_outside_the_clip_is_never_placed() {
        let mut placer = Placer::new(Rect::new(0.0, 0.0, 100.0, 100.0), 0.0);
        assert_eq!(placer.place(&[(box_at(95.0, 0.0), Placement::Above)]), None);
        assert_eq!(placer.place(&[(box_at(-1.0, 0.0), Placement::Above)]), None);
        // Exactly flush with the edge is inside.
        assert!(placer.place(&[(box_at(80.0, 90.0), Placement::Above)]).is_some());
    }

    #[test]
    fn padding_keeps_two_labels_from_reading_as_one_string() {
        let mut tight = Placer::new(Rect::new(0.0, 0.0, 100.0, 100.0), 0.0);
        tight.place(&[(box_at(0.0, 0.0), Placement::Above)]).unwrap();
        assert!(tight.place(&[(box_at(20.5, 0.0), Placement::Above)]).is_some());

        let mut padded = Placer::new(Rect::new(0.0, 0.0, 100.0, 100.0), 2.0);
        padded.place(&[(box_at(0.0, 0.0), Placement::Above)]).unwrap();
        assert_eq!(padded.place(&[(box_at(20.5, 0.0), Placement::Above)]), None);
    }

    #[test]
    fn degenerate_candidates_are_skipped_not_placed() {
        let mut placer = Placer::new(Rect::new(0.0, 0.0, 100.0, 100.0), 0.0);
        let empty = Rect::new(10.0, 10.0, 0.0, 10.0);
        let nan = Rect::new(f32::NAN, 0.0, 10.0, 10.0);
        assert_eq!(
            placer.place(&[(empty, Placement::Above), (nan, Placement::Below)]),
            None
        );
    }

    #[test]
    fn reserved_boxes_push_value_labels_out_of_the_way() {
        let mut placer = Placer::new(Rect::new(0.0, 0.0, 100.0, 100.0), 1.0);
        placer.reserve(box_at(0.0, 0.0));
        assert_eq!(placer.place(&[(box_at(2.0, 2.0), Placement::Above)]), None);
    }

    #[test]
    fn the_policy_picks_the_marks_that_earn_a_number() {
        let extremes = Some((1_usize, 3_usize));
        assert!(!LabelPolicy::None.labels(0, 4, extremes));
        assert!(LabelPolicy::All.labels(0, 4, extremes));
        assert!(LabelPolicy::Endpoints.labels(3, 4, extremes));
        assert!(!LabelPolicy::Endpoints.labels(2, 4, extremes));
        assert!(LabelPolicy::Extremes.labels(1, 4, extremes));
        assert!(LabelPolicy::Extremes.labels(3, 4, extremes));
        assert!(!LabelPolicy::Extremes.labels(2, 4, extremes));
        // A series with no values at all has no extremes and no labels.
        assert!(!LabelPolicy::Extremes.labels(0, 0, None));
        assert!(!LabelPolicy::Endpoints.labels(0, 0, None));
    }

    #[test]
    fn a_stride_is_chosen_to_keep_both_ends_of_the_axis() {
        // Five labels 20 wide pitched 25 apart: they fit with a 2px gap.
        let roomy: Vec<(f32, f32)> = (0..5).map(|i| (i as f32 * 25.0, i as f32 * 25.0 + 20.0)).collect();
        assert_eq!(choose_stride(&roomy, 2.0), 1);

        // Pitched 21 apart they do not, and every second label is dropped. Five
        // labels means four gaps, so a stride of 2 keeps the first and the last.
        let tight: Vec<(f32, f32)> = (0..5).map(|i| (i as f32 * 21.0, i as f32 * 21.0 + 20.0)).collect();
        assert_eq!(choose_stride(&tight, 2.0), 2);

        // Absurdly wide labels: the stride grows rather than the labels overlapping.
        let overlapping: Vec<(f32, f32)> = (0..5).map(|i| (i as f32 * 5.0, i as f32 * 5.0 + 40.0)).collect();
        assert!(choose_stride(&overlapping, 2.0) >= 4);
    }

    /// The bug this ordering exists for: a left axis hands its extents over from the
    /// bottom of the plot upwards, so they descend. Before the sort, that made every
    /// gap negative and cost the axis every label but one.
    #[test]
    fn a_descending_axis_gets_the_same_stride_as_an_ascending_one() {
        let ascending: Vec<(f32, f32)> =
            (0..5).map(|i| (i as f32 * 40.0, i as f32 * 40.0 + 15.0)).collect();
        let mut descending = ascending.clone();
        descending.reverse();
        assert_eq!(choose_stride(&ascending, 4.0), 1);
        assert_eq!(choose_stride(&descending, 4.0), 1);

        let tight: Vec<(f32, f32)> =
            (0..5).map(|i| (i as f32 * 16.0, i as f32 * 16.0 + 15.0)).collect();
        let mut tight_descending = tight.clone();
        tight_descending.reverse();
        assert_eq!(choose_stride(&tight, 4.0), choose_stride(&tight_descending, 4.0));
        assert!(choose_stride(&tight, 4.0) > 1);
    }

    #[test]
    fn a_stride_is_defined_for_zero_and_one_label() {
        assert_eq!(choose_stride(&[], 2.0), 1);
        assert_eq!(choose_stride(&[(0.0, 10.0)], 2.0), 1);
    }

    #[test]
    fn a_leader_line_carries_the_label_back_to_its_mark() {
        // Not an algorithm, just the shape of the association the placer records.
        let leader = Segment::new(pt(10.0, 10.0), pt(30.0, 4.0));
        assert!(leader.is_finite());
        assert!(leader.from.distance(leader.to) > 0.0);
    }
}
