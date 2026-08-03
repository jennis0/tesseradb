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

use crate::wal::WalRecord;

/// The entity-ID space's ceiling (contracts §2.6): `bundle_format = 1`
/// narrows every entity ID to `u32`, and `IdentityKey::forward`'s checked conversion refuses
/// any entity at or above this bound. The allocator refusing first is what makes that
/// conversion's error unreachable in practice rather than a rare, hard-to-reach corruption
/// path: without this cap, "collision-free by construction" rested entirely on the corpus
/// happening to stay small, and nothing enforced it.
const ENTITY_ID_CEILING: u64 = u32::MAX as u64;

/// Allocator errors. The batch has no effect when this is returned — `allocate` does not
/// advance `high_water` on the error path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// Issuing the requested range would hand out an ID at or above [`ENTITY_ID_CEILING`].
    /// `high_water` is the allocator's state at the time of refusal (unchanged by the call).
    Exhausted { high_water: u64 },
    /// [`Allocator::try_new`]'s seed already meets or exceeds the ceiling — caught at open
    /// rather than at the first ingest, so a corrupt or hand-edited manifest high-water fails
    /// closed immediately instead of silently colliding on the first allocation.
    SeedAtCeiling { high_water: u64 },
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocError::Exhausted { high_water } => write!(
                f,
                "entity-ID space exhausted: allocating from high-water {high_water} would issue \
                 an ID at or above u32::MAX ({ENTITY_ID_CEILING})"
            ),
            AllocError::SeedAtCeiling { high_water } => write!(
                f,
                "allocator seed {high_water} is already at or above u32::MAX ({ENTITY_ID_CEILING}); \
                 refusing to open rather than collide on the first allocation"
            ),
        }
    }
}

impl std::error::Error for AllocError {}

/// Monotone, never-reusing entity-ID allocator (I9).
pub struct Allocator {
    high_water: u64,
}

impl Allocator {
    /// Seeds the allocator at `high_water` — the caller's `max(manifest_hw, replayed rows/leases)`.
    /// Unchecked: prefer [`Allocator::try_new`] wherever the seed has not already been validated,
    /// since this constructor will happily seed at or above the ceiling and let the first
    /// `allocate` refuse instead.
    pub fn new(high_water: u64) -> Self {
        Allocator { high_water }
    }

    /// [`Allocator::new`], refusing a seed at or above [`ENTITY_ID_CEILING`] — the check that
    /// belongs at open, so a manifest high-water that already exceeds the bound is caught before
    /// any ingest is attempted rather than surfacing as an opaque exhaustion error later.
    pub fn try_new(high_water: u64) -> Result<Self, AllocError> {
        if high_water >= ENTITY_ID_CEILING {
            return Err(AllocError::SeedAtCeiling { high_water });
        }
        Ok(Allocator { high_water })
    }

    /// Allocates `n` consecutive, never-before-issued entity IDs and advances the high-water
    /// mark past them. Refuses — leaving `high_water` unchanged — if any ID in the range would
    /// be at or above [`ENTITY_ID_CEILING`]: a truncating allocation past
    /// `u32::MAX` is exactly what would make "collision-free by construction" false.
    pub fn allocate(&mut self, n: u64) -> Result<Range<u64>, AllocError> {
        let lo = self.high_water;
        let hi = lo + n;
        if hi > ENTITY_ID_CEILING {
            return Err(AllocError::Exhausted { high_water: lo });
        }
        self.high_water = hi;
        Ok(lo..hi)
    }

    /// The next ID this allocator will hand out.
    pub fn high_water(&self) -> u64 {
        self.high_water
    }
}

/// Computes the entity-ID high-water mark implied by a set of replayed WAL records: the maximum
/// of every `Lease.hi` and every `WalRow.entity_id.raw() + 1`.
///
/// This is the "replayed rows/leases" half of `Allocator::new`'s `max(manifest_hw, replayed
/// rows/leases)` seeding contract — callers should rebuild with
/// `Allocator::new(manifest_hw.max(high_water_from(&replayed)))` rather than carrying a
/// pre-crash `Allocator::high_water()` value across a restart, since the allocator itself does
/// not persist: only what actually made it into the WAL (or the bundle manifest) did.
pub fn high_water_from(records: &[WalRecord]) -> u64 {
    let mut hw = 0u64;
    for rec in records {
        match rec {
            WalRecord::Lease { hi, .. } => {
                if *hi > hw {
                    hw = *hi;
                }
            }
            WalRecord::IngestBatch { rows, .. } => {
                for row in rows {
                    let candidate = row.entity_id.raw() + 1;
                    if candidate > hw {
                        hw = candidate;
                    }
                }
            }
            // An overlay snapshot names entities that were certainly allocated, so it raises the
            // floor — but only for entities something has *denied*, which is a weak bound and not
            // the mechanism. Rotation deletes the `Lease` and `IngestBatch` records this function
            // really derives from; what replaces them is the side-manifest's own high-water mark
            // (⊘ not built — Task 15), never the build `MANIFEST.json`, which would reallocate
            // every flushed entity id and violate I9.
            WalRecord::OverlaySnapshot { entries } => {
                for entry in entries {
                    let candidate = entry.entity_id.raw() + 1;
                    if candidate > hw {
                        hw = candidate;
                    }
                }
            }
            WalRecord::Change { .. } => {}
        }
    }
    hw
}

