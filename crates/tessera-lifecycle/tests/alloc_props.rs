//! I9 allocator property tests: monotonicity, no reuse across simulated
//! crashes, and `assign_sorted`'s contiguous-signature grouping.

use std::collections::HashSet;

use proptest::prelude::*;

use tessera_lifecycle::alloc::{assign_sorted, high_water_from, Allocator, PendingItem};
use tessera_lifecycle::wal::{WalRecord, WalRow};
use tessera_types::{EntityId, TermId};

proptest! {
    /// Interleaved `allocate` calls on a single allocator never overlap and always advance the
    /// high-water mark by exactly the amount requested.
    #[test]
    fn allocate_is_strictly_monotone(sizes in prop::collection::vec(1u64..64, 1..60)) {
        let mut alloc = Allocator::new(0);
        let mut expected_next = 0u64;
        let mut seen: HashSet<u64> = HashSet::new();

        for n in sizes {
            let range = alloc.allocate(n).unwrap();
            prop_assert_eq!(range.start, expected_next);
            prop_assert_eq!(range.end, expected_next + n);
            for id in range.clone() {
                prop_assert!(seen.insert(id), "id {} allocated twice in one session", id);
            }
            expected_next = range.end;
            prop_assert_eq!(alloc.high_water(), expected_next);
        }
    }

    /// Simulates repeated crashes against a *replayed* WAL, not a carried-over `high_water()`:
    /// each "session" allocates some ranges, records one `WalRow` per allocated ID (as the real
    /// system would once each row is fsynced) into a log standing in for the on-disk WAL, then
    /// the allocator is dropped (the crash). The next session rebuilds via
    /// `Allocator::new(manifest_hw.max(high_water_from(&wal_records)))` — the actual seeding
    /// contract (`Allocator::high_water()` itself is not durable; only what made it into the WAL
    /// is). No ID handed out in an earlier session may reappear in a later one.
    #[test]
    fn no_reuse_across_simulated_crashes(
        session_sizes in prop::collection::vec(prop::collection::vec(1u64..32, 1..10), 1..12),
    ) {
        // No bundle exists in this simulation, so `manifest_hw` is always 0 — the WAL's replayed
        // high-water mark is the only contributor. Spelled out as `max(manifest_hw, ...)` anyway
        // (with the redundant-with-0 lint silenced) because that full expression is the actual
        // production seeding contract this test exists to exercise, not just the degenerate case.
        let manifest_hw: u64 = 0;
        let mut wal_records: Vec<WalRecord> = Vec::new();
        let mut used: HashSet<u64> = HashSet::new();

        for sizes in session_sizes {
            // Rebuild exactly as a real restart would: from the bundle's manifest high-water
            // mark and whatever the (accumulated, "replayed") WAL log actually contains.
            #[allow(clippy::unnecessary_min_or_max)]
            let seed = manifest_hw.max(high_water_from(&wal_records));
            let mut alloc = Allocator::new(seed);

            for n in sizes {
                let range = alloc.allocate(n).unwrap();
                for id in range {
                    prop_assert!(!used.contains(&id), "id {} reused across a simulated crash", id);
                    used.insert(id);
                    wal_records.push(WalRecord::IngestBatch {
                        batch_id: format!("batch-{id}"),
                        body_hash: [0u8; 32],
                        rows: vec![WalRow {
                            external_id: Some(id.to_le_bytes().to_vec()),
                            entity_id: EntityId::new(id),
                            descriptors: Vec::new(),
                            x: 0.0,
                            y: 0.0,
                            scalars: Vec::new(),
                        }],
                    });
                }
            }
            // `alloc` is dropped here — the simulated crash. `wal_records` persists, standing in
            // for durable, already-fsynced WAL content survived from disk.
        }
    }

    /// `assign_sorted` gives every item a distinct ID from a dense range, and items that share a
    /// signature (sorted, deduplicated term-ID list) land on a contiguous run of IDs.
    #[test]
    fn assign_sorted_groups_identical_signatures_contiguously(
        signatures in prop::collection::vec(prop::collection::vec(0u32..8, 0..5), 1..50),
    ) {
        let mut items: Vec<PendingItem> = signatures
            .iter()
            .enumerate()
            .map(|(i, sig)| PendingItem {
                external_id: Some(format!("ext-{i:05}").into_bytes()),
                terms: sig.iter().map(|&t| TermId::new(t)).collect(),
                entity_id: None,
            })
            .collect();

        let mut alloc = Allocator::new(0);
        assign_sorted(&mut items, &mut alloc).unwrap();

        let n = items.len() as u64;
        let mut ids: Vec<u64> = items
            .iter()
            .map(|it| it.entity_id.expect("assign_sorted must fill every entity_id").raw())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        prop_assert_eq!(ids, (0..n).collect::<Vec<_>>(), "ids are not a dense, distinct 0..n range");

        // Walk items in assigned-ID order; each time the (sorted, deduplicated) signature
        // changes, it must never recur — otherwise two runs of the same signature exist.
        let mut by_id: Vec<&PendingItem> = items.iter().collect();
        by_id.sort_by_key(|it| it.entity_id.unwrap().raw());

        let mut closed_signatures: HashSet<Vec<u32>> = HashSet::new();
        let mut current: Option<Vec<u32>> = None;
        for it in &by_id {
            let mut sig: Vec<u32> = it.terms.iter().map(|t| t.raw()).collect();
            sig.sort_unstable();
            sig.dedup();

            if current.as_ref() != Some(&sig) {
                if let Some(prev) = current.take() {
                    closed_signatures.insert(prev);
                }
                prop_assert!(
                    !closed_signatures.contains(&sig),
                    "signature {:?} reappeared after a different signature — not contiguous",
                    sig
                );
                current = Some(sig);
            }
        }
    }
}
