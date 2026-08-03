//! Merge selection: which segments a merge takes, and why adjacency is the first rule.
//!
//! Flush publishes one segment per tick. Without merge that is a serving cliff on two axes — a tile
//! resolves to one contiguous range **per live segment** (arch §11.3), and a fragment build unions
//! across **every live delta tier** — and a 90 s period produces roughly a thousand of each per day.
//!
//! **Selection is size-tiered over entity-adjacent segments, and adjacency is not an optimisation.**
//! A size-only policy is free to merge segments covering discontiguous entity sets; the result is a
//! row-space extent whose entity range interleaves with its neighbours', so the extent list
//! fragments monotonically and nothing but compaction can repair it. Requiring the inputs to be
//! adjacent keeps every merged extent a single contiguous entity range, which is the property
//! `RowSpace` is built on.
//!
//! **The cost of that rule, stated rather than hidden:** a large segment sitting between two small
//! ones blocks their merge. The alternative — merging across it — is what fragments the extent
//! list, so this is the trade taken deliberately, and [`MergePolicy::select`]'s doc says where it
//! shows up.
//!
//! **The base segment is excluded by the size bound, not by a rule.** A merge that swallowed the
//! base would be a legal row-space-only rewrite; the objection is that it pays compaction's entire
//! cost — a full permutation rewrite, up to 10⁹ rows of columns re-emitted — and banks none of
//! compaction's benefit. There is a sharper form: base files live in `MANIFEST.files`, so a merge
//! consuming the base must either leave them digested there with nothing referencing them, or write
//! a new prefix, at which point it *is* compaction under another name.
//!
//! **No deletes-percentage trigger.** `architecture.md` §11.3 lists tombstone reclamation as one of
//! three reasons to merge; it is not one here, because reclaiming a tombstoned row is a *fold*, and
//! a fold is invariant-bearing work that belongs to compaction. A merge that dropped rows would
//! have left this module.

use crate::manifest::SegmentDescriptor;

/// What a merge is allowed to take.
///
/// The three knobs are independent and each answers a different question: how many segments make a
/// merge worth doing, when two segments count as the same size, and how large a single merge may
/// get.
#[derive(Debug, Clone, Copy)]
pub struct MergePolicy {
    /// How many same-tier, adjacent segments select a merge. Below this, nothing is merged.
    pub tier_width: usize,
    /// Sizes at or below this compare **equal**, so a tail of tiny segments forms one tier rather
    /// than a ladder of singletons that never reaches `tier_width`.
    ///
    /// Without it, a deployment whose flushes vary in size by a few bytes produces a size class per
    /// flush and merges nothing at all — the failure is silent, and looks like a policy that is
    /// simply never triggered.
    pub segment_floor_bytes: u64,
    /// The largest total a single merge may produce. Bounds the pool time and the write
    /// amplification of one merge, and is what keeps the base segment out of selection.
    pub max_merged_segment_bytes: u64,
}

impl MergePolicy {
    /// The seg_ids a merge should take, or `None` if nothing qualifies.
    ///
    /// `sizes` is parallel to `segments`, in bytes. Both are in the manifest's listed order, which
    /// for row space is also entity order.
    ///
    /// **Three conditions, all of them necessary:**
    ///
    /// 1. **Adjacent in the list, with strictly increasing, non-overlapping entity ranges.** The
    ///    merged extent must be one contiguous entity range or the extent list fragments (see this
    ///    module's doc). Note this is *not* `hi + 1 == lo`: a deleted entity acquires no row, so a
    ///    flush's range legitimately has gaps, and requiring exact contiguity would stop merging
    ///    entirely on a deployment that deletes.
    /// 2. **The same size tier**, by power-of-two class over `max(size, segment_floor_bytes)`.
    /// 3. **Total within `max_merged_segment_bytes`.** This is where a large neighbour blocks a
    ///    merge, and where the base segment excludes itself.
    ///
    /// The **first** qualifying window in list order is taken rather than the best one. Merging is
    /// idempotent work on a cadence — whatever this leaves, the next tick reconsiders — so a
    /// search for the best window would buy a marginally better choice at the cost of a policy
    /// nobody can predict from the manifest.
    pub fn select(&self, segments: &[SegmentDescriptor], sizes: &[u64]) -> Option<Vec<String>> {
        if self.tier_width < 2 || segments.len() < self.tier_width || sizes.len() != segments.len()
        {
            return None;
        }

        for start in 0..=segments.len() - self.tier_width {
            let window = &segments[start..start + self.tier_width];
            let window_sizes = &sizes[start..start + self.tier_width];

            let adjacent = window
                .windows(2)
                .all(|pair| pair[0].entity_hi < pair[1].entity_lo);
            if !adjacent {
                continue;
            }

            let tier = self.tier_of(window_sizes[0]);
            if !window_sizes.iter().all(|s| self.tier_of(*s) == tier) {
                continue;
            }

            let total: u64 = window_sizes.iter().copied().sum();
            if total > self.max_merged_segment_bytes {
                continue;
            }

            return Some(window.iter().map(|s| s.seg_id.clone()).collect());
        }
        None
    }

    /// `size`'s tier: the power-of-two class of `max(size, floor)`.
    ///
    /// Clamping to the floor **before** taking the class is what makes the floor mean "these
    /// compare equal" rather than "these are skipped": two segments of 1 and 999 bytes against a
    /// 1,000-byte floor are one tier, which is the tail-of-tiny-segments case the floor exists for.
    fn tier_of(&self, size: u64) -> u32 {
        size.max(self.segment_floor_bytes).max(1).ilog2()
    }
}
