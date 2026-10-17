//! A compaction of one shard of a bundle built as two: it rewrites that shard alone, retires that
//! shard's deletions alone, frees ids into that shard alone, and leaves every other shard's files,
//! identity and caches standing. A request compacts every shard with something to reclaim, and
//! the schedule picks each shard on its own gauges.

mod common;

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use common::*;
use mosaica_engine::{CompactionSchedule, Engine, EngineConfig, RecordsOrder, ViewportRequest};
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_store::manifest::EntitySet;
use mosaica_types::{EntityId, MosaicaId, ShardId, Target};

/// Items the fixture builds.
const N: u64 = 40;
/// Shards of 25 and 15 items.
const SIZE: u64 = 25;

fn shard(id: u32) -> ShardId {
    ShardId::new(id).unwrap()
}

fn item_in(shard_id: u32, entity: u64) -> Target {
    Target::Item(shard(shard_id), EntityId::new(entity))
}

/// An engine over `built` that writes several shards, its executor running under `config`.
fn writing_with(built: &Fixture, config: EngineConfig) -> Engine {
    let mut engine =
        Engine::open_writing_several_shards(&built.root, &built.cache, &built.wal, config)
            .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine
}

fn writing(built: &Fixture) -> Engine {
    writing_with(built, config())
}

/// What a viewer is served: every item's `id`, and each tile's visible count.
fn served(engine: &Engine, credential: &[u8]) -> (Vec<u64>, Vec<(u64, u64)>) {
    let session = engine.authorise(credential).unwrap();
    let mut ids: Vec<u64> = read_items(engine, &session, &Items::new(RecordsOrder::Stored))
        .rows_with_ids
        .iter()
        .map(|&(_, id)| id)
        .collect();
    ids.sort_unstable();
    (ids, tiles(engine, &session))
}

fn tiles(engine: &Engine, session: &mosaica_engine::Session) -> Vec<(u64, u64)> {
    let out = engine
        .viewport(
            session,
            ViewportRequest::new("s0", 3, WHOLE_MAP, N as usize),
        )
        .expect("the viewport answers");
    let mut counts: Vec<(u64, u64)> = out.tiles.iter().map(|t| (t.tile, t.visible)).collect();
    counts.sort_unstable();
    counts
}

fn ids_in(set: &EntitySet) -> Vec<u32> {
    set.entities().expect("the set decodes").iter().collect()
}

/// Each shard's `(identity, suppressed, deleted)` as the served side-manifest records them.
fn records(root: &std::path::Path) -> Vec<([u8; 32], Vec<u32>, Vec<u32>)> {
    side_manifest(root)
        .shards
        .records
        .iter()
        .map(|record| {
            (
                record.identity,
                ids_in(&record.deny),
                ids_in(&record.tombstones),
            )
        })
        .collect()
}

/// Every file under `shard`'s directory of the live prefix but its record files, with its inode.
fn shard_files(root: &std::path::Path, shard: ShardId) -> Vec<(String, u64)> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, u64)>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if !entry.file_name().to_string_lossy().starts_with("SEGMENTS-") {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, entry.metadata().unwrap().ino()));
            }
        }
    }
    let dir =
        mosaica_store::ShardLayout::new(PARTITION, shard).dir(&root.join(current_prefix(root)));
    let mut out = Vec::new();
    walk(&dir, &dir, &mut out);
    out.sort();
    out
}

/// The same deletions and suppression in a bundle of one shard, by its entities.
fn one_shard_with(changes: &[(Target, ChangeOp)]) -> (Fixture, Engine) {
    let one = fixture_sized(N, None);
    let engine = one.open();
    let renumbered = changes
        .iter()
        .map(|(target, op)| match target {
            Target::Item(at, entity) if at.raw() == 1 => (item_in(0, SIZE + entity.raw()), *op),
            other => (*other, *op),
        })
        .collect();
    engine.accept_changes(renumbered).unwrap();
    (one, engine)
}

