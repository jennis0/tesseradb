//! I9 allocator property tests: monotonicity, no reuse across simulated
//! crashes, and `assign_sorted`'s contiguous-signature grouping.

use std::collections::HashSet;

use proptest::prelude::*;

use tessera_lifecycle::alloc::{
    allocator_floor, assign_sorted, high_water_from, Allocator, PendingItem,
};
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
                            view: "default".to_string(),
                            join: false,
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

    /// **No entity id is reissued after a fold** (I9; compaction §12's obligation 10), across
    /// fuzzed interleavings of ingest, flush, fold and crash.
    ///
    /// A fold is the one operation that makes the two durable high-water marks disagree *on
    /// purpose*. `publish_fold` writes the **snapshot's** entity bound into `MANIFEST.json`,
    /// because that field's other reader is the base locator's declared length and a live value
    /// there would claim every post-snapshot entity; it writes the **live** value into
    /// `SEGMENTS-<n>.json`. The rotation that follows then reclaims the WAL records the ids were
    /// really derived from. So after a fold every one of the three homes is individually wrong —
    /// the bundle manifest was lowered, the WAL was reclaimed — and only
    /// `alloc::allocator_floor`'s `max` recovers the floor.
    ///
    /// **What this covers, and what it does not.** Like
    /// `no_reuse_across_simulated_crashes` beside it, this is a model: it simulates the crash
    /// rather than crashing a process, and it exercises the composition rule at its real function
    /// rather than re-deriving it. The fold's *own* write — that `SEGMENTS-<n>.json` gets the live
    /// value and `MANIFEST.json` the snapshot's — is a fact about IO and is pinned end to end by
    /// `tessera-engine`'s `the_watermark_and_high_water_published_are_the_live_ones_not_the_snapshot`
    /// and `the_folded_manifests_high_water_is_the_snapshots_entity_space`. Obligation 10 needs
    /// both; neither alone establishes it.
    #[test]
    fn no_reuse_across_a_fold_that_lowers_the_bundles_high_water(
        rounds in prop::collection::vec((1u64..32, any::<bool>(), any::<bool>()), 1..14),
    ) {
        // The three durable homes, none of which is sufficient alone.
        let mut bundle_high_water = 0u64;       // MANIFEST.json
        let mut side_high_water = 0u64;         // SEGMENTS-<n>.json
        let mut wal: Vec<WalRecord> = Vec::new();
        let mut used: HashSet<u64> = HashSet::new();

        for (n, flush, fold) in rounds {
            // ---- restart, seeded exactly as `Engine::open` does ----------------------------
            let seed = allocator_floor(bundle_high_water, &[side_high_water])
                .max(high_water_from(&wal));
            let mut alloc = Allocator::new(seed);
            // A fold plans against the generation it opened on, so its snapshot bound is the
            // entity space as of *now* — before this round's ingests, which is what makes the
            // value it writes to `MANIFEST.json` strictly lower than the live one.
            let planned_bound = seed;

            for id in alloc.allocate(n).unwrap() {
                prop_assert!(!used.contains(&id), "entity id {} reissued", id);
                used.insert(id);
                wal.push(WalRecord::IngestBatch {
                    batch_id: format!("batch-{id}"),
                    body_hash: [0u8; 32],
                    rows: vec![WalRow {
                        external_id: Some(id.to_le_bytes().to_vec()),
                        entity_id: EntityId::new(id),
                        view: "default".to_string(),
                        join: false,
                        descriptors: Vec::new(),
                        x: 0.0,
                        y: 0.0,
                        scalars: Vec::new(),
                    }],
                });
            }

            if flush {
                // A flush publication raises the side-manifest past the ids it consumed.
                side_high_water = alloc.high_water();
            }
            if fold {
                bundle_high_water = planned_bound;
                side_high_water = alloc.high_water();
                // The rotation behind the fold reclaims the log. Faithful precisely *because*
                // the line above took the live value: it is what makes the reclaimed records
                // redundant, and a fold that wrote the snapshot's bound here instead would
                // reclaim ids nothing else records.
                wal.clear();
            }
            // `alloc` is dropped here — the crash.
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
