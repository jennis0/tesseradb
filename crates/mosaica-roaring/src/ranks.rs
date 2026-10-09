//! A member's rank in a bitmap in constant time.
//!
//! croaring's `rank` sums the cardinality of every container below the value, so its cost grows
//! with the bitmap. [`BlockRanks`] holds, for each block of 2¹⁶ from the first member's to the
//! last's, how many members lie below it, and adds the rank within the value's own block, one
//! container's count. Its memory is 8 bytes a block of the bitmap's span, 512 KiB at the `u32`
//! ceiling.

use croaring::Bitmap;

/// How many members of one bitmap lie below each of its blocks.
#[derive(Debug, Clone, Default)]
pub struct BlockRanks {
    /// The first block holding a member.
    first: u32,
    /// Per block from `first` to the last member's, the members below it, then the total.
    below: Vec<u64>,
}

impl BlockRanks {
    /// The ranks of `bitmap`'s blocks, which the lookups must be asked of.
    pub fn of(bitmap: &Bitmap) -> BlockRanks {
        let (Some(lo), Some(hi)) = (bitmap.minimum(), bitmap.maximum()) else {
            return BlockRanks {
                first: 0,
                below: vec![0],
            };
        };
        let (lo, hi) = (lo >> 16, hi >> 16);
        let mut below = Vec::with_capacity((hi - lo) as usize + 2);
        let mut total = 0u64;
        for block in lo..=hi {
            below.push(total);
            total += bitmap.range_cardinality(block << 16..=(block << 16) | 0xFFFF);
        }
        below.push(total);
        BlockRanks { first: lo, below }
    }

    /// How many members of `bitmap`, the bitmap these ranks were taken of, lie below `value`,
    /// where `value` is a member; `None` where it is not.
    #[inline]
    pub fn rank_of(&self, bitmap: &Bitmap, value: u32) -> Option<u64> {
        if !bitmap.contains(value) {
            return None;
        }
        let below = self.below[((value >> 16) - self.first) as usize];
        Some(below + bitmap.range_cardinality((value & !0xFFFF)..value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rank is croaring's, less one for the member itself, across blocks and container kinds.
    #[test]
    fn a_rank_is_the_members_below() {
        let mut bitmap = Bitmap::new();
        bitmap.add_range(70_000..140_000);
        bitmap.add_many(&[5, 9, 1 << 20, (1 << 20) + 3, (1 << 20) + 5_000, u32::MAX]);
        for value in (300_000..400_000).step_by(3) {
            bitmap.add(value);
        }
        let mut optimised = bitmap.clone();
        optimised.run_optimize();
        for bitmap in [bitmap, optimised] {
            let ranks = BlockRanks::of(&bitmap);
            for value in bitmap.iter().step_by(97).chain([u32::MAX, 70_000, 139_999]) {
                assert_eq!(
                    ranks.rank_of(&bitmap, value),
                    Some(bitmap.rank(value) - 1),
                    "{value}"
                );
            }
            assert_eq!(ranks.rank_of(&bitmap, 6), None);
        }
        assert_eq!(
            BlockRanks::of(&Bitmap::new()).rank_of(&Bitmap::new(), 3),
            None
        );
    }

    /// The table spans the bitmap's own blocks, from its first member's to its last's.
    #[test]
    fn the_table_spans_the_bitmaps_own_blocks() {
        let first = 40u32 << 16;
        let bitmap = Bitmap::of(&[first + 7, first + 3 * 65_536 - 1]);
        let ranks = BlockRanks::of(&bitmap);
        assert_eq!((ranks.first, ranks.below.len()), (40, 4));
    }
}
