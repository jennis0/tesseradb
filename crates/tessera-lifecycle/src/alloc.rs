//! The I9 entity-ID allocator: monotone, never-reusing, seeded from durable state at boot.
//!
//! `Allocator::new` is always seeded from `max(manifest_hw, replayed rows/leases)` — the highest
//! entity ID any durable artefact (bundle manifest, or a replayed WAL row/lease) has ever
//! claimed. Because IDs are append-only and never reused (I9), and the signature-sorted
//! assignment from the first build is permanent (it cannot be re-sorted without invalidating
//! every posting, permutation and handle ever issued — see `tessera_build`'s module docs), this
//! seeding rule is the entire durability contract: as long as the seed never goes backwards
//! across a restart, no ID is ever handed out twice.

use std::ops::Range;

use tessera_types::{EntityId, TermId};

/// Monotone, never-reusing entity-ID allocator (I9).
pub struct Allocator {
    high_water: u64,
}

impl Allocator {
    /// Seeds the allocator at `high_water` — the caller's `max(manifest_hw, replayed rows/leases)`.
    pub fn new(high_water: u64) -> Self {
        Allocator { high_water }
    }

    /// Allocates `n` consecutive, never-before-issued entity IDs and advances the high-water
    /// mark past them.
    pub fn allocate(&mut self, n: u64) -> Range<u64> {
        let lo = self.high_water;
        let hi = lo + n;
        self.high_water = hi;
        lo..hi
    }

    /// The next ID this allocator will hand out.
    pub fn high_water(&self) -> u64 {
        self.high_water
    }
}

/// One item awaiting entity-ID assignment at serve time (append ingest).
///
/// `terms` are already-**resolved** `TermId`s — bundle-relative ordinals for descriptors the
/// dictionary already knows, extended with the deterministic in-memory ordinals a session
/// assigns to novel descriptors (see `tessera_lifecycle::wal`'s module docs). Resolution happens
/// in the caller, not here, so this module stays free of the dictionary/interning machinery.
pub struct PendingItem {
    pub external_id: Vec<u8>,
    pub terms: Vec<TermId>,
    /// Filled in by [`assign_sorted`]; `None` beforehand.
    pub entity_id: Option<EntityId>,
}

/// The signature-sorted assignment key (§11.1): an item's sorted, deduplicated term-ID list.
///
/// This is a deliberate duplicate of `tessera_build::signature_sort_key`, not an import: the
/// crate dependency direction runs `tessera-build → tessera-lifecycle` (the build pipeline will
/// depend on this crate's allocator), never the reverse, so importing from `tessera-build` here
/// would invert SA §3's crate graph. The two copies must stay in lockstep — this one operates on
/// resolved `TermId`s exactly as `tessera_build::signature_sort_key` does, and the logic is three
/// lines by design specifically so keeping them in sync is cheap.
fn signature_sort_key(terms: &[TermId]) -> Vec<u32> {
    let mut key: Vec<u32> = terms.iter().map(|t| t.raw()).collect();
    key.sort_unstable();
    key.dedup();
    key
}

/// Assigns sequential entity IDs to `items`, ordered by `(signature_sort_key, external_id)` —
/// the same total order the batch build uses (§11.1), so appended items interleave into the
/// permanent signature ordering rather than breaking it. Items with identical signatures land in
/// a contiguous ID run, which is what makes their postings compress as runs.
pub fn assign_sorted(items: &mut [PendingItem], alloc: &mut Allocator) {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        let ka = signature_sort_key(&items[a].terms);
        let kb = signature_sort_key(&items[b].terms);
        ka.cmp(&kb)
            .then_with(|| items[a].external_id.cmp(&items[b].external_id))
    });

    let ids = alloc.allocate(items.len() as u64);
    for (rank, idx) in order.into_iter().enumerate() {
        items[idx].entity_id = Some(EntityId::new(ids.start + rank as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_is_monotone_and_never_reuses() {
        let mut alloc = Allocator::new(10);
        let a = alloc.allocate(3);
        let b = alloc.allocate(5);
        assert_eq!(a, 10..13);
        assert_eq!(b, 13..18);
        assert_eq!(alloc.high_water(), 18);
    }

    #[test]
    fn signature_sort_key_matches_tessera_build_semantics() {
        // Same three-line rule as tessera_build::signature_sort_key: sorted, deduplicated.
        let key = signature_sort_key(&[TermId::new(9), TermId::new(2), TermId::new(2)]);
        assert_eq!(key, vec![2, 9]);
    }
}
