//! Where a running service puts new items among several point shards: into the largest pool of
//! freed ids a sealed shard holds, then the open shard's pool and high water, and past the open
//! shard's size into a shard it opens; and what a restart keeps of it.
//!
//! Written through the engine's several-shard feature, whose admission lets a window's new items
//! run past the open shard's size.

mod common;

use common::*;
use mosaica_engine::Engine;
use mosaica_lifecycle::wal::{Wal, WalRecord};
use mosaica_store::manifest::SegmentsManifest;
use mosaica_types::{MosaicaId, ShardId, ShardState, Target};

/// Items the fixture builds.
const N: u64 = 40;
/// The size the fixture is built at: shard 0 sealed at 25 items, shard 1 open at 15.
const SIZE: u64 = 25;

fn shard(id: u32) -> ShardId {
    ShardId::new(id).unwrap()
}

/// An engine over `built` that writes several shards, its executor running. A compaction a seal
/// starts holds before it publishes, so what these tests read from the bundle is what allocation
/// wrote, whenever they read it.
fn writing(built: &Fixture) -> Engine {
    let mut engine =
        Engine::open_writing_several_shards(&built.root, &built.cache, &built.wal, config())
            .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_fold_paused_for_test(true);
    engine
}

/// One batch of new items, one for each of `labelled`: the item holds the `id` of its key and the
/// term every credential holds where it is labelled, and no term where it is not. The receipt's
/// `mosaica_id`s, in row order.
fn insert(engine: &Engine, batch: &str, labelled: &[bool]) -> Vec<MosaicaId> {
    let rows = labelled
        .iter()
        .enumerate()
        .map(|(at, labelled)| mosaica_lifecycle::IngestRow {
            mosaica_id: None,
            labels: Some(match labelled {
                true => vec![b"0".to_vec()],
                false => Vec::new(),
            }),
            position: Some((5.0, 5.0)),
            scalars: keyed(&format!("{batch}-{at}")),
            scoped: Vec::new(),
            omitted: Vec::new(),
        })
        .collect();
    let receipt = engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: batch.to_string(),
            body_hash: [9; 32],
            view: Some("s0".to_string()),
            rows,
            artifacts: Default::default(),
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the batch is accepted");
    receipt
        .mosaica_ids
        .into_iter()
        .map(|id| id.expect("each row created an item"))
        .collect()
}

/// Each `mosaica_id`'s shard, entity and tenancy.
fn placed(engine: &Engine, ids: &[MosaicaId]) -> Vec<(u32, u64, u16)> {
    engine
        .resolve_targets(ids)
        .unwrap()
        .into_iter()
        .zip(ids)
        .map(|(target, id)| match target {
            Some(Target::Item(shard, entity)) => (shard.raw(), entity.raw(), tenancy_of(*id)),
            other => panic!("{id:?} names {other:?}, not an item"),
        })
        .collect()
}

/// Each shard's state, high water, freed ids and held ids as the served side-manifest records
/// them, and its `next_id`.
type Recorded = (Vec<(u32, ShardState, u64, Vec<u32>, usize)>, u32);

fn recorded(manifest: &SegmentsManifest) -> Recorded {
    let shards = manifest
        .shards
        .records
        .iter()
        .map(|record| {
            (
                record.id.raw(),
                record.state,
                record.high_water,
                record
                    .free_entities
                    .entities()
                    .map(|ids| ids.iter().collect())
                    .unwrap_or_default(),
                record.held_entities.len(),
            )
        })
        .collect();
    (shards, manifest.shards.next_id)
}

/// Remove every partition file published after publication `n`, as a crash before those
/// publications committed leaves the bundle.
fn crash_back_to(root: &std::path::Path, n: u64) {
    let dir = partition_dir(root, &current_prefix(root));
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name();
        let later = name
            .to_str()
            .and_then(|name| name.strip_prefix("SEGMENTS-"))
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|m| m.parse::<u64>().ok())
            .is_some_and(|m| m > n);
        if later {
            std::fs::remove_file(dir.join(name)).unwrap();
        }
    }
}

