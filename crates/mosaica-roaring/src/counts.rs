//! How many members of one bitmap lie in each of many ranges, in one walk of its containers.
//!
//! croaring's `range_cardinality` takes two ranks, and a rank inside a bitset container is a
//! popcount from the container's start, so a range of four rows costs what a range of the whole
//! container does, and N adjacent ranges in one container popcount it 2N times. [`count_ranges`]
//! ranks every range's two ends in one call to CRoaring's `roaring_bitmap_rank_many`, which walks
//! the containers once and carries the rank forward inside each, so the ranges that share a
//! container popcount it once between them. croaring does not wrap that call, so it is made
//! through `croaring-sys`, which names the same `roaring_bitmap_t` croaring's `Bitmap` wraps.
//!
//! `rank_many` also adds up the cardinality of every container below the first value it ranks,
//! which on a bitmap of 2³² rows is tens of thousands of containers for each call. The call is
//! therefore made on a view of only the containers the ranges reach: the same `roaring_array_t`
//! with its pointers moved to the first such container and its size cut to their number. Its ranks
//! count from that container, and a range's count is the difference of two of them.

use std::ops::Range;

use croaring::Bitmap;

// `Bitmap` is `repr(transparent)` over `roaring_bitmap_t`, which is what makes the pointer cast in
// `rank_many` sound. A croaring release that changed that would fail here before it misread one.
const _: () =
    assert!(std::mem::size_of::<Bitmap>() == std::mem::size_of::<croaring_sys::roaring_bitmap_t>());

/// The number of members of `bitmap` in each of `ranges`, in order: [`RangeEnds::counts`] over
/// [`RangeEnds::of`] `ranges`.
pub fn count_ranges(bitmap: &Bitmap, ranges: &[Range<u32>]) -> Vec<u64> {
    RangeEnds::of(ranges).counts(bitmap)
}

/// The ends of many ranges, sorted once and counted against as many bitmaps as need them.
///
/// Ranges in ascending order that do not overlap, which is how a request's tiles and a tile's
/// cells come out of a segment, are counted in one walk. Any other order is sorted first. An empty
/// range counts 0.
#[derive(Debug, Clone)]
pub struct RangeEnds {
    /// Each non-empty range's `start − 1`, where it does not start at 0, and its `end − 1`,
    /// ascending.
    ends: Vec<u32>,
    /// Where each range's two ends are in `ends`: none for an empty range, and no start for a range
    /// from 0.
    places: Vec<[Option<usize>; 2]>,
    /// Where the ranks count from ([`rank_many`]): 0 where a range starts at 0, which counts by its
    /// end's rank alone, and otherwise the lowest end.
    from: u32,
}

impl RangeEnds {
    pub fn of(ranges: &[Range<u32>]) -> RangeEnds {
        // Each end with its slot: twice its range's index, plus one for the range's end.
        let mut keyed: Vec<(u32, usize)> = Vec::with_capacity(2 * ranges.len());
        let mut from_zero = false;
        for (i, r) in ranges.iter().enumerate().filter(|(_, r)| r.start < r.end) {
            from_zero |= r.start == 0;
            if r.start > 0 {
                keyed.push((r.start - 1, 2 * i));
            }
            keyed.push((r.end - 1, 2 * i + 1));
        }
        keyed.sort_unstable();
        let mut ends = Vec::with_capacity(keyed.len());
        let mut places = vec![[None; 2]; ranges.len()];
        for (place, (end, slot)) in keyed.into_iter().enumerate() {
            ends.push(end);
            places[slot / 2][slot % 2] = Some(place);
        }
        let from = match ends.first() {
            Some(&lowest) if !from_zero => lowest,
            _ => 0,
        };
        RangeEnds { ends, places, from }
    }

    /// The number of members of `bitmap` in each range, in the order the ranges were given.
    pub fn counts(&self, bitmap: &Bitmap) -> Vec<u64> {
        let ranks = rank_many(bitmap, &self.ends, self.from);
        self.places
            .iter()
            .map(|place| match *place {
                [start, Some(end)] => ranks[end] - start.map_or(0, |start| ranks[start]),
                _ => 0,
            })
            .collect()
    }
}

