//! A bitmap read as runs, and the rank of each run's first member in a second bitmap.
//!
//! `Bitmap::rank` sums the cardinality of every container below its argument, so a rank taken per
//! member of a bitmap over billions of values costs a pass over tens of thousands of containers
//! each, and a walk that takes one per row is quadratic in the bitmap's size. Rank is affine inside
//! a run: the member `e` of a run starting at `s`, with `r` members of the set below `s`, has rank
//! `r + (e - s)`. [`RankedRuns`] merges two bitmaps' runs and carries `r` forward, so a walk pays
//! for the runs it passes rather than for the containers below each one.

use std::ops::Range;

use croaring::bitmap::BitmapCursor;
use croaring::{Bitmap, RangeInclusive};

/// Ranges per cursor read: enough that the call amortises, few enough to stay on the stack.
const RUN_BUF: usize = 64;

/// A bitmap's members as ascending, non-overlapping, inclusive runs, read through the cursor in
/// bulk.
pub struct Runs<'a> {
    cursor: BitmapCursor<'a>,
    buf: [RangeInclusive<u32>; RUN_BUF],
    filled: usize,
    at: usize,
}

impl<'a> Runs<'a> {
    #[inline]
    pub fn new(bitmap: &'a Bitmap) -> Self {
        Runs {
            cursor: bitmap.cursor(),
            buf: [RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF],
            filled: 0,
            at: 0,
        }
    }

    /// Whether a run is already read and waiting, so the next costs no cursor call.
    #[inline]
    fn buffered(&self) -> bool {
        self.at < self.filled
    }

    /// Discard what is buffered and read on from the first member at or after `value`. A run
    /// holding `value` is returned from `value`.
    #[inline]
    fn seek(&mut self, value: u32) {
        self.cursor.reset_at_or_after(value);
        self.refill();
    }

    #[inline]
    fn refill(&mut self) {
        self.filled = self.cursor.read_many_ranges(&mut self.buf);
        self.at = 0;
    }
}

impl Iterator for Runs<'_> {
    /// A run as `(start, last)`, inclusive.
    type Item = (u32, u32);

    #[inline(always)]
    fn next(&mut self) -> Option<(u32, u32)> {
        if self.at == self.filled {
            self.refill();
            if self.filled == 0 {
                return None;
            }
        }
        let r = self.buf[self.at];
        self.at += 1;
        Some((r.start, r.last))
    }
}

/// The runs of `members ∩ set`, ascending, each as `(start, last, rank)` where `rank` is how many
/// members of `set` lie below `start`: the position of `start` in `set`, counting from zero.
///
/// Where a run of `members` begins past everything read so far of `set`, the walk counts the
/// members between with one range cardinality and seeks there, so a sparse `members` over a large
/// `set` costs a seek per run rather than a step over every run of `set` it passes.
pub struct RankedRuns<'a, 'b> {
    set: &'a Bitmap,
    runs: Runs<'a>,
    members: Runs<'b>,
    /// The run of `set` being read, and how many members of `set` lie below its start.
    run: Option<(u32, u32)>,
    below: u64,
    /// One past the last member of `set` counted into `below` or held in `run`.
    counted_to: u64,
    /// What is left of the current run of `members`.
    member: Option<(u32, u32)>,
}

impl<'a, 'b> RankedRuns<'a, 'b> {
    #[inline]
    pub fn new(set: &'a Bitmap, members: &'b Bitmap) -> Self {
        let mut members = Runs::new(members);
        let member = members.next();
        RankedRuns {
            set,
            runs: Runs::new(set),
            members,
            run: None,
            below: 0,
            counted_to: 0,
            member,
        }
    }
}

impl Iterator for RankedRuns<'_, '_> {
    /// A run of the intersection as `(start, last, rank)`.
    type Item = (u32, u32, u64);

