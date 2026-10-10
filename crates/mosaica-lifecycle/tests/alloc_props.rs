//! Allocator property tests: monotonicity without freed ids, the order a window's ids are taken
//! in, no id live twice and no identifier naming two items across simulated crashes and folds that
//! free ids and numbers, and `assign_sorted`'s contiguous-signature grouping.

use std::collections::{BTreeMap, HashMap, HashSet};

use proptest::prelude::*;

use croaring::Bitmap;
use mosaica_lifecycle::alloc::{
    allocator_floor, assign_sorted, entities_named, high_water_from, union_of, Allocator,
    ByTenancy, PendingItem,
};
use mosaica_lifecycle::wal::{ChangeOp, WalRecord, WalRow};
use mosaica_types::{EntityId, Tenancy, TermId};

proptest! {
    /// Interleaved `allocate` calls on a single allocator never overlap and always advance the
    /// high-water mark by exactly the amount requested.
    #[test]
    fn allocate_is_strictly_monotone(sizes in prop::collection::vec(1u64..64, 1..60)) {
        let mut alloc = Allocator::new(0);
        let mut expected_next = 0u64;
        let mut seen: HashSet<u64> = HashSet::new();

        for n in sizes {
            let ids = alloc.allocate(0, n).unwrap().items;
            prop_assert_eq!(&ids, &(expected_next..expected_next + n).collect::<Vec<_>>());
            for id in ids {
                prop_assert!(seen.insert(id), "id {} allocated twice in one session", id);
            }
            expected_next += n;
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
                let range = alloc.allocate(0, n).unwrap().items;
                for id in range {
                    prop_assert!(!used.contains(&id), "id {} reused across a simulated crash", id);
                    used.insert(id);
                    wal_records.push(WalRecord::IngestBatch {
                        edits: Vec::new(),
                        receipt: Vec::new(),
                        batch_id: format!("batch-{id}"),
                        body_hash: [0u8; 32],
                        rows: vec![WalRow {
                            entity_id: EntityId::new(id),
                            view: "default".to_string(),
                            join: false,
                            descriptors: Vec::new(),
                            x: 0.0,
                            y: 0.0,
                            scalars: Vec::new(),
                            scoped: Vec::new(),
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
    /// `mosaica-engine`'s `the_watermark_and_high_water_published_are_the_live_ones_not_the_snapshot`
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

            for id in alloc.allocate(0, n).unwrap().items {
                prop_assert!(!used.contains(&id), "entity id {} reissued", id);
                used.insert(id);
                wal.push(WalRecord::IngestBatch {
                    edits: Vec::new(),
                    receipt: Vec::new(),
                    batch_id: format!("batch-{id}"),
                    body_hash: [0u8; 32],
                    rows: vec![WalRow {
                        entity_id: EntityId::new(id),
                        view: "default".to_string(),
                        join: false,
                        descriptors: Vec::new(),
                        x: 0.0,
                        y: 0.0,
                        scalars: Vec::new(),
                        scoped: Vec::new(),
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

    /// **One allocation takes ids in the allocator's order**, over a pool of freed ids at fuzzed
    /// tenancies: an edit's new entity takes tenancy 0, lowest first, then the high-water; a new
    /// item takes the lowest tenancy, lowest id first, then the high-water; the edits draw first.
    /// An allocation the space between the marks cannot hold is refused and takes nothing.
    #[test]
    fn a_window_takes_ids_in_the_allocators_order(
        pool in prop::collection::btree_map(0u32..200, 0u16..4, 0..40),
        edits in 0u64..30,
        items in 0u64..30,
        room in 0u64..60,
    ) {
        const HIGH_WATER: u64 = 1_000;
        let mut alloc = Allocator::with_marks(HIGH_WATER, HIGH_WATER + room);
        let mut free = ByTenancy::new();
        for (&id, &tenancy) in &pool {
            free.entry(Tenancy::new(tenancy).unwrap()).or_default().add(id);
        }
        alloc.release_after(0, free.clone());
        alloc.promote(0);

        let mut by_tenancy: Vec<(u16, u64)> =
            pool.iter().map(|(&id, &tenancy)| (tenancy, u64::from(id))).collect();
        by_tenancy.sort_unstable();
        let mut expected_edits: Vec<u64> = by_tenancy
            .iter()
            .filter(|(tenancy, _)| *tenancy == 0)
            .map(|(_, id)| *id)
            .take(edits as usize)
            .collect();
        by_tenancy.retain(|(_, id)| !expected_edits.contains(id));
        let mut expected_items: Vec<u64> =
            by_tenancy.iter().map(|(_, id)| *id).take(items as usize).collect();
        let edits_fresh = edits - expected_edits.len() as u64;
        let items_fresh = items - expected_items.len() as u64;
        expected_edits.extend(HIGH_WATER..HIGH_WATER + edits_fresh);
        expected_items.extend(HIGH_WATER + edits_fresh..HIGH_WATER + edits_fresh + items_fresh);

        match alloc.allocate(edits, items) {
            Ok(given) => {
                prop_assert!(edits_fresh + items_fresh <= room);
                prop_assert_eq!(given.edits, expected_edits);
                prop_assert_eq!(given.items, expected_items);
                prop_assert_eq!(alloc.high_water(), HIGH_WATER + edits_fresh + items_fresh);
            }
            Err(_) => {
                prop_assert!(edits_fresh + items_fresh > room);
                prop_assert_eq!(alloc.free(), &free, "a refused allocation takes no freed id");
                prop_assert_eq!(alloc.high_water(), HIGH_WATER);
            }
        }
    }

    /// **No id holds two items and no identifier names two**, across fuzzed rounds of windows
    /// creating items and editing them, deletions, folds that free the ids edits left at tenancy 0
    /// and deleted items' numbers one tenancy higher, rotations that reclaim the log, and crashes.
    /// A fold records the allocator's sets, each as one set of ids, beside the tenancy index; a
    /// restart splits them by that index and removes every id a kept log record names. An edit's
    /// new entity is never an id an item has held as its number.
    #[test]
    fn no_identifier_names_two_items_across_folds_that_free_numbers(
        rounds in prop::collection::vec(
            (0u64..6, 0usize..4, 0usize..3, any::<bool>(), any::<bool>(), any::<bool>()),
            1..32,
        ),
    ) {
        // Ids holding something: an item's entity, or a number its item still resolves through.
        let mut in_use: HashSet<u64> = HashSet::new();
        // Live items, as (number, entity).
        let mut items: Vec<(u64, u64)> = Vec::new();
        // What the next fold frees, each with whether it is an item's number.
        let mut gone: Vec<(u64, bool)> = Vec::new();
        let mut index: HashMap<u64, u16> = HashMap::new();
        let mut identifiers: HashSet<(u16, u64)> = HashSet::new();
        let mut published = Published::default();
        let mut wal: Vec<WalRecord> = Vec::new();
        let mut position = 0u64;
        let mut retained_from = 0u64;
        let mut alloc = Allocator::new(0);

        for (created, edits, deletes, fold, rotate, crash) in rounds {
            let edited = edits.min(items.len());
            let given = alloc.allocate(edited as u64, created).unwrap();
            for (at, &entity) in given.edits.iter().enumerate() {
                prop_assert_eq!(
                    index.get(&entity).copied().unwrap_or(0),
                    0,
                    "an edit took id {} above tenancy 0",
                    entity
                );
                prop_assert!(in_use.insert(entity), "entity id {} holds two items", entity);
                let (number, old) = items[at];
                if old != number {
                    gone.push((old, false));
                }
                items[at].1 = entity;
                position += 1;
                wal.push(row(entity));
                wal.push(deletion(old));
            }
            for &number in &given.items {
                prop_assert!(in_use.insert(number), "entity id {} holds two items", number);
                let tenancy = index.get(&number).copied().unwrap_or(0);
                prop_assert!(
                    identifiers.insert((tenancy, number)),
                    "the identifier of number {} at tenancy {} names two items",
                    number,
                    tenancy
                );
                items.push((number, number));
                position += 1;
                wal.push(row(number));
            }
            for _ in 0..deletes.min(items.len()) {
                let (number, entity) = items.pop().expect("an item to delete");
                if entity != number {
                    gone.push((entity, false));
                }
                gone.push((number, true));
                position += 1;
                wal.push(deletion(entity));
            }
            if fold {
                let mut freed = ByTenancy::new();
                for (id, number) in gone.drain(..) {
                    in_use.remove(&id);
                    let tenancy = if number {
                        let tenancy = index.entry(id).or_insert(0);
                        *tenancy += 1;
                        *tenancy
                    } else {
                        0
                    };
                    freed
                        .entry(Tenancy::new(tenancy).unwrap())
                        .or_default()
                        .add(id as u32);
                }
                alloc.release_after(position, freed);
                published = Published {
                    high_water: alloc.high_water(),
                    free: union_of(alloc.free()),
                    held: alloc.held().iter().map(|(p, ids)| (*p, union_of(ids))).collect(),
                    index: index.clone(),
                };
                if rotate {
                    wal.clear();
                    retained_from = position;
                    alloc.promote(retained_from);
                }
            }
            if crash {
                let split = |ids: &Bitmap| -> ByTenancy {
                    let mut sets: BTreeMap<Tenancy, Bitmap> = BTreeMap::new();
                    for id in ids.iter() {
                        let tenancy = published.index.get(&u64::from(id)).copied().unwrap_or(0);
                        sets.entry(Tenancy::new(tenancy).unwrap()).or_default().add(id);
                    }
                    sets
                };
                let mut next = Allocator::new(published.high_water.max(high_water_from(&wal)));
                next.seed_freed(
                    split(&published.free),
                    published.held.iter().map(|(p, ids)| (*p, split(ids))).collect(),
                    retained_from,
                    &entities_named(&wal),
                );
                alloc = next;
            }
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
            .map(|sig| PendingItem {
                terms: sig.iter().map(|&t| TermId::new(t)).collect(),
                entity_id: None,
                edit: false,
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

/// What the last fold published: the high-water, the allocator's sets as one set of ids each,
/// and the tenancy index.
#[derive(Default)]
struct Published {
    high_water: u64,
    free: Bitmap,
    held: Vec<(u64, Bitmap)>,
    index: HashMap<u64, u16>,
}

fn deletion(id: u64) -> WalRecord {
    WalRecord::ChangeBatch {
        changes: vec![(EntityId::new(id), ChangeOp::Delete)],
    }
}

fn row(id: u64) -> WalRecord {
    WalRecord::IngestBatch {
        edits: Vec::new(),
        receipt: Vec::new(),
        batch_id: format!("batch-{id}"),
        body_hash: [0u8; 32],
        rows: vec![WalRow {
            entity_id: EntityId::new(id),
            view: "default".to_string(),
            join: false,
            descriptors: Vec::new(),
            x: 0.0,
            y: 0.0,
            scalars: Vec::new(),
            scoped: Vec::new(),
        }],
    }
}
