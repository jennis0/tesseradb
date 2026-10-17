//! A compaction of one shard of a bundle built as two: it rewrites that shard alone, retires that
//! shard's deletions alone, frees ids into that shard alone, and leaves every other shard's files,
//! identity and caches standing. A request compacts every shard with something to reclaim, and
//! the schedule picks each shard on its own gauges.

mod common;

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use common::*;
use mosaica_engine::{CompactionSchedule, Engine, EngineConfig};
use mosaica_lifecycle::wal::ChangeOp;
use mosaica_types::{ShardId, Target};

/// Each shard's `(identity, suppressed, deleted)` as the served side-manifest records them.
fn records(root: &std::path::Path) -> Vec<([u8; 32], Vec<u32>, Vec<u32>)> {
    side_manifest(root)
        .shards
        .records
        .iter()
        .map(|record| {
            (
                record.identity,
                entity_ids(&record.deny),
                entity_ids(&record.tombstones),
            )
        })
        .collect()
}

/// Every file under `shard`'s directory of the live prefix but its record files, with its inode.
fn shard_files(root: &std::path::Path, shard: ShardId) -> Vec<(String, u64)> {
    let dir =
        mosaica_store::ShardLayout::new(PARTITION, shard).dir(&root.join(current_prefix(root)));
    let mut out: Vec<(String, u64)> = files_under(&dir)
        .into_iter()
        .filter(|path| {
            !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("SEGMENTS-")
        })
        .map(|path| {
            let inode = std::fs::metadata(&path).unwrap().ino();
            let rel = path
                .strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (rel, inode)
        })
        .collect();
    out.sort();
    out
}

/// The same deletions and suppression in a bundle of one shard, by its entities.
fn one_shard_with(changes: &[(Target, ChangeOp)]) -> (Fixture, Engine) {
    let one = fixture_sized(TWO_SHARD_N, None);
    let engine = one.open();
    let renumbered = changes
        .iter()
        .map(|(target, op)| (as_one_shard(*target), *op))
        .collect();
    engine.accept_changes(renumbered).unwrap();
    (one, engine)
}

/// Compacting shard 1 leaves shard 0's files the same inodes, its identity, deletions and
/// fragment standing; shard 1's deletion leaves its record and its ids are issued again there
/// alone; what every viewer is served is what one shard serves, before and after a restart.
#[test]
fn compacting_one_shard_leaves_the_other_standing() {
    let several = two_shards();
    let engine = several.open_writing();
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
    tile_counts(&engine, &session, 3);
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
    tile_counts(&engine, &session, 3);
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
    assert_serves_as(&engine, &one_engine, "after the compaction");

    // The freed id is shard 1's own, issued there once the log no longer names it.
    reclaim_log(&engine);
    let inserted = insert_keyed(&engine, "reused", &[true])[0];
    assert_eq!(
        engine.resolve_targets(&[inserted]).unwrap(),
        vec![Some(item_in(1, 4))],
        "shard 1's freed id is issued again in shard 1"
    );
    assert_ne!(tenancy_of(inserted), 0, "at the next tenancy");

    let expected = served_to(&engine, &full_coverage_credential());
    drop(engine);
    let engine = several.open_writing();
    assert_eq!(served_to(&engine, &full_coverage_credential()), expected);
    assert_eq!(engine.retirable_deletions(), 1, "shard 0's deletion stands");
}

/// A deletion in a sealed shard leaves its overlay only at that shard's compaction, and a
/// suppression there survives both shards' compactions and a restart.
#[test]
fn a_sealed_shards_deletion_leaves_only_at_its_own_compaction() {
    let several = two_shards();
    let engine = several.open_writing();
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
    assert_serves_as(&engine, &one_engine, "after both compactions");

    drop(engine);
    let engine = several.open_writing();
    assert_eq!(
        engine.overlay_depth(),
        1,
        "the suppression survives the restart"
    );
    let held = records(&several.root);
    assert_eq!((held[0].1.clone(), held[0].2.clone()), (vec![2], vec![]));
    assert_eq!(
        served_to(&engine, &full_coverage_credential()),
        served_to(&one_engine, &full_coverage_credential())
    );
}