/// For each of `values`, ascending, how many members of `bitmap` are at or below it and in or
/// after the container that holds or follows `from`, which is at most `values[0]`. The difference
/// of two of these is the difference of the two ranks, and with `from` 0 each is the rank itself.
fn rank_many(bitmap: &Bitmap, values: &[u32], from: u32) -> Vec<u64> {
    debug_assert!(values.is_sorted());
    let mut ranks = vec![0u64; values.len()];
    let (Some(&first), Some(&last)) = (values.first(), values.last()) else {
        return ranks;
    };
    debug_assert!(from <= first);
    let raw = (bitmap as *const Bitmap).cast::<croaring_sys::roaring_bitmap_t>();
    // SAFETY: `Bitmap` is `repr(transparent)` over `roaring_bitmap_t` (its size is asserted above),
    // so `raw` points at the bitmap's own `roaring_array_t` for as long as `bitmap` is borrowed.
    // Its `keys`, `containers` and `typecodes` each hold `size` entries, read only once `size` is
    // known to be positive. The view shares those arrays from `lo` for `hi − lo` entries, which
    // lie inside them, and lives on this stack frame: nothing frees, grows or writes it.
    // `roaring_bitmap_rank_many` reads the view and `values.len()` values, which it requires
    // ascending, and writes one rank for each into `ranks`, which holds as many.
    unsafe {
        let whole = &(*raw).high_low_container;
        let size = usize::try_from(whole.size).unwrap_or(0);
        if size == 0 {
            return ranks;
        }
        let keys = std::slice::from_raw_parts(whole.keys, size);
        let lo = keys.partition_point(|&k| u32::from(k) < from >> 16);
        let hi = keys.partition_point(|&k| u32::from(k) <= last >> 16);
        let span = (hi - lo) as i32;
        let view = croaring_sys::roaring_bitmap_t {
            high_low_container: croaring_sys::roaring_array_t {
                size: span,
                allocation_size: span,
                containers: whole.containers.add(lo),
                keys: whole.keys.add(lo),
                typecodes: whole.typecodes.add(lo),
                flags: whole.flags,
            },
        };
        croaring_sys::roaring_bitmap_rank_many(
            &view,
            values.as_ptr(),
            values.as_ptr().add(values.len()),
            ranks.as_mut_ptr(),
        );
    }
    ranks
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    /// Every container kind, a run that crosses a container boundary, the last value of the
    /// space, and a run-optimised copy of the same members.
    fn bitmaps() -> Vec<Bitmap> {
        let mut rng = StdRng::seed_from_u64(7);
        let mut b = Bitmap::new();
        for v in (0..200_000u32).step_by(3) {
            b.add(v);
        }
        b.add_range(300_000..400_000);
        for _ in 0..50_000 {
            b.add(rng.gen_range(1_000_000..1_200_000));
        }
        b.add_many(&[5_000_000, u32::MAX - 1, u32::MAX]);
        let mut optimised = b.clone();
        optimised.run_optimize();
        vec![b, optimised, Bitmap::new()]
    }

    /// Ascending ranges, adjacent ones, empty ones, one starting at 0 and one ending at the last
    /// value: each count equals croaring's own.
    #[test]
    fn counts_equal_croarings_in_order() {
        let mut ranges: Vec<Range<u32>> = vec![0..1, 1..1, 1..70_000];
        let mut start = 70_000u32;
        for len in [1u32, 7, 64, 4_000, 65_536, 130_000, 3] {
            ranges.push(start..start + len);
            start += len;
        }
        for cell in (1_000_000u32..1_200_000).step_by(4_096) {
            ranges.push(cell..cell + 4_096);
        }
        ranges.push(4_000_000..u32::MAX);
        for b in bitmaps() {
            let want: Vec<u64> = ranges
                .iter()
                .map(|r| b.range_cardinality(r.clone()))
                .collect();
            assert_eq!(count_ranges(&b, &ranges), want);
        }
    }

    /// Ranges out of order and overlapping are counted as croaring counts them.
    #[test]
    fn counts_equal_croarings_in_any_order() {
        let mut rng = StdRng::seed_from_u64(11);
        let ranges: Vec<Range<u32>> = (0..2_000)
            .map(|_| {
                let start = rng.gen_range(0..1_300_000);
                start..start + rng.gen_range(0..90_000)
            })
            .collect();
        for b in bitmaps() {
            let want: Vec<u64> = ranges
                .iter()
                .map(|r| b.range_cardinality(r.clone()))
                .collect();
            assert_eq!(count_ranges(&b, &ranges), want);
        }
    }

    /// Ranges that reach only some containers of a bitmap spanning many, including none at all,
    /// the first and the last, and ranges from 0 that end past the first container, beside the
    /// range after them, beside later ones and out of order: the counts are croaring's.
    #[test]
    fn counts_over_a_part_of_a_wide_bitmap_equal_croarings() {
        let mut b = Bitmap::new();
        for block in (0..2_000u32).step_by(3) {
            b.add_range(block << 16..(block << 16) + 5_000);
            b.add((block << 16) + 40_000 + block);
        }
        let past = 2_000u32 << 16;
        for ranges in [
            vec![
                (1_000 << 16) + 10..(1_000 << 16) + 20,
                (1_000 << 16) + 20..(1_001 << 16) + 9,
            ],
            vec![0..3, 3..70_000],
            vec![
                (1_998 << 16)..(1_999 << 16) + 70,
                (1_999 << 16) + 70..u32::MAX,
            ],
            vec![
                (1 << 16) + 6_000..(1 << 16) + 7_000,
                (4 << 16) + 6_000..(4 << 16) + 7_000,
            ],
            vec![past..past + 100, past + 100..u32::MAX],
            vec![0..(1_500 << 16) + 41_000, (1_500 << 16) + 41_000..past],
            vec![
                0..(3 << 16) + 2,
                (1_200 << 16)..(1_200 << 16) + 9,
                (1_201 << 16)..past,
            ],
            vec![(1_200 << 16)..(1_200 << 16) + 9, 0..(3 << 16) + 2],
            vec![
                (600 << 16) + 39_000..(1_400 << 16) + 41_000,
                (1_400 << 16) + 41_000..(1_400 << 16) + 41_000,
            ],
        ] {
            let want: Vec<u64> = ranges
                .iter()
                .map(|r| b.range_cardinality(r.clone()))
                .collect();
            assert_eq!(count_ranges(&b, &ranges), want, "{ranges:?}");
        }
    }

    #[test]
    fn no_ranges_count_nothing() {
        assert!(count_ranges(&bitmaps()[0], &[]).is_empty());
    }
}