    /// The next run of the intersection, or `None` when either bitmap is exhausted.
    #[inline(always)]
    fn next(&mut self) -> Option<(u32, u32, u64)> {
        loop {
            let (ls, ll) = self.member?;
            // Bring the run of `set` forward until it ends at or after `ls`.
            let (ps, pl) = loop {
                match self.run {
                    Some((ps, pl)) if pl >= ls => break (ps, pl),
                    Some((ps, pl)) => {
                        self.below += u64::from(pl - ps) + 1;
                        self.run = None;
                    }
                    None => {}
                }
                let next = if self.runs.buffered() {
                    self.runs.next()
                } else {
                    // Nothing read ahead: count what lies between here and `ls` in one call and
                    // read on from `ls`, rather than stepping every run of `set` in between.
                    let from = u32::try_from(self.counted_to).ok()?;
                    if from < ls {
                        self.below += self.set.range_cardinality(from..ls);
                    }
                    self.runs.seek(ls);
                    self.runs.next()
                };
                let (ps, pl) = next?;
                self.run = Some((ps, pl));
                self.counted_to = u64::from(pl) + 1;
            };
            if ll < ps {
                self.member = self.members.next();
                continue;
            }
            let lo = ls.max(ps);
            let hi = ll.min(pl);
            let rank = self.below + u64::from(lo - ps);
            self.member = if ll <= pl {
                self.members.next()
            } else {
                Some((pl + 1, ll))
            };
            return Some((lo, hi, rank));
        }
    }
}

