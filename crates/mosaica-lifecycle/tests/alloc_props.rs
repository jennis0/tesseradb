//! Allocator property tests: monotonicity without freed ids, the order a window's ids are taken
//! in, no id live twice and no identifier naming two items across simulated crashes and folds that
//! free ids and numbers, and `assign_sorted`'s contiguous-signature grouping.

use std::collections::{BTreeMap, HashMap, HashSet};

use proptest::prelude::*;

use croaring::Bitmap;
use mosaica_lifecycle::alloc::{
    assign_sorted, name_entities, raise_high_water, union_of, Allocator, Allocators, ByTenancy,
    PendingItem,
};
use mosaica_lifecycle::wal::{ChangeOp, WalRecord, WalRow};
use mosaica_types::{EntityId, ShardId, ShardState, Tenancy, TermId};

proptest! {
    /// Interleaved allocations on a single allocator never overlap and always advance the
    /// high-water mark by exactly the amount requested.
    #[test]
    fn issued_ids_rise_strictly(sizes in prop::collection::vec(1u64..64, 1..60)) {
        let mut alloc = Allocator::new(0);
        let mut expected_next = 0u64;
        let mut seen: HashSet<u64> = HashSet::new();

        for n in sizes {
            let ids = alloc.allocate_items(n, u64::MAX, &mut ByTenancy::new());
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
    /// `Allocator::new(manifest_hw.max(shard_zero_mark(&wal_records)))` — the actual seeding
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
            let seed = manifest_hw.max(shard_zero_mark(&wal_records));
            let mut alloc = Allocator::new(seed);

            for n in sizes {
                let range = alloc.allocate_items(n, u64::MAX, &mut ByTenancy::new());
                for id in range {
                    prop_assert!(!used.contains(&id), "id {} reused across a simulated crash", id);
                    used.insert(id);
                    wal_records.push(WalRecord::IngestBatch {
                        edits: Vec::new(),
                        receipt: Vec::new(),
                        batch_id: format!("batch-{id}"),
                        body_hash: [0u8; 32],
                        rows: vec![WalRow {
                            shard: ShardId::ZERO,
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

    /// **No entity id is reissued after a fold**, across fuzzed interleavings of ingest, flush,
    /// fold and crash. The fold's rotation reclaims the log, so after it the side-manifest's high
    /// water is the only home of the ids the log named.
    #[test]
    fn no_reuse_across_a_fold_that_reclaims_the_log(
        rounds in prop::collection::vec((1u64..32, any::<bool>(), any::<bool>()), 1..14),
    ) {
        // The two durable homes, neither of which is sufficient alone.
        let mut side_high_water = 0u64;         // SEGMENTS-<n>.json
        let mut wal: Vec<WalRecord> = Vec::new();
        let mut used: HashSet<u64> = HashSet::new();

        for (n, flush, fold) in rounds {
            // ---- restart, seeded exactly as `Engine::open` does ----------------------------
            let seed = side_high_water.max(shard_zero_mark(&wal));
            let mut alloc = Allocator::new(seed);

            for id in alloc.allocate_items(n, u64::MAX, &mut ByTenancy::new()) {
                prop_assert!(!used.contains(&id), "entity id {} reissued", id);
                used.insert(id);
                wal.push(WalRecord::IngestBatch {
                    edits: Vec::new(),
                    receipt: Vec::new(),
                    batch_id: format!("batch-{id}"),
                    body_hash: [0u8; 32],
                    rows: vec![WalRow {
                        shard: ShardId::ZERO,
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

            if flush || fold {
                // A publication raises the side-manifest past the ids it consumed.
                side_high_water = alloc.high_water();
            }
            if fold {
                // The rotation behind the fold reclaims the log; the live high water above still
                // records its ids.
                wal.clear();
            }
            // `alloc` is dropped here — the crash.
        }
    }

    /// **One window takes ids in the allocator's order** in its one open shard, over a pool of
    /// freed ids at fuzzed tenancies: an edit's new entity takes tenancy 0, lowest first, then
    /// the high water, whatever the size; a new item takes the lowest tenancy, lowest id first,
    /// then the high water up to the size; the edits draw first. The items the size cannot hold
    /// open the next shard and seal this one.
    #[test]
    fn a_window_takes_ids_in_the_allocators_order(
        pool in prop::collection::btree_map(0u32..200, 0u16..4, 0..40),
        edits in 0u64..30,
        items in 0u64..30,
        room in 0u64..60,
    ) {
        const HIGH_WATER: u64 = 1_000;
        let mut alloc = Allocator::new(HIGH_WATER);
        let free = by_tenancy(pool.iter().map(|(&id, &tenancy)| (id, tenancy)));
        alloc.release_after(0, free);
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
        let expected_items: Vec<u64> =
            by_tenancy.iter().map(|(_, id)| *id).take(items as usize).collect();
        let edits_fresh = edits - expected_edits.len() as u64;
        let items_fresh = items - expected_items.len() as u64;
        let here = items_fresh.min(room.saturating_sub(edits_fresh));
        expected_edits.extend(HIGH_WATER..HIGH_WATER + edits_fresh);
        let mut expected: Vec<(u32, u64)> = expected_edits.iter().map(|&id| (0, id)).collect();
        expected.extend(expected_items.iter().map(|&id| (0, id)));
        let first = HIGH_WATER + edits_fresh;
        expected.extend((first..first + here).map(|id| (0, id)));
        expected.extend((0..items_fresh - here).map(|id| (1, id)));

        let mut pending: Vec<PendingItem> = (0..edits + items)
            .map(|at| PendingItem {
                terms: Vec::new(),
                item: None,
                edit: (at < edits).then_some(ShardId::ZERO),
            })
            .collect();
        let placed = assign_sorted(
            &mut pending,
            &mut Allocators::single(ShardId::ZERO, &mut alloc, HIGH_WATER + room),
        )
        .unwrap();
        let given: Vec<(u32, u64)> = pending
            .iter()
            .map(|p| p.item.map(|item| (item.shard.raw(), item.entity.raw())).unwrap())
            .collect();
        prop_assert_eq!(given, expected);
        prop_assert_eq!(alloc.high_water(), HIGH_WATER + edits_fresh + here);
        let overflow = items_fresh > here;
        prop_assert_eq!(alloc.state() == ShardState::Sealed, overflow);
        prop_assert_eq!(placed.opened.len(), usize::from(overflow));
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
            let given_edits = alloc.allocate_edits(edited as u64, &mut ByTenancy::new()).unwrap();
            let given_items = alloc.allocate_items(created, u64::MAX, &mut ByTenancy::new());
            for (at, &entity) in given_edits.iter().enumerate() {
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
            for &number in &given_items {
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
                let mut freed = Vec::new();
                for (id, number) in gone.drain(..) {
                    in_use.remove(&id);
                    let tenancy = if number {
                        let tenancy = index.entry(id).or_insert(0);
                        *tenancy += 1;
                        *tenancy
                    } else {
                        0
                    };
                    freed.push((id as u32, tenancy));
                }
                alloc.release_after(position, by_tenancy(freed));
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
                let split = |ids: &Bitmap| {
                    by_tenancy(ids.iter().map(|id| {
                        (id, published.index.get(&u64::from(id)).copied().unwrap_or(0))
                    }))
                };
                let mut next = Allocator::new(published.high_water.max(shard_zero_mark(&wal)));
                let mut named = BTreeMap::new();
                for record in &wal {
                    name_entities(record, &mut named);
                }
                next.seed_freed(
                    split(&published.free),
                    published.held.iter().map(|(p, ids)| (*p, split(ids))).collect(),
                    retained_from,
                    &named.remove(&ShardId::ZERO).unwrap_or_default(),
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
                item: None,
                edit: None,
            })
            .collect();

        let mut alloc = Allocator::new(0);
        assign_sorted(
            &mut items,
            &mut Allocators::single(ShardId::ZERO, &mut alloc, u64::MAX),
        )
        .unwrap();

        let n = items.len() as u64;
        let id_of = |it: &PendingItem| {
            it.item.expect("assign_sorted must fill every item").entity.raw()
        };
        let mut ids: Vec<u64> = items.iter().map(id_of).collect();
        ids.sort_unstable();
        ids.dedup();
        prop_assert_eq!(ids, (0..n).collect::<Vec<_>>(), "ids are not a dense, distinct 0..n range");

        // Walk items in assigned-ID order; each time the (sorted, deduplicated) signature
        // changes, it must never recur — otherwise two runs of the same signature exist.
        let mut by_id: Vec<&PendingItem> = items.iter().collect();
        by_id.sort_by_key(|it| id_of(it));

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

/// Each id of `pairs` in the set of its tenancy.
fn by_tenancy(pairs: impl IntoIterator<Item = (u32, u16)>) -> ByTenancy {
    let mut sets = ByTenancy::new();
    for (id, tenancy) in pairs {
        sets.entry(Tenancy::new(tenancy).unwrap())
            .or_default()
            .add(id);
    }
    sets
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
        changes: vec![(item(EntityId::new(id)), ChangeOp::Delete)],
    }
}

fn row(id: u64) -> WalRecord {
    WalRecord::IngestBatch {
        edits: Vec::new(),
        receipt: Vec::new(),
        batch_id: format!("batch-{id}"),
        body_hash: [0u8; 32],
        rows: vec![WalRow {
            shard: ShardId::ZERO,
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

/// Shard 0's high water as the log `records` leaves it.
fn shard_zero_mark(records: &[WalRecord]) -> u64 {
    let mut hw = BTreeMap::new();
    for record in records {
        raise_high_water(record, &mut hw);
    }
    hw.get(&ShardId::ZERO).copied().unwrap_or(0)
}

fn item(entity: EntityId) -> mosaica_types::Target {
    mosaica_types::Target::Item(mosaica_types::ShardId::ZERO, entity)
}
