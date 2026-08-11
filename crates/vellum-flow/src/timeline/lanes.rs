//! Packing bars into as few lanes as possible.
//!
//! Two bars can share a lane exactly when their date ranges do not overlap, so this
//! is graph colouring — and the graph of a set of intervals is an *interval graph*,
//! which is perfect. That is the whole reason a roadmap can be packed greedily and
//! still be optimal: for interval graphs, sorting by start date and putting each bar
//! in the lowest lane that is free uses exactly χ colours, and χ equals the size of
//! the largest clique, which here is the largest number of bars overlapping on any
//! single day. No search, no backtracking, no heuristic — `O(n log n)`, and provably
//! the fewest lanes that exist.
//!
//! The proof is short enough to state: a new lane is opened only when every existing
//! lane holds a bar that overlaps the one being placed. Those bars all overlap each
//! other too, since they all contain the new bar's start date. So opening lane `k`
//! exhibits `k` mutually overlapping bars, and no assignment can use fewer than that.
//!
//! # Determinism
//!
//! Bars are ordered by `(start, end, position in the model)` and the lowest free lane
//! is always taken, so the same timeline packs the same way every time. A roadmap
//! whose rows shuffled when an unrelated bar moved would be unreadable, and a drop
//! preview computed from a repack would not match what the drop produced.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use crate::date::DateRange;

/// Lane assignments, parallel to the ranges that were packed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Packing {
    /// `lanes[i]` is the lane of the `i`th range handed to [`pack`].
    pub lanes: Vec<usize>,
    /// How many lanes were needed. Provably the minimum — see the module docs.
    pub lane_count: usize,
}

/// Assigns every range a lane, using as few as exist.
pub fn pack(ranges: &[DateRange]) -> Packing {
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by_key(|&i| (ranges[i].start(), ranges[i].end(), i));

    let mut lanes = vec![0usize; ranges.len()];
    let mut lane_count = 0usize;
    // Lanes whose last bar has already finished, lowest first.
    let mut free: BTreeSet<usize> = BTreeSet::new();
    // Lanes still occupied, ordered by when they come free.
    let mut occupied: BinaryHeap<Reverse<(i32, usize)>> = BinaryHeap::new();

    for index in order {
        let range = ranges[index];
        // Release every lane whose bar ends before this one starts. `end_exclusive`
        // is the day after the bar, so a bar ending on the 10th frees its lane for a
        // bar starting on the 11th — back-to-back work shares a lane, which is what
        // makes a sequential plan one row instead of twenty.
        while let Some(&Reverse((free_at, lane))) = occupied.peek() {
            if free_at <= range.start().days() {
                occupied.pop();
                free.insert(lane);
            } else {
                break;
            }
        }

        let lane = match free.pop_first() {
            Some(lane) => lane,
            None => {
                lane_count += 1;
                lane_count - 1
            }
        };
        lanes[index] = lane;
        occupied.push(Reverse((range.end_exclusive().days(), lane)));
    }

    Packing { lanes, lane_count }
}

