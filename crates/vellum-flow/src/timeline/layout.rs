//! Turning a timeline into rectangles, and turning a point back into a date.
//!
//! Every horizontal coordinate here comes from the same [`TimeAxis`], so a bar's left
//! edge, the gridline under it and the date the pointer reads are three uses of one
//! expression rather than three chances to disagree. Vertical position comes from the
//! lane packing, which is derived from the dates alone — see [`lanes`](super::lanes).
//!
//! # Dragging a bar re-packs the whole timeline
//!
//! A bar has no stored row, so moving one can change which lane it lands in — and
//! that cannot be known without packing again. [`TimelineLayout::drop_target`]
//! therefore re-packs the bars with the dragged one at its proposed dates, which is
//! `O(n log n)` per pointer move. For the tens of bars a roadmap holds that is
//! nothing, and the alternative — previewing a lane that the drop then contradicts —
//! is a card that jumps out from under the cursor when it lands.

use crate::date::{Date, DateRange};
use crate::geometry::{Point, Rect, Size};
use crate::id::BarId;
use crate::metrics::TimelineMetrics;
use crate::timeline::axis::{Tick, TimeAxis};
use crate::timeline::{Dependency, LanePolicy, LinkKind, Timeline, lanes};

/// Absolute rectangles for a whole timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineLayout {
    /// The area the timeline was laid out in.
    pub rect: Rect,
    /// The container's title strip.
    pub title: Rect,
    /// The date scale across the top.
    pub axis: Rect,
    /// Where the lanes are.
    pub body: Rect,
    /// The dates the axis covers.
    pub span: DateRange,
    /// The map between dates and x. Public because a caller drawing a today-marker or
    /// a highlighted quarter needs exactly this and nothing else.
    pub scale: TimeAxis,
    pub ticks: Vec<Tick>,
    pub lanes: Vec<LaneLayout>,
    /// In the timeline's own order, not lane order, so this can be zipped with
    /// [`Timeline::bars`].
    pub bars: Vec<BarLayout>,
    pub links: Vec<LinkLayout>,
    /// What the timeline needs. Larger than `rect` when the axis does not fit.
    pub content: Size,
    metrics: TimelineMetrics,
    policy: LanePolicy,
}

/// One packed row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaneLayout {
    pub index: usize,
    pub rect: Rect,
}

/// One bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarLayout {
    pub id: BarId,
    pub lane: usize,
    pub rect: Rect,
    /// Carried so that a repack — for a drop preview, a tooltip, an export — needs
    /// nothing but the layout.
    pub range: DateRange,
}

/// One dependency arrow, reduced to the two points it joins.
///
/// Routing between them is `vellum-connect`'s job; which ends to join is this
/// crate's, because it depends on the [`LinkKind`] and on where the lane packing put
/// the two bars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkLayout {
    pub from: BarId,
    pub to: BarId,
    pub kind: LinkKind,
    /// On the predecessor.
    pub source: Point,
    /// On the successor.
    pub target: Point,
}

/// Which end of a bar a point is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarEdge {
    Start,
    End,
}

/// What is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineTarget {
    Outside,
    Title,
    /// Inside the timeline but on none of its parts.
    Chrome,
    /// The date scale. Carries the date under the pointer, which is what a click
    /// there is for.
    Axis { date: Date },
    /// Empty space in a lane — where a drag to create a bar begins.
    Lane { index: usize, date: Date },
    Bar(BarId),
    /// Within the grab tolerance of a bar's start or end: a resize handle.
    BarEdge { bar: BarId, edge: BarEdge },
}

/// What a drag is doing to a bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarDrag {
    /// Sliding the whole bar, keeping its length.
    Move,
    /// Dragging the start, keeping the end.
    Start,
    /// Dragging the end, keeping the start.
    End,
}