/// Compacting shard 1 leaves shard 0's files the same inodes, its identity, deletions and
/// fragment standing; shard 1's deletion leaves its record and its ids are issued again there
/// alone; what every viewer is served is what one shard serves, before and after a restart.
#[test]
fn compacting_one_shard_leaves_the_other_standing() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    engine.set_merge_for_test(false);
    let changes = [
        (item_in(0, 5), ChangeOp::Delete),
        (item_in(0, 2), ChangeOp::Suppress),
        (item_in(1, 4), ChangeOp::Delete),
        (item_in(1, 3), ChangeOp::Suppress),
    ];
    engine.accept_changes(changes.to_vec()).unwrap();
    let (_one, one_engine) = one_shard_with(&changes);
    tick_until(
        &engine,
        "each shard's denies in its record",
        Duration::from_secs(30),
        || {
            let held = records(&several.root);
            held[0].2 == vec![5] && held[1].2 == vec![4]
        },
    );
    let before = records(&several.root);
    let files = shard_files(&several.root, shard(0));
    assert!(!files.is_empty());
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    tiles(&engine, &session);
    let (fragments, projections) = (
        engine.fragment_cache_rebuilds(),
        engine.full_projection_builds(),
    );

    assert_eq!(compact(&engine, Some(shard(1))), 1);
    let after = records(&several.root);
    assert_eq!(after[0], before[0], "shard 0's identity and denies stand");
    assert_ne!(after[1].0, before[1].0, "shard 1's identity moves");
    assert_eq!(
        (after[1].1.clone(), after[1].2.clone()),
        (vec![3], vec![]),
        "shard 1's deletion is retired and its suppression stands"
    );
    assert_eq!(
        shard_files(&several.root, shard(0)),
        files,
        "shard 0's files are linked, not written"
    );
    assert_eq!(engine.retirable_deletions(), 1);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    tiles(&engine, &session);
    assert_eq!(
        engine.fragment_cache_rebuilds(),
        fragments + 1,
        "shard 1's fragment alone is rebuilt"
    );
    assert_eq!(
        engine.full_projection_builds(),
        projections + 1,
        "shard 1's projection alone is rebuilt"
    );
    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(
            served(&engine, &credential),
            served(&one_engine, &credential)
        );
    }

    // The freed id is shard 1's own, issued there once the log no longer names it.
    reclaim_log(&engine);
    let inserted = insert(&engine, "reused");
    assert_eq!(
        engine.resolve_targets(&[inserted]).unwrap(),
        vec![Some(item_in(1, 4))],
        "shard 1's freed id is issued again in shard 1"
    );
    assert_ne!(tenancy_of(inserted), 0, "at the next tenancy");

    let expected = served(&engine, &full_coverage_credential());
    drop(engine);
    let engine = writing(&several);
    assert_eq!(served(&engine, &full_coverage_credential()), expected);
    assert_eq!(engine.retirable_deletions(), 1, "shard 0's deletion stands");
}

/// A deletion in a sealed shard leaves its overlay only at that shard's compaction, and a
/// suppression there survives both shards' compactions and a restart.
#[test]
fn a_sealed_shards_deletion_leaves_only_at_its_own_compaction() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    let changes = [
        (item_in(0, 5), ChangeOp::Delete),
        (item_in(0, 2), ChangeOp::Suppress),
    ];
    engine.accept_changes(changes.to_vec()).unwrap();
    let (_one, one_engine) = one_shard_with(&changes);

    compact(&engine, Some(shard(1)));
    assert_eq!(
        engine.retirable_deletions(),
        1,
        "shard 1's compaction leaves it"
    );
    let held = records(&several.root);
    assert_eq!((held[0].1.clone(), held[0].2.clone()), (vec![2], vec![5]));

    compact(&engine, Some(shard(0)));
    assert_eq!(
        engine.retirable_deletions(),
        0,
        "shard 0's compaction retires it"
    );
    let held = records(&several.root);
    assert_eq!((held[0].1.clone(), held[0].2.clone()), (vec![2], vec![]));
    for credential in [full_coverage_credential(), subset_credential()] {
        assert_eq!(
            served(&engine, &credential),
            served(&one_engine, &credential)
        );
    }

    drop(engine);
    let engine = writing(&several);
    assert_eq!(
        engine.overlay_depth(),
        1,
        "the suppression survives the restart"
    );
    let held = records(&several.root);
    assert_eq!((held[0].1.clone(), held[0].2.clone()), (vec![2], vec![]));
    assert_eq!(
        served(&engine, &full_coverage_credential()),
        served(&one_engine, &full_coverage_credential())
    );
}