/// One item awaiting entity-ID assignment at serve time (append ingest).
///
/// `terms` are already-**resolved** `TermId`s — bundle-relative ordinals for descriptors the
/// dictionary already knows, extended with the deterministic in-memory ordinals a session
/// assigns to novel descriptors (see `tessera_lifecycle::wal`'s module docs). Resolution happens
/// in the caller, not here, so this module stays free of the dictionary/interning machinery.
pub struct PendingItem {
    /// Optional (contracts §3.4 r6): `None` when the caller supplied no external id, in which
    /// case the item is addressable only by its `tessera_id`. Used here only as (part of) the
    /// tie-break in [`assign_sorted`]'s sort key — `None` sorts before every `Some`, which is
    /// fine because it is only a tie-break within an already-equal signature, never itself a
    /// visibility-bearing order.
    pub external_id: Option<Vec<u8>>,
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
///
/// Fallible: propagates [`AllocError::Exhausted`] from the underlying
/// `Allocator::allocate` rather than swallowing it — a batch that would exhaust the entity-ID
/// space has no effect, exactly as `allocate` leaves `high_water` unchanged on that error.
pub fn assign_sorted(items: &mut [PendingItem], alloc: &mut Allocator) -> Result<(), AllocError> {
    // Compute each item's (signature, external_id) sort key once, up front, rather than inside
    // the comparator — `sort_by`'s comparator can be called O(n log n) times, and
    // `signature_sort_key` allocates, so recomputing it per-comparison would be O(n log n)
    // allocations instead of O(n).
    let mut order: Vec<(usize, Vec<u32>)> = items
        .iter()
        .enumerate()
        .map(|(i, item)| (i, signature_sort_key(&item.terms)))
        .collect();
    order.sort_by(|(a, ka), (b, kb)| {
        ka.cmp(kb)
            .then_with(|| items[*a].external_id.cmp(&items[*b].external_id))
    });

    let ids = alloc.allocate(items.len() as u64)?;
    for (rank, (idx, _)) in order.into_iter().enumerate() {
        items[idx].entity_id = Some(EntityId::new(ids.start + rank as u64));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_is_monotone_and_never_reuses() {
        let mut alloc = Allocator::new(10);
        let a = alloc.allocate(3).unwrap();
        let b = alloc.allocate(5).unwrap();
        assert_eq!(a, 10..13);
        assert_eq!(b, 13..18);
        assert_eq!(alloc.high_water(), 18);
    }

    #[test]
    fn the_allocator_refuses_to_issue_an_id_at_or_above_u32_max() {
        // Without the cap, `allocate` would be `lo + n` on a u64 and "collision-free by
        // construction" would rest on the corpus happening to stay small. Past
        // 2^32 two entities would share a tessera_id and `invert` would return the WRONG one.
        let mut a = Allocator::new(u32::MAX as u64 - 2);
        assert!(a.allocate(1).is_ok());
        assert!(matches!(a.allocate(10), Err(AllocError::Exhausted { .. })));
        // The failed call must not have moved the high-water mark (the batch has no effect).
        assert_eq!(a.high_water(), u32::MAX as u64 - 1);

        // And the seed itself: a manifest high-water past the bound is refused at open, not
        // silently carried into the first ingest.
        assert!(Allocator::try_new(1u64 << 33).is_err());
        assert!(Allocator::try_new(u32::MAX as u64).is_err());
        assert!(Allocator::try_new(u32::MAX as u64 - 1).is_ok());
    }

    #[test]
    fn signature_sort_key_matches_tessera_build_semantics() {
        // Same three-line rule as tessera_build::signature_sort_key: sorted, deduplicated.
        let key = signature_sort_key(&[TermId::new(9), TermId::new(2), TermId::new(2)]);
        assert_eq!(key, vec![2, 9]);
    }

    fn row(entity_id: u64) -> WalRecord {
        WalRecord::IngestBatch {
            batch_id: "b".into(),
            body_hash: [0u8; 32],
            rows: vec![crate::wal::WalRow {
                external_id: Some(entity_id.to_le_bytes().to_vec()),
                entity_id: EntityId::new(entity_id),
                slice: "default".to_string(),
                descriptors: Vec::new(),
                x: 0.0,
                y: 0.0,
                scalars: Vec::new(),
            }],
        }
    }

    #[test]
    fn high_water_from_takes_the_max_of_rows_and_leases() {
        assert_eq!(high_water_from(&[]), 0);
        // A row with entity_id 5 means IDs 0..=5 are taken, so the next free ID is 6.
        assert_eq!(high_water_from(&[row(5)]), 6);
        assert_eq!(
            high_water_from(&[row(5), WalRecord::Lease { lo: 6, hi: 20 }]),
            20
        );
        // A lease lower than an already-seen row must not pull the high-water mark backwards.
        assert_eq!(
            high_water_from(&[WalRecord::Lease { lo: 6, hi: 20 }, row(3)]),
            20
        );
        // Change records carry no entity-ID information.
        assert_eq!(
            high_water_from(&[WalRecord::Change {
                external_id: vec![1],
                op: crate::wal::ChangeOp::Delete,
                descriptors: None,
            }]),
            0
        );
    }
}