/// Where a dragged bar would end up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimelineDrop {
    pub bar: BarId,
    /// The dates it would take. Pass to [`Timeline::set_range`] to commit.
    pub range: DateRange,
    /// The lane it would land in, after everything is re-packed.
    pub lane: usize,
    /// Where it would be drawn.
    pub preview: Rect,
}

impl Timeline {
    /// Absolute rectangles for the container chrome and every child, in `area`.
    pub fn layout(&self, area: Rect) -> TimelineLayout {
        let m = self.metrics();
        let (title, below) = area.split_top(m.title_height);
        let inner = below.inset_by(m.padding);
        let (axis_strip, body) = inner.split_top(m.axis_height);

        let span = self.span();
        let day_width = match m.day_width {
            Some(width) => width.max(m.min_day_width),
            // Fit: the whole span in the width available, but never so fine that
            // bars stop being distinguishable — past that the timeline overflows.
            None => (inner.width() / f64::from(span.length())).max(m.min_day_width),
        };
        let scale = TimeAxis::new(span, inner.left(), day_width);

        let packing = self.lanes();
        let lane_width = inner.width().max(scale.width());
        let lanes: Vec<LaneLayout> = (0..packing.lane_count)
            .map(|index| LaneLayout {
                index,
                rect: Rect::new(inner.left(), lane_top(body, &m, index), lane_width, m.lane_height),
            })
            .collect();

        let bars: Vec<BarLayout> = self
            .bars()
            .iter()
            .enumerate()
            .map(|(index, bar)| BarLayout {
                id: bar.id(),
                lane: packing.lanes[index],
                rect: bar_rect(body, &m, scale, bar.range(), packing.lanes[index]),
                range: bar.range(),
            })
            .collect();

        let links = self
            .dependencies()
            .iter()
            .filter_map(|link| link_layout(*link, &bars))
            .collect();

        let lane_extent = if packing.lane_count == 0 {
            0.0
        } else {
            packing.lane_count as f64 * (m.lane_height + m.lane_gap) - m.lane_gap
        };
        let content = Size::new(
            2.0 * m.padding + scale.width(),
            m.title_height + 2.0 * m.padding + m.axis_height + lane_extent,
        );

        TimelineLayout {
            rect: area,
            title,
            axis: axis_strip,
            body,
            span,
            scale,
            ticks: scale.ticks(m.min_tick_spacing),
            lanes,
            bars,
            links,
            content,
            metrics: m,
            policy: self.policy(),
        }
    }
}

fn lane_top(body: Rect, metrics: &TimelineMetrics, lane: usize) -> f64 {
    body.top() + lane as f64 * (metrics.lane_height + metrics.lane_gap)
}

/// A bar is centred in its lane, so the lane's own edges stay available for the
/// hover and selection chrome that has to sit outside the bar without overlapping
/// the lane above.
fn bar_rect(
    body: Rect,
    metrics: &TimelineMetrics,
    scale: TimeAxis,
    range: DateRange,
    lane: usize,
) -> Rect {
    let (left, right) = scale.x_for_range(range);
    let top = lane_top(body, metrics, lane) + (metrics.lane_height - metrics.bar_height) * 0.5;
    Rect::new(left, top, right - left, metrics.bar_height)
}

/// Which ends of the two bars an arrow joins, per [`LinkKind`].
fn link_layout(link: Dependency, bars: &[BarLayout]) -> Option<LinkLayout> {
    let from = bars.iter().find(|bar| bar.id == link.from)?;
    let to = bars.iter().find(|bar| bar.id == link.to)?;
    let start = |bar: &BarLayout| Point::new(bar.rect.left(), bar.rect.centre().y);
    let finish = |bar: &BarLayout| Point::new(bar.rect.right(), bar.rect.centre().y);
    let (source, target) = match link.kind {
        LinkKind::FinishToStart => (finish(from), start(to)),
        LinkKind::StartToStart => (start(from), start(to)),
        LinkKind::FinishToFinish => (finish(from), finish(to)),
        LinkKind::StartToFinish => (start(from), finish(to)),
    };
    Some(LinkLayout { from: link.from, to: link.to, kind: link.kind, source, target })
}