/// A request compacts the shards with something to reclaim, one after another, and stays raised
/// until the last publishes.
#[test]
fn a_request_compacts_every_shard_with_something_to_reclaim() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    engine
        .accept_changes(vec![(item_in(1, 4), ChangeOp::Delete)])
        .unwrap();
    let identity = |root: &std::path::Path, at: usize| records(root)[at].0;
    let zero = identity(&several.root, 0);
    assert_eq!(compact(&engine, None), 1, "shard 1 alone has a deletion");
    assert_eq!(identity(&several.root, 0), zero, "shard 0 is not compacted");

    engine
        .accept_changes(vec![
            (item_in(0, 5), ChangeOp::Delete),
            (item_in(1, 6), ChangeOp::Delete),
        ])
        .unwrap();
    let before = engine.write_executor_stats().folds;
    engine.set_fold_paused_for_test(true);
    engine.request_fold();
    wait_until(
        "the first compaction holds",
        Duration::from_secs(60),
        || engine.fold_is_holding_for_test(),
    );
    let held = engine.write_executor_stats();
    assert!(
        held.fold_requested,
        "raised while the first compaction runs"
    );
    assert_eq!(held.folds, before);
    engine.set_fold_paused_for_test(false);
    wait_until("both compactions publish", Duration::from_secs(120), || {
        let now = engine.write_executor_stats();
        now.folds == before + 2 && !now.fold_requested
    });
    assert_ne!(identity(&several.root, 0), zero, "shard 0 is compacted");
    assert_eq!(engine.retirable_deletions(), 0);
}

/// With nothing to reclaim anywhere, a request compacts the shard the schedule ranks first.
#[test]
fn a_request_with_nothing_to_reclaim_compacts_one_shard() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    compact(&engine, Some(shard(0)));
    compact(&engine, Some(shard(1)));
    assert_eq!(compact(&engine, None), 1);
}

/// A schedule firing on deletions compacts the shard holding them, and a shard is held to the
/// interval on its own: shard 1, just compacted, waits; shard 0 does not.
#[test]
fn the_schedule_compacts_each_shard_on_its_own_gauges_and_interval() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing_with(
        &several,
        EngineConfig {
            compaction: CompactionSchedule {
                min_interval_secs: 86_400,
                after_deletions: Some(1),
                ..CompactionSchedule::off()
            },
            ..config()
        },
    );
    let zero = records(&several.root)[0].0;
    engine
        .accept_changes(vec![(item_in(1, 4), ChangeOp::Delete)])
        .unwrap();
    tick_until(
        &engine,
        "shard 1 is compacted",
        Duration::from_secs(60),
        || engine.write_executor_stats().folds == 1,
    );
    assert_eq!(records(&several.root)[0].0, zero, "shard 0 has nothing due");
    assert_eq!(engine.retirable_deletions(), 0);

    engine
        .accept_changes(vec![(item_in(1, 6), ChangeOp::Delete)])
        .unwrap();
    for _ in 0..5 {
        tick(&engine);
    }
    assert_eq!(
        engine.write_executor_stats().folds,
        1,
        "shard 1 waits out its interval"
    );
    engine
        .accept_changes(vec![(item_in(0, 5), ChangeOp::Delete)])
        .unwrap();
    tick_until(
        &engine,
        "shard 0 is compacted",
        Duration::from_secs(60),
        || engine.write_executor_stats().folds == 2,
    );
    assert_ne!(records(&several.root)[0].0, zero);
    assert_eq!(
        engine.retirable_deletions(),
        1,
        "shard 1's second deletion waits"
    );
}

/// The memory pre-flight charges the shard compacted: memory between that shard's estimate and
/// the whole corpus's compacts it.
#[test]
fn memory_between_one_shards_estimate_and_the_corpus_still_compacts() {
    let need = |engine: &Engine, shard_id: Option<ShardId>| {
        engine.set_fold_memory_for_test(Some(1));
        let refusals = engine.write_executor_stats().fold_refusals;
        engine.request_compaction(shard_id);
        wait_until("the refusal", Duration::from_secs(60), || {
            engine.write_executor_stats().fold_refusals > refusals
        });
        let refusal = engine.write_executor_stats().last_fold_refusal.unwrap();
        assert_eq!(refusal.gate, "insufficient_memory");
        refusal.need_bytes.unwrap()
    };
    let one = fixture_sized(N, None);
    let whole = need(&one.open(), None);
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    let shard_one = need(&engine, Some(shard(1)));
    assert!(shard_one < whole, "{shard_one} is below {whole}");

    engine.set_fold_memory_for_test(Some((shard_one + whole) / 2));
    assert_eq!(compact(&engine, Some(shard(1))), 1);
}