/// The largest number of ranges overlapping on any one day.
///
/// The lower bound [`pack`] is claimed to meet. Written independently of the packer —
/// a sweep over endpoints rather than a lane assignment — so that the test comparing
/// the two is a real check and not the same code twice.
pub fn max_overlap(ranges: &[DateRange]) -> usize {
    let mut events: Vec<(i32, i32)> = Vec::with_capacity(ranges.len() * 2);
    for range in ranges {
        events.push((range.start().days(), 1));
        events.push((range.end_exclusive().days(), -1));
    }
    // Ends before starts at the same coordinate: a bar ending on the day another
    // begins does not overlap it.
    events.sort_by_key(|&(day, delta)| (day, delta));

    let mut depth = 0i32;
    let mut deepest = 0i32;
    for (_, delta) in events {
        depth += delta;
        deepest = deepest.max(depth);
    }
    deepest as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date::Date;

    fn range(start: &str, end: &str) -> DateRange {
        DateRange::new(start.parse().unwrap(), end.parse().unwrap())
    }

    /// No two bars in one lane may overlap. Asserted on every packing below.
    fn assert_no_collisions(ranges: &[DateRange], packing: &Packing) {
        for (i, a) in ranges.iter().enumerate() {
            for (j, b) in ranges.iter().enumerate().skip(i + 1) {
                if packing.lanes[i] == packing.lanes[j] {
                    assert!(!a.overlaps(*b), "{a} and {b} share lane {}", packing.lanes[i]);
                }
            }
        }
        assert!(packing.lanes.iter().all(|&lane| lane < packing.lane_count.max(1)));
    }

    /// The required case: bars that all overlap each other need one lane each.
    #[test]
    fn bars_that_all_overlap_produce_one_lane_per_bar() {
        for count in 1..12 {
            let ranges: Vec<DateRange> = (0..count)
                .map(|n| DateRange::days_from(Date::from_days(n), 30))
                .collect();
            let packing = pack(&ranges);
            assert_eq!(packing.lane_count, count as usize, "{count} overlapping bars");
            assert_no_collisions(&ranges, &packing);
            // Each one gets its own lane, and the lanes are the first `count` of them.
            let mut used = packing.lanes.clone();
            used.sort_unstable();
            assert_eq!(used, (0..count as usize).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_sequence_of_disjoint_bars_shares_one_lane() {
        let ranges: Vec<DateRange> =
            (0..10).map(|n| DateRange::days_from(Date::from_days(n * 10), 10)).collect();
        let packing = pack(&ranges);
        assert_eq!(packing.lane_count, 1, "back-to-back bars do not overlap");
        assert!(packing.lanes.iter().all(|&lane| lane == 0));
    }

    #[test]
    fn an_empty_timeline_needs_no_lanes() {
        let packing = pack(&[]);
        assert_eq!(packing.lane_count, 0);
        assert!(packing.lanes.is_empty());
        assert_eq!(max_overlap(&[]), 0);
    }

    /// The optimality claim, checked against an independent count of the deepest
    /// overlap, on shapes that break naive packers: nesting, staircases, one long
    /// bar under many short ones, and a gap that lets a lane be reused.
    #[test]
    fn the_lane_count_always_equals_the_deepest_overlap() {
        let cases: Vec<Vec<DateRange>> = vec![
            vec![range("2026-01-01", "2026-12-31"), range("2026-03-01", "2026-03-10")],
            // A staircase: each bar overlaps only its neighbour.
            (0..8).map(|n| DateRange::days_from(Date::from_days(n * 5), 8)).collect(),
            // One long bar with six short ones inside it.
            {
                let new_year = Date::from_ymd(2026, 1, 1).unwrap();
                std::iter::once(range("2026-01-01", "2026-06-30"))
                    .chain((0..6).map(|n| DateRange::days_from(new_year.add_days(n * 20), 5)))
                    .collect()
            },
            // Two clusters separated by a gap: the second reuses the first's lanes.
            vec![
                range("2026-01-01", "2026-01-31"),
                range("2026-01-05", "2026-02-05"),
                range("2026-06-01", "2026-06-30"),
                range("2026-06-05", "2026-07-05"),
            ],
            // Single-day bars, all on the same day.
            (0..5).map(|_| DateRange::single(Date::from_days(100))).collect(),
        ];

        for ranges in cases {
            let packing = pack(&ranges);
            assert_eq!(
                packing.lane_count,
                max_overlap(&ranges),
                "packing {ranges:?} used {} lanes",
                packing.lane_count
            );
            assert_no_collisions(&ranges, &packing);
        }
    }

    /// The same set of bars must pack the same way whatever order it arrives in —
    /// and the assignment must not depend on the order at all, only on the dates.
    #[test]
    fn packing_is_deterministic_and_independent_of_input_order() {
        let ranges = vec![
            range("2026-03-01", "2026-03-20"),
            range("2026-01-01", "2026-02-10"),
            range("2026-01-15", "2026-04-01"),
            range("2026-02-20", "2026-02-28"),
        ];
        let first = pack(&ranges);
        assert_eq!(pack(&ranges), first, "the same input packs the same way twice");

        let mut reordered = ranges.clone();
        reordered.reverse();
        let second = pack(&reordered);
        assert_eq!(second.lane_count, first.lane_count);
        for (index, range) in ranges.iter().enumerate() {
            let mirror = reordered.iter().position(|r| r == range).unwrap();
            assert_eq!(first.lanes[index], second.lanes[mirror], "{range} moved lane");
        }
    }

    /// Lanes are filled from the top: a bar never lands in lane 3 while lane 1 is
    /// free, which is what stops a roadmap from growing gaps as bars are moved.
    #[test]
    fn a_freed_lane_is_reused_before_a_new_one_is_opened() {
        let ranges = vec![
            range("2026-01-01", "2026-01-31"), // lane 0
            range("2026-01-01", "2026-01-31"), // lane 1
            range("2026-01-01", "2026-01-31"), // lane 2
            range("2026-02-01", "2026-02-28"), // lane 0 again
        ];
        let packing = pack(&ranges);
        assert_eq!(packing.lanes, vec![0, 1, 2, 0]);
        assert_eq!(packing.lane_count, 3);
    }
}