/// Walk `bitmap ∩ r` as ascending, non-overlapping, half-open runs. croaring yields
/// `{start, last}` with `last` inclusive, and `last + 1` at `last == u32::MAX` is a debug panic
/// and a release wrap to 0, so the end-clamp test runs first: clamping to `r.end` covers that
/// case too, since `r.end` is exclusive and so never reaches `u32::MAX`, matching
/// [`Bitmap::from_range`]. No start-clamp is needed: `reset_at_or_after(r.start)` already begins
/// the first run at the first set value ≥ `r.start`.
pub fn for_each_run_in(bitmap: &Bitmap, r: Range<u32>, f: &mut impl FnMut(Range<u32>)) {
    if r.start >= r.end {
        return;
    }
    let mut cursor = bitmap.cursor();
    cursor.reset_at_or_after(r.start);
    let mut buf = [RangeInclusive::<u32> { start: 0, last: 0 }; RUN_BUF];
    loop {
        let n = cursor.read_many_ranges(&mut buf);
        if n == 0 {
            // Exhausted — `read_many_ranges` returning 0 is the termination signal.
            return;
        }
        for run in &buf[..n] {
            if run.start >= r.end {
                return;
            }
            if run.last >= r.end - 1 {
                f(run.start..r.end);
                return;
            }
            f(run.start..run.last + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked(set: &Bitmap, members: &Bitmap) -> Vec<(u32, u32, u64)> {
        RankedRuns::new(set, members).collect()
    }

    /// Every member of the intersection, with its rank taken the slow way.
    fn expected(set: &Bitmap, members: &Bitmap) -> Vec<(u32, u64)> {
        set.and(members)
            .iter()
            .map(|e| (e, set.rank(e) - 1))
            .collect()
    }

    fn flattened(runs: &[(u32, u32, u64)]) -> Vec<(u32, u64)> {
        runs.iter()
            .flat_map(|&(lo, hi, rank)| (lo..=hi).map(move |e| (e, rank + u64::from(e - lo))))
            .collect()
    }

    fn check(set: &Bitmap, members: &Bitmap) {
        let got = ranked(set, members);
        assert_eq!(flattened(&got), expected(set, members));
        for pair in got.windows(2) {
            assert!(pair[0].1 < pair[1].0, "runs ascend without overlap");
        }
    }

    #[test]
    fn the_ranks_are_the_ranks_the_bitmap_gives() {
        let mut set = Bitmap::new();
        set.add_range(10..5_000);
        set.add_range(70_000..200_000);
        set.add_many(&[300_000, 300_002, 300_004, 1 << 20]);
        set.add_range((3 << 20)..(3 << 20) + 9);
        set.add(u32::MAX);
        set.run_optimize();

        check(&set, &set);
        check(&set, &Bitmap::from_range(0..u32::MAX));
        check(&set, &Bitmap::new());
        check(&Bitmap::new(), &set);
        // Runs of members straddling runs of the set, gaps and single values.
        let mut members = Bitmap::new();
        members.add_range(0..20);
        members.add_range(4_990..70_010);
        members.add_many(&[300_001, 300_002, 1 << 20, (3 << 20) + 8, u32::MAX]);
        check(&set, &members);
    }

    /// Bitmaps of random runs and scattered values on both sides, run-optimised or not.
    #[test]
    fn random_bitmaps_rank_as_the_bitmap_does() {
        let mut state: u64 = 1;
        let mut below = |n: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % n
        };
        for _ in 0..400 {
            let span = 1 + below(200_000);
            let mut sides = [Bitmap::new(), Bitmap::new()];
            for side in &mut sides {
                for _ in 0..below(60) {
                    let at = below(span) as u32;
                    side.add_range(at..at + below(3_000) as u32);
                }
                for _ in 0..below(300) {
                    side.add(below(span) as u32);
                }
                if below(2) == 0 {
                    side.run_optimize();
                }
            }
            check(&sides[0], &sides[1]);
        }
    }

    /// Sets spread over the whole `u32` space, thousands of containers apart, with sparse members
    /// that make the walk seek across containers, the last value included, run-optimised or not.
    #[test]
    fn sparse_members_over_the_whole_space_rank_as_the_bitmap_does() {
        let mut state: u64 = 7;
        let mut below = |n: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 11) % n
        };
        const SPACE: u64 = 1 << 32;
        for case in 0..60 {
            let mut set = Bitmap::new();
            for _ in 0..below(400) {
                let at = below(SPACE) as u32;
                set.add_range(at..at.saturating_add(below(200_000) as u32));
            }
            for _ in 0..below(2_000) {
                set.add(below(SPACE) as u32);
            }
            let mut members = Bitmap::new();
            for _ in 0..below(300) {
                members.add(below(SPACE) as u32);
            }
            // Members inside the set's own runs as well as between them.
            for value in set.iter().step_by(1 + below(50_000) as usize).take(200) {
                members.add(value);
            }
            if case % 3 == 0 {
                set.add(u32::MAX);
                members.add(u32::MAX);
            }
            if case % 2 == 0 {
                set.run_optimize();
                members.run_optimize();
            }
            check(&set, &members);
        }
    }

    /// More runs than one buffered read holds on both sides, and members sparse enough that the
    /// walk seeks rather than steps.
    #[test]
    fn a_walk_past_the_buffer_seeks_and_keeps_counting() {
        let set: Bitmap = (0..20_000u32).map(|k| k * 3).collect();
        let every_fifth: Bitmap = (0..4_000u32).map(|k| k * 15).collect();
        let sparse: Bitmap = (0..40u32).map(|k| k * 1_501 * 3).collect();
        let runs: Bitmap = (0..1_000u32).flat_map(|k| k * 60..k * 60 + 7).collect();
        for members in [&set, &every_fifth, &sparse, &runs] {
            check(&set, members);
        }
        check(&runs, &sparse);
        check(&runs, &every_fifth);
    }
}

#[cfg(test)]
mod run_walk_tests {
    //! Unit tests for [`for_each_run_in`]: the closed→half-open conversion and its edges.

    use super::*;
    use std::ops::Range;

    fn runs_of(bitmap: &Bitmap, r: Range<u32>) -> Vec<Range<u32>> {
        let mut out = Vec::new();
        for_each_run_in(bitmap, r, &mut |run| out.push(run));
        out
    }

    /// Every emitted run flattens back to exactly `bitmap ∩ r`, half-open — the property every
    /// other test here is a named corner of.
    fn assert_flattens_to_intersection(bitmap: &Bitmap, r: Range<u32>) {
        let flat: Vec<u32> = runs_of(bitmap, r.clone()).into_iter().flatten().collect();
        let expected: Vec<u32> = bitmap.and(&Bitmap::from_range(r.clone())).to_vec();
        assert_eq!(flat, expected, "runs disagree with bitmap ∩ {r:?}");
    }