/// A shard sealed with a deletion pending is compacted once, without a request or a schedule,
/// and not again.
#[test]
fn a_shard_sealed_with_a_deletion_pending_is_compacted_once() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    engine
        .accept_changes(vec![(item_in(1, 4), ChangeOp::Delete)])
        .unwrap();
    seal_shard_one(&engine);
    assert_eq!(shards_of(&several.root).1.len(), 3);
    let identity = records(&several.root)[1].0;
    tick_until(
        &engine,
        "the closing compaction",
        Duration::from_secs(60),
        || engine.write_executor_stats().folds == 1,
    );
    assert_eq!(engine.retirable_deletions(), 0);
    assert_ne!(
        records(&several.root)[1].0,
        identity,
        "shard 1 is the one compacted"
    );
    for _ in 0..5 {
        tick(&engine);
    }
    assert_eq!(engine.write_executor_stats().folds, 1, "and only once");
}

/// A shard sealed with its rows still buffered, and nothing else to reclaim, is compacted once
/// those rows are flushed. An edit in shard 0 is older, so the first tick flushes it and none of
/// shard 1's rows.
#[test]
fn a_shard_sealed_with_its_rows_buffered_is_compacted_once_they_flush() {
    let several = fixture_sized(N, Some(SIZE));
    let engine = writing(&several);
    edit_in_shard_zero(&engine);
    seal_shard_one(&engine);
    let identity = records(&several.root)[1].0;
    tick_until(
        &engine,
        "the closing compaction",
        Duration::from_secs(30),
        || engine.write_executor_stats().folds == 1,
    );
    assert_ne!(
        records(&several.root)[1].0,
        identity,
        "shard 1 is the one compacted"
    );
    for _ in 0..5 {
        tick(&engine);
    }
    assert_eq!(engine.write_executor_stats().folds, 1, "and only once");
}

/// Move an item of shard 0, found by its `id` value: an edit, whose row shard 0 buffers.
fn edit_in_shard_zero(engine: &Engine) {
    let named = |id: u64| {
        let table = mosaica_engine::AddressTable {
            rows: 1,
            mosaica_id: None,
            columns: vec![(
                "id".to_string(),
                vec![Some(mosaica_engine::AddressValue::Text(id.to_string()))],
            )],
        };
        engine.name_items(&table).unwrap().verdicts[0]
            .clone()
            .expect("the value names an item")
    };
    let id = (0..N)
        .find(|id| named(*id).shard == shard(0))
        .expect("shard 0 holds an item");
    engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: "edit".to_string(),
            body_hash: [9; 32],
            view: Some("s0".to_string()),
            rows: vec![mosaica_lifecycle::IngestRow {
                mosaica_id: None,
                labels: Some(vec![b"0".to_vec()]),
                position: Some((7.0, 7.0)),
                scalars: vec![mosaica_lifecycle::wal::WalScalar::U64(id)],
                scoped: Vec::new(),
                omitted: Vec::new(),
            }],
            artifacts: Default::default(),
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the edit is accepted");
}

/// Ingest eleven rows: ten fill shard 1 to its size and the eleventh opens shard 2, sealing
/// shard 1.
fn seal_shard_one(engine: &Engine) {
    let rows = (0..11)
        .map(|at| mosaica_lifecycle::IngestRow {
            mosaica_id: None,
            labels: Some(vec![b"0".to_vec()]),
            position: Some((5.0, 5.0)),
            scalars: keyed(&format!("sealing-{at}")),
            scoped: Vec::new(),
            omitted: Vec::new(),
        })
        .collect();
    engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: "sealing".to_string(),
            body_hash: [8; 32],
            view: Some("s0".to_string()),
            rows,
            artifacts: Default::default(),
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the batch is accepted");
}

fn insert(engine: &Engine, key: &str) -> MosaicaId {
    let receipt = engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: key.to_string(),
            body_hash: [7; 32],
            view: Some("s0".to_string()),
            rows: vec![mosaica_lifecycle::IngestRow {
                mosaica_id: None,
                labels: Some(vec![b"0".to_vec()]),
                position: Some((5.0, 5.0)),
                scalars: keyed(key),
                scoped: Vec::new(),
                omitted: Vec::new(),
            }],
            artifacts: Default::default(),
            strict: true,
            mosaica_id_column: false,
        })
        .expect("the insert is accepted");
    receipt.mosaica_ids[0].expect("the row created an item")
}