impl TimelineLayout {
    pub fn bar(&self, id: BarId) -> Option<&BarLayout> {
        self.bars.iter().find(|bar| bar.id == id)
    }

    pub fn lane(&self, index: usize) -> Option<&LaneLayout> {
        self.lanes.get(index)
    }

    pub fn overflows(&self) -> bool {
        self.content.width > self.rect.width() || self.content.height > self.rect.height()
    }

    /// Which day an x coordinate falls in.
    pub fn date_at(&self, x: f64) -> Date {
        self.scale.date_at(x)
    }

    /// What is under `point`, with `tolerance` pixels of grab range on a bar's ends.
    ///
    /// A bar narrower than twice the tolerance is all handle; its start wins, because
    /// a bar that short is usually one being dragged out from nothing and the end is
    /// the edge already under the pointer.
    pub fn hit_test(&self, point: Point, tolerance: f64) -> TimelineTarget {
        if !self.rect.contains(point) {
            return TimelineTarget::Outside;
        }
        if self.title.contains(point) {
            return TimelineTarget::Title;
        }
        if self.axis.contains(point) {
            return TimelineTarget::Axis { date: self.date_at(point.x) };
        }
        for bar in &self.bars {
            if point.y < bar.rect.top() || point.y >= bar.rect.bottom() {
                continue;
            }
            if (point.x - bar.rect.left()).abs() <= tolerance {
                return TimelineTarget::BarEdge { bar: bar.id, edge: BarEdge::Start };
            }
            if (point.x - bar.rect.right()).abs() <= tolerance {
                return TimelineTarget::BarEdge { bar: bar.id, edge: BarEdge::End };
            }
            if bar.rect.contains(point) {
                return TimelineTarget::Bar(bar.id);
            }
        }
        if let Some(lane) = self.lanes.iter().find(|lane| lane.rect.contains(point)) {
            return TimelineTarget::Lane { index: lane.index, date: self.date_at(point.x) };
        }
        TimelineTarget::Chrome
    }