    #[test]
    fn a_run_ending_at_u32_max_is_clamped_not_overflowed() {
        // `last + 1` at `last == u32::MAX` would panic in debug and wrap in release.
        let mut b = Bitmap::new();
        b.add_range(u32::MAX - 100..=u32::MAX);
        let r = (u32::MAX - 50)..u32::MAX;
        assert_eq!(runs_of(&b, r.clone()), vec![(u32::MAX - 50)..u32::MAX]);
        // Parity with `Bitmap::from_range`, which also cannot express u32::MAX: neither route
        // ever emits it.
        assert_flattens_to_intersection(&b, r);
    }

    #[test]
    fn a_run_spanning_the_container_boundary_comes_back_merged() {
        // croaring merges runs across the 65535/65536 container boundary — one run, not two.
        let mut b = Bitmap::new();
        b.add_range(65_530..=65_540);
        assert_eq!(runs_of(&b, 0..100_000), vec![65_530..65_541]);
        assert_flattens_to_intersection(&b, 0..100_000);
    }

    #[test]
    fn runs_touching_the_range_ends_are_clamped_to_it() {
        let mut b = Bitmap::new();
        b.add_range(0..=9);
        b.add(15);
        b.add_range(20..=29);
        // The first run starts before r (the cursor seek supplies the start, no clamp code), the
        // last extends past r.end (the end-clamp cuts it).
        assert_eq!(runs_of(&b, 5..25), vec![5..10, 15..16, 20..25]);
        assert_flattens_to_intersection(&b, 5..25);
    }

    #[test]
    // The inverted range is the guard's own input, not a mistaken iteration bound.
    #[allow(clippy::reversed_empty_ranges)]
    fn empty_mask_empty_range_and_no_overlap_all_yield_nothing() {
        let empty = Bitmap::new();
        assert!(runs_of(&empty, 0..1000).is_empty());

        let mut b = Bitmap::new();
        b.add_range(100..=200);
        assert!(runs_of(&b, 50..50).is_empty(), "empty range");
        assert!(runs_of(&b, 60..40).is_empty(), "inverted range");
        assert!(
            runs_of(&b, 0..100).is_empty(),
            "range wholly before the run"
        );
        assert!(
            runs_of(&b, 201..300).is_empty(),
            "range wholly after the run"
        );
    }

    #[test]
    fn more_runs_than_one_buffer_read_are_all_emitted() {
        // 200 single-value runs forces multiple `read_many_ranges` refills (RUN_BUF = 64).
        let mut b = Bitmap::new();
        for i in 0..200u32 {
            b.add(i * 3);
        }
        let runs = runs_of(&b, 0..600);
        assert_eq!(runs.len(), 200);
        assert_flattens_to_intersection(&b, 0..600);
    }

    #[test]
    fn run_walk_agrees_with_the_bitmap_route_on_random_container_mixes() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xB9_0B9);
        for _ in 0..50 {
            let mut b = Bitmap::new();
            // A mix that lands array, bitmap and run containers: sparse randoms, a dense random
            // stretch, and a long literal run.
            for _ in 0..rng.gen_range(0..200) {
                b.add(rng.gen_range(0..300_000));
            }
            let dense_start = rng.gen_range(0..200_000);
            for v in dense_start..dense_start + 40_000 {
                if rng.gen_bool(0.5) {
                    b.add(v);
                }
            }
            let run_start = rng.gen_range(0..250_000);
            b.add_range(run_start..=run_start + rng.gen_range(0..30_000));

            for _ in 0..20 {
                let a = rng.gen_range(0..320_000);
                let z = rng.gen_range(0..320_000);
                assert_flattens_to_intersection(&b, a.min(z)..a.max(z));
            }
        }
    }
}