/// The built fixture with shard 0's pool seeded: numbers 25 to 28 freed at tenancy 1, which new
/// items take, and 29 and 30 at tenancy 0, which only shard 0's edits take.
fn seeded() -> Fixture {
    let built = fixture_sized(N, Some(SIZE));
    seed_freed(
        &built.root,
        ShardId::ZERO,
        31,
        &[(1, &[25, 26, 27, 28]), (0, &[29, 30])],
    );
    built
}

/// New items take the sealed shard's pool lowest first at the tenancy the number is freed at, and
/// a window straddles the pool and the open shard in signature order: the unlabelled rows sort
/// first and take the pool, the labelled the open shard's high water. Once the pool is spent a new
/// item takes the open shard's high water, and never an id freed at tenancy 0.
#[test]
fn new_items_take_the_sealed_pool_lowest_first_then_the_open_shard() {
    let built = seeded();
    let engine = writing(&built);

    let first = insert(&engine, "a", &[true, true]);
    assert_eq!(placed(&engine, &first), vec![(0, 25, 1), (0, 26, 1)]);

    let straddling = insert(&engine, "b", &[true, false, true, false]);
    assert_eq!(
        placed(&engine, &straddling),
        vec![(1, 15, 0), (0, 27, 1), (1, 16, 0), (0, 28, 1)]
    );

    let after = insert(&engine, "c", &[false]);
    assert_eq!(
        placed(&engine, &after),
        vec![(1, 17, 0)],
        "shard 0's tenancy-0 ids are its edits'"
    );
    assert_eq!(engine.allocator_high_water(), 31 + 18);

    publish_buffered(&engine);
    let (shards, next_id) = recorded(&side_manifest(&built.root));
    assert_eq!(next_id, 2);
    assert_eq!(
        shards,
        vec![
            (0, ShardState::Sealed, 31, vec![29, 30], 0),
            (1, ShardState::Open, 18, vec![], 0),
        ]
    );
}

/// A window whose new items run past the open shard's size fills it, seals it and opens the next
/// shard, which the log records ahead of the window's rows. Shards, states, `next_id`, high waters
/// and pools survive a restart that replays the opening and one that reads it from a manifest, and
/// the next window takes the ids an engine that did not restart takes.
#[test]
fn a_window_past_the_size_opens_a_shard_that_survives_restarts() {
    let restarted = seeded();
    let unbroken = seeded();
    // Sixteen rows, the odd ones unlabelled, which sort first.
    let first_window: Vec<bool> = (0..16).map(|at| at % 2 == 0).collect();

    let engine = writing(&restarted);
    let first = insert(&engine, "a", &first_window);
    let unlabelled = [(0, 25, 1), (0, 26, 1), (0, 27, 1), (0, 28, 1)]
        .into_iter()
        .chain((15..19).map(|entity| (1, entity, 0)));
    let labelled = (19..25)
        .map(|entity| (1, entity, 0))
        .chain((0..2).map(|entity| (2, entity, 0)));
    let mut wanted = Vec::new();
    for (labelled, unlabelled) in labelled.zip(unlabelled) {
        wanted.extend([labelled, unlabelled]);
    }
    assert_eq!(
        placed(&engine, &first),
        wanted,
        "the pool's four, shard 1's ten to its size, and two in shard 2"
    );
    let logged: Vec<WalRecord> = Wal::open(&restarted.wal)
        .unwrap()
        .records()
        .map(|record| record.unwrap().1)
        .filter(|record| {
            matches!(
                record,
                WalRecord::ShardOpen { .. }
                    | WalRecord::ShardSeal { .. }
                    | WalRecord::IngestBatch { .. }
            )
        })
        .collect();
    assert!(matches!(
        logged.as_slice(),
        [
            WalRecord::ShardOpen { shard: opened },
            WalRecord::ShardSeal { shard: sealed },
            WalRecord::IngestBatch { .. },
        ] if *opened == shard(2) && *sealed == shard(1)
    ));
    let placed_first = placed(&engine, &first);
    drop(engine);
    assert_eq!(
        side_manifest(&restarted.root).shards.records.len(),
        3,
        "the publication after the window names shard 2"
    );
    // A crash before that publication: the restart replays the opening.
    crash_back_to(&restarted.root, 0);
    assert_eq!(side_manifest(&restarted.root).shards.records.len(), 2);
    let engine = writing(&restarted);
    assert_eq!(placed(&engine, &first), placed_first);
    let second = insert(&engine, "b", &[true, false, true]);

    let other = writing(&unbroken);
    let other_first = insert(&other, "a", &first_window);
    assert_eq!(placed(&other, &other_first), placed_first);
    let other_second = insert(&other, "b", &[true, false, true]);
    assert_eq!(
        placed(&engine, &second),
        placed(&other, &other_second),
        "the next window takes the same ids"
    );
    assert_eq!(
        placed(&engine, &second),
        vec![(2, 3, 0), (2, 2, 0), (2, 4, 0)]
    );

    publish_buffered(&engine);
    publish_buffered(&other);
    let wanted = (
        vec![
            (0, ShardState::Sealed, 31, vec![29, 30], 0),
            (1, ShardState::Sealed, 25, vec![], 0),
            (2, ShardState::Open, 5, vec![], 0),
        ],
        3,
    );
    assert_eq!(recorded(&side_manifest(&restarted.root)), wanted);
    assert_eq!(recorded(&side_manifest(&unbroken.root)), wanted);
    let live_rows = engine.generation_status().live_rows;
    assert_eq!(live_rows, other.generation_status().live_rows);
    assert_eq!(live_rows, N + 19);
    drop(engine);

    // The manifest names shard 2 now.
    let engine = writing(&restarted);
    assert_eq!(placed(&engine, &first), placed_first);
    assert_eq!(placed(&engine, &second), placed(&other, &other_second));
    assert_eq!(engine.generation_status().live_rows, live_rows);
    let third = insert(&engine, "c", &[false]);
    let other_third = insert(&other, "c", &[false]);
    assert_eq!(placed(&engine, &third), vec![(2, 5, 0)]);
    assert_eq!(placed(&other, &other_third), vec![(2, 5, 0)]);
}