    /// Where a bar would end up after a horizontal drag of `dx` pixels.
    ///
    /// The distance becomes a whole number of days — bars snap to the day, because a
    /// roadmap has no finer unit — and the timeline is re-packed with the bar at its
    /// new dates, so `lane` and `preview` are what the drop will actually produce.
    ///
    /// One caveat, and it is inherent rather than an oversight: on an automatic axis
    /// (see [`AxisSpan::Auto`](crate::timeline::AxisSpan::Auto)) a drag that takes a
    /// bar past the end of the current span will *widen the span* when it is
    /// committed, which moves everything. The preview is drawn on the axis as it is
    /// now. A caller that wants a drag to feel rigid should pin the span for the
    /// duration of it, which is what [`AxisSpan::Fixed`](crate::timeline::AxisSpan::Fixed)
    /// is for.
    pub fn drop_target(&self, bar: BarId, drag: BarDrag, dx: f64) -> Option<TimelineDrop> {
        let index = self.bars.iter().position(|laid| laid.id == bar)?;
        let days = self.scale.days_for(dx);
        let current = self.bars[index].range;
        let range = match drag {
            BarDrag::Move => current.shifted(days),
            BarDrag::Start => current.with_start(current.start().add_days(days)),
            BarDrag::End => current.with_end(current.end().add_days(days)),
        };

        let lane = match self.policy {
            LanePolicy::Packed => {
                let ranges: Vec<DateRange> = self
                    .bars
                    .iter()
                    .map(|laid| if laid.id == bar { range } else { laid.range })
                    .collect();
                lanes::pack(&ranges).lanes[index]
            }
            LanePolicy::OnePerBar => index,
        };

        Some(TimelineDrop {
            bar,
            range,
            lane,
            preview: bar_rect(self.body, &self.metrics, self.scale, range, lane),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::AxisSpan;

    const AREA: Rect =
        Rect { origin: Point { x: 0.0, y: 0.0 }, size: Size { width: 1000.0, height: 500.0 } };

    fn day(text: &str) -> Date {
        text.parse().unwrap()
    }

    fn range(start: &str, end: &str) -> DateRange {
        DateRange::new(day(start), day(end))
    }

    /// A fixed axis of exactly 100 days at 9.76px each, so the arithmetic in the
    /// assertions is the layout's and not the test's.
    fn plan() -> (Timeline, Vec<BarId>) {
        let mut plan = Timeline::new("Renderer");
        let bars = vec![
            plan.add_bar("SDF shapes", range("2026-08-01", "2026-08-20")),
            plan.add_bar("Glyph atlas", range("2026-08-10", "2026-09-05")),
            plan.add_bar("Ship", range("2026-09-06", "2026-09-12")),
        ];
        plan.set_axis_span(AxisSpan::Fixed(range("2026-08-01", "2026-11-08")));
        (plan, bars)
    }

    #[test]
    fn the_chrome_stacks_title_then_axis_then_lanes() {
        let (plan, _) = plan();
        let m = plan.metrics();
        let layout = plan.layout(AREA);

        assert_eq!(layout.title, Rect::new(0.0, 0.0, 1000.0, m.title_height));
        assert_eq!(layout.axis.top(), m.title_height + m.padding);
        assert_eq!(layout.axis.height(), m.axis_height);
        assert_eq!(layout.body.top(), layout.axis.bottom());
        assert_eq!(layout.scale.left(), m.padding);
        assert_eq!(layout.span, range("2026-08-01", "2026-11-08"));

        // Two lanes, stacked with one gap between them.
        assert_eq!(layout.lanes.len(), 2);
        assert_eq!(layout.lanes[0].rect.top(), layout.body.top());
        assert_eq!(layout.lanes[1].rect.top(), layout.lanes[0].rect.bottom() + m.lane_gap);
    }

    #[test]
    fn a_bar_spans_its_dates_and_sits_centred_in_its_lane() {
        let (plan, bars) = plan();
        let m = plan.metrics();
        let layout = plan.layout(AREA);
        let first = layout.bar(bars[0]).unwrap();

        assert_eq!(first.rect.left(), layout.scale.x_for(day("2026-08-01")));
        assert_eq!(first.rect.right(), layout.scale.x_after(day("2026-08-20")));
        assert_eq!(first.rect.width(), 20.0 * layout.scale.day_width(), "inclusive of both ends");
        assert_eq!(first.rect.height(), m.bar_height);

        let lane = layout.lane(first.lane).unwrap();
        assert_eq!(first.rect.top() - lane.rect.top(), lane.rect.bottom() - first.rect.bottom());
        // The overlapping bar is in the other lane; the third re-uses the first.
        assert_ne!(layout.bar(bars[1]).unwrap().lane, first.lane);
        assert_eq!(layout.bar(bars[2]).unwrap().lane, first.lane);
    }

    #[test]
    fn ticks_span_the_axis_and_line_up_with_the_dates_under_them() {
        let (plan, _) = plan();
        let layout = plan.layout(AREA);
        assert!(!layout.ticks.is_empty());
        for tick in &layout.ticks {
            assert_eq!(tick.x, layout.scale.x_for(tick.date));
            assert!(layout.span.contains(tick.date));
            assert_eq!(layout.date_at(tick.x), tick.date);
        }
        // Ticks never crowd: the axis chose a scale that respects the spacing.
        let spacing = plan.metrics().min_tick_spacing;
        for pair in layout.ticks.windows(2) {
            assert!(pair[1].x - pair[0].x >= spacing, "{} apart", pair[1].x - pair[0].x);
        }
    }

    #[test]
    fn a_fixed_day_width_overflows_rather_than_shrinking_the_axis() {
        let (mut plan, _) = plan();
        let mut m = plan.metrics();
        m.day_width = Some(40.0);
        plan.set_metrics(m);
        let layout = plan.layout(AREA);
        assert_eq!(layout.scale.day_width(), 40.0);
        assert_eq!(layout.content.width, 2.0 * m.padding + 100.0 * 40.0);
        assert!(layout.overflows());
    }

    #[test]
    fn hit_testing_finds_bars_their_handles_lanes_and_the_axis() {
        let (plan, bars) = plan();
        let layout = plan.layout(AREA);
        let bar = *layout.bar(bars[1]).unwrap();

        assert_eq!(layout.hit_test(bar.rect.centre(), 4.0), TimelineTarget::Bar(bars[1]));
        assert_eq!(
            layout.hit_test(Point::new(bar.rect.left() + 1.0, bar.rect.centre().y), 4.0),
            TimelineTarget::BarEdge { bar: bars[1], edge: BarEdge::Start }
        );
        assert_eq!(
            layout.hit_test(Point::new(bar.rect.right() - 1.0, bar.rect.centre().y), 4.0),
            TimelineTarget::BarEdge { bar: bars[1], edge: BarEdge::End }
        );

        let axis_point = Point::new(layout.scale.x_for(day("2026-09-01")) + 1.0, layout.axis.centre().y);
        assert_eq!(layout.hit_test(axis_point, 4.0), TimelineTarget::Axis { date: day("2026-09-01") });

        // Empty space in a lane, well right of every bar.
        let empty = Point::new(layout.scale.x_for(day("2026-11-01")), bar.rect.centre().y);
        assert_eq!(
            layout.hit_test(empty, 4.0),
            TimelineTarget::Lane { index: bar.lane, date: day("2026-11-01") }
        );
        assert_eq!(layout.hit_test(layout.title.centre(), 4.0), TimelineTarget::Title);
        assert_eq!(layout.hit_test(Point::new(-1.0, 10.0), 4.0), TimelineTarget::Outside);
    }

    /// The contract: what the preview draws is where the bar lands.
    #[test]
    fn every_previewed_drag_lands_exactly_where_it_was_drawn() {
        let (plan, bars) = plan();
        let layout = plan.layout(AREA);

        for &bar in &bars {
            for drag in [BarDrag::Move, BarDrag::Start, BarDrag::End] {
                for dx in [-200.0, -50.0, -5.0, 0.0, 5.0, 50.0, 200.0] {
                    let drop = layout.drop_target(bar, drag, dx).unwrap();
                    let mut moved = plan.clone();
                    moved.set_range(bar, drop.range).unwrap();
                    let after = moved.layout(AREA);
                    let landed = after.bar(bar).unwrap();
                    assert_eq!(landed.rect, drop.preview, "{bar} {drag:?} by {dx}");
                    assert_eq!(landed.lane, drop.lane);
                    assert_eq!(landed.range, drop.range);
                }
            }
        }
    }

    #[test]
    fn a_drag_snaps_to_whole_days_and_keeps_the_length_when_moving() {
        let (plan, bars) = plan();
        let layout = plan.layout(AREA);
        let width = layout.scale.day_width();
        let original = layout.bar(bars[0]).unwrap().range;

        // Less than half a day of movement is no movement.
        assert_eq!(layout.drop_target(bars[0], BarDrag::Move, width * 0.4).unwrap().range, original);
        let moved = layout.drop_target(bars[0], BarDrag::Move, width * 3.0).unwrap().range;
        assert_eq!(moved, original.shifted(3));
        assert_eq!(moved.length(), original.length());

        // Resizing moves one end only, and cannot invert the bar.
        let stretched = layout.drop_target(bars[0], BarDrag::End, width * 5.0).unwrap().range;
        assert_eq!(stretched.start(), original.start());
        assert_eq!(stretched.length(), original.length() + 5);
        let collapsed = layout.drop_target(bars[0], BarDrag::Start, width * 500.0).unwrap().range;
        assert_eq!(collapsed.length(), 1, "dragging the start past the end pins it to a day");
    }

    /// Moving a bar out of the way can empty a lane, and the packing has to notice.
    #[test]
    fn a_drag_that_removes_an_overlap_previews_the_lane_it_frees() {
        let (plan, bars) = plan();
        let layout = plan.layout(AREA);
        assert_eq!(layout.lanes.len(), 2);

        // Slide the second bar clear of the first: everything fits in one lane.
        let clear = layout.scale.day_width() * 40.0;
        let drop = layout.drop_target(bars[1], BarDrag::Move, clear).unwrap();
        assert_eq!(drop.lane, 0);

        let mut moved = plan.clone();
        moved.set_range(bars[1], drop.range).unwrap();
        let after = moved.layout(AREA);
        assert_eq!(after.lanes.len(), 1);
        assert_eq!(after.bar(bars[1]).unwrap().rect, drop.preview);
    }

    #[test]
    fn dependency_arrows_join_the_ends_their_kind_names() {
        let (mut plan, bars) = plan();
        plan.add_dependency(bars[0], bars[2], LinkKind::FinishToStart).unwrap();
        plan.add_dependency(bars[0], bars[1], LinkKind::StartToStart).unwrap();
        let layout = plan.layout(AREA);
        let first = layout.bar(bars[0]).unwrap().rect;
        let second = layout.bar(bars[1]).unwrap().rect;
        let third = layout.bar(bars[2]).unwrap().rect;

        assert_eq!(layout.links.len(), 2);
        let fs = layout.links.iter().find(|l| l.kind == LinkKind::FinishToStart).unwrap();
        assert_eq!(fs.source, Point::new(first.right(), first.centre().y));
        assert_eq!(fs.target, Point::new(third.left(), third.centre().y));

        let ss = layout.links.iter().find(|l| l.kind == LinkKind::StartToStart).unwrap();
        assert_eq!(ss.source, Point::new(first.left(), first.centre().y));
        assert_eq!(ss.target, Point::new(second.left(), second.centre().y));
    }

    #[test]
    fn one_lane_per_bar_stacks_them_in_model_order() {
        let (mut plan, bars) = plan();
        plan.set_policy(LanePolicy::OnePerBar);
        let layout = plan.layout(AREA);
        assert_eq!(layout.lanes.len(), 3);
        for (index, &bar) in bars.iter().enumerate() {
            assert_eq!(layout.bar(bar).unwrap().lane, index);
        }
        // And a drag keeps a bar in its own row rather than re-packing it.
        let drop = layout.drop_target(bars[2], BarDrag::Move, -500.0).unwrap();
        assert_eq!(drop.lane, 2);
    }

    #[test]
    fn an_empty_timeline_lays_out_its_chrome_and_offers_no_drop() {
        let plan = Timeline::new("empty");
        let layout = plan.layout(AREA);
        assert!(layout.bars.is_empty());
        assert!(layout.lanes.is_empty());
        assert!(!layout.ticks.is_empty(), "an axis with no bars still has dates on it");
        assert_eq!(layout.drop_target(BarId::from_raw(1), BarDrag::Move, 10.0), None);
        assert_eq!(layout.hit_test(layout.body.centre(), 4.0), TimelineTarget::Chrome);
    }

    #[test]
    fn a_zero_sized_area_produces_zero_sized_chrome_rather_than_nonsense() {
        let (plan, bars) = plan();
        let layout = plan.layout(Rect::ZERO);
        assert!(layout.title.is_empty());
        assert!(layout.body.is_empty());
        assert_eq!(layout.scale.day_width(), plan.metrics().min_day_width, "clamped, not zero");
        assert!(layout.bar(bars[0]).unwrap().rect.width() > 0.0);
    }
}