/// A request compacts the shards with something to reclaim, one after another, and stays raised
/// until the last publishes.
#[test]
fn a_request_compacts_every_shard_with_something_to_reclaim() {
    let several = two_shards();
    let engine = several.open_writing();
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
    let several = two_shards();
    let engine = several.open_writing();
    compact(&engine, Some(shard(0)));
    compact(&engine, Some(shard(1)));
    assert_eq!(compact(&engine, None), 1);
}

/// A schedule firing on deletions compacts the shard holding them, and a shard is held to the
/// interval on its own: shard 1, just compacted, waits; shard 0 does not.
#[test]
fn the_schedule_compacts_each_shard_on_its_own_gauges_and_interval() {
    let several = two_shards();
    let engine = open_writing(
        &several.root,
        &several.cache,
        &several.wal,
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
    let one = fixture_sized(TWO_SHARD_N, None);
    let whole = need(&one.open(), None);
    let several = two_shards();
    let engine = several.open_writing();
    let shard_one = need(&engine, Some(shard(1)));
    assert!(shard_one < whole, "{shard_one} is below {whole}");

    engine.set_fold_memory_for_test(Some((shard_one + whole) / 2));
    assert_eq!(compact(&engine, Some(shard(1))), 1);
}

/// A shard sealed with a deletion pending is compacted once, without a request or a schedule,
/// and not again.
#[test]
fn a_shard_sealed_with_a_deletion_pending_is_compacted_once() {
    let several = two_shards();
    let engine = several.open_writing();
    engine
        .accept_changes(vec![(item_in(1, 4), ChangeOp::Delete)])
        .unwrap();
    engine.set_fold_paused_for_test(true);
    seal_shard_one(&engine);
    tick_until(
        &engine,
        "the opened shard's publication",
        Duration::from_secs(30),
        || shards_of(&several.root).1.len() == 3,
    );
    let identity = records(&several.root)[1].0;
    engine.set_fold_paused_for_test(false);
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
    let several = two_shards();
    let engine = several.open_writing();
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

/// A window that fills shard 1, opens shard 2 and fills it too, then opens shard 3, queues a
/// closing compaction for each shard it sealed, the one it opened among them.
#[test]
fn a_shard_opened_and_sealed_in_one_window_is_compacted_once() {
    let several = two_shards();
    let engine = several.open_writing();
    engine.set_fold_paused_for_test(true);
    insert_keyed(&engine, "through", &[true; 36]);
    tick_until(
        &engine,
        "the opened shards' publication",
        Duration::from_secs(30),
        || shards_of(&several.root).1.len() == 4,
    );
    let before = records(&several.root);
    engine.set_fold_paused_for_test(false);
    tick_until(
        &engine,
        "both closing compactions",
        Duration::from_secs(60),
        || engine.write_executor_stats().folds == 2,
    );
    let after = records(&several.root);
    assert_ne!(after[1].0, before[1].0, "shard 1 is compacted");
    assert_ne!(after[2].0, before[2].0, "shard 2 is compacted");
    assert_eq!(after[3].0, before[3].0, "the open shard 3 is not");
    for _ in 0..5 {
        tick(&engine);
    }
    assert_eq!(engine.write_executor_stats().folds, 2, "and only once each");
}

/// Move an item of shard 0, found by its `id` value: an edit, whose row shard 0 buffers.
fn edit_in_shard_zero(engine: &Engine) {
    let id = ids_in_shard(engine, TWO_SHARD_N, 0)[0];
    edit_by_id(engine, "edit", id).expect("the edit is accepted");
}

/// Ingest eleven rows: ten fill shard 1 to its size and the eleventh opens shard 2, sealing
/// shard 1.
fn seal_shard_one(engine: &Engine) {
    insert_keyed(engine, "sealing", &[true; 11]);
}