/// A window whose records do not become durable opens and seals nothing: the next window seals the
/// shard and opens the next itself, the log holding one opening and one seal, and the ids the
/// failed window took are not issued again.
#[test]
fn a_window_that_is_not_made_durable_opens_and_seals_nothing() {
    let built = fixture_sized(N, Some(SIZE));
    let mut engine =
        Engine::open_writing_several_shards(&built.root, &built.cache, &built.wal, config())
            .expect("the engine opens");
    let faults = std::sync::Arc::new(mosaica_lifecycle::faults::FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(8, std::sync::Arc::clone(&faults))
        .expect("the executor starts");
    let twelve: Vec<bool> = vec![true; 12];

    faults.fail_next_fsyncs(1);
    let refused = engine.ingest(mosaica_engine::IngestRequest {
        batch_id: "failed".to_string(),
        body_hash: [9; 32],
        view: Some("s0".to_string()),
        rows: (0..12)
            .map(|at| mosaica_lifecycle::IngestRow {
                mosaica_id: None,
                labels: Some(vec![b"0".to_vec()]),
                position: Some((5.0, 5.0)),
                scalars: keyed(&format!("failed-{at}")),
                scoped: Vec::new(),
                omitted: Vec::new(),
            })
            .collect(),
        artifacts: Default::default(),
        strict: true,
        mosaica_id_column: false,
    });
    assert!(
        matches!(
            refused,
            Err(mosaica_engine::AcceptError::Exec(
                mosaica_lifecycle::ExecError::Wal(_)
            ))
        ),
        "{:?}",
        refused.err()
    );

    let accepted = insert(&engine, "after", &twelve);
    assert_eq!(
        placed(&engine, &accepted),
        (0..12).map(|entity| (2, entity, 0)).collect::<Vec<_>>(),
        "shard 1's ids the failed window took stay spent, and shard 2 opens once"
    );
    let logged: Vec<WalRecord> = Wal::open(&built.wal)
        .unwrap()
        .records()
        .map(|record| record.unwrap().1)
        .filter(|record| {
            matches!(
                record,
                WalRecord::ShardOpen { .. } | WalRecord::ShardSeal { .. }
            )
        })
        .collect();
    assert_eq!(
        logged,
        vec![
            WalRecord::ShardOpen { shard: shard(2) },
            WalRecord::ShardSeal { shard: shard(1) },
        ]
    );
}
