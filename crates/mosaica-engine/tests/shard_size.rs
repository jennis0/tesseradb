//! `[shards] size`, declared at a build or at a running service, and the inserts and edits it
//! refuses.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use common::*;
use mosaica_engine::{AcceptError, Engine};
use mosaica_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};
use mosaica_lifecycle::{ExecError, UnallocatedRow};
use mosaica_types::{EntityId, ShardFull, ShardLimit, ShardSizeError, SHARD_SIZE_DEFAULT};

/// Items the fixture builds.
const N: u64 = 40;
const WAIT: Duration = Duration::from_secs(20);

/// The fixture of [`N`] items, built at `size` where one is given.
fn built(size: Option<u64>) -> TempDir {
    let tmp = TempDir::new().unwrap();
    build_fixture_sized(
        &tmp.path().join("bundle"),
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
        N,
        size,
    );
    tmp
}

fn engine_over(tmp: &Path) -> (Engine, Arc<FaultSwitchboard>) {
    let mut engine = open_engine(
        &tmp.join("bundle"),
        &tmp.join("cache"),
        &tmp.join("wal.log"),
    );
    let faults = Arc::new(FaultSwitchboard::new());
    engine
        .start_write_executor_with_faults(8, Arc::clone(&faults))
        .expect("the executor starts once");
    (engine, faults)
}

/// Reopens the bundle and log, dropping `engine` first so its executor releases the log.
fn restart(tmp: &Path, engine: Engine) -> Engine {
    drop(engine);
    engine_over(tmp).0
}

/// `n` new items, holding the `id`s `{prefix}-0` onwards.
fn rows(prefix: &str, n: usize) -> Vec<UnallocatedRow> {
    (0..n).map(|i| keyed_row(&format!("{prefix}-{i}"))).collect()
}

fn full(size: u64, high_water: u64, needed: u64) -> ShardFull {
    ShardFull {
        limit: ShardLimit::Size(size),
        high_water,
        needed,
    }
}

fn refused_for_room(result: Result<Vec<EntityId>, AcceptError>) -> ShardFull {
    match result {
        Err(AcceptError::Exec(ExecError::ShardFull(full))) => full,
        other => panic!("expected a refusal for the shard's size, got {other:?}"),
    }
}

fn size_refused(result: Result<bool, AcceptError>) -> ShardSizeError {
    match result {
        Err(AcceptError::Exec(ExecError::ShardSize(e))) => e,
        other => panic!("expected the size to be refused, got {other:?}"),
    }
}

#[test]
fn a_size_declared_at_a_running_service_survives_a_restart_a_fold_and_both() {
    let tmp = built(None);
    let (engine, _faults) = engine_over(tmp.path());
    assert_eq!(engine.shard_size(), SHARD_SIZE_DEFAULT);
    assert_eq!(shards_of(&tmp.path().join("bundle")).0, SHARD_SIZE_DEFAULT);

    let size = N + 10;
    assert!(!engine.declare_shards(size).expect("the size is declared"));
    assert!(
        engine.declare_shards(size).expect("the same size again"),
        "a size the shard has answers as existing"
    );
    assert_eq!(engine.shard_size(), size);

    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), size, "replayed from the log");

    publish_buffered(&engine);
    assert_eq!(shards_of(&tmp.path().join("bundle")).0, size, "the publication restates it");
    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), size, "read from the side-manifest");

    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");
    assert_eq!(
        shards_of(&tmp.path().join("bundle")).0,
        size,
        "the fold's side-manifest carries it"
    );
    assert_eq!(engine.shard_size(), size);
    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), size, "after a fold and a restart");
}

#[test]
fn a_size_of_zero_or_below_the_high_water_is_refused() {
    let tmp = built(None);
    let (engine, _faults) = engine_over(tmp.path());
    assert_eq!(size_refused(engine.declare_shards(0)), ShardSizeError::Zero);
    assert_eq!(
        size_refused(engine.declare_shards(N - 1)),
        ShardSizeError::BelowHighWater {
            size: N - 1,
            high_water: N
        }
    );
    assert_eq!(engine.shard_size(), SHARD_SIZE_DEFAULT);
    assert!(
        !engine.declare_shards(N).expect("a size at the high water"),
        "a size at the high water is accepted"
    );
    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), N);
}

/// Three batches queue behind a parked window: `a` fits, `b` fits neither beside `a` nor after it,
/// and `c` fits after `a`.
#[test]
fn a_batch_past_the_size_is_refused_alone_and_goes_in_once_the_size_is_raised() {
    let tmp = built(None);
    let (engine, faults) = engine_over(tmp.path());
    let size = N + 4;
    engine.declare_shards(size).expect("the size is declared");
    let engine = Arc::new(engine);

    // Park the executor inside a window of its own, one id past the build.
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    let primer = Arc::clone(&engine);
    let prime =
        std::thread::spawn(move || primer.ingest_rows(rows("prime", 1), "prime".into(), [9; 32]));
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);
    // Already counts the size's declaration and the priming batch.
    let queued = engine.write_executor_stats().work_submitted;

    let batches = [("a", 2usize), ("b", 3), ("c", 1)];
    let results = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for (i, (batch, n)) in batches.into_iter().enumerate() {
        let e = Arc::clone(&engine);
        let out = Arc::clone(&results);
        handles.push(std::thread::spawn(move || {
            let answer = e.ingest_rows(rows(batch, n), batch.to_string(), [i as u8; 32]);
            out.lock().unwrap().push((batch, answer));
        }));
        wait_until("the batch reaches the queue", WAIT, || {
            engine.write_executor_stats().work_submitted > queued + i as u64
        });
    }
    faults.release();
    for handle in handles {
        handle.join().unwrap();
    }
    prime.join().unwrap().expect("the priming batch goes in");

    let mut results = Arc::try_unwrap(results).unwrap().into_inner().unwrap();
    results.sort_by_key(|(batch, _)| *batch);
    let mut results = results.into_iter().map(|(_, answer)| answer);
    assert_eq!(results.next().unwrap().expect("a fits").len(), 2);
    assert_eq!(
        refused_for_room(results.next().unwrap()),
        full(size, N + 3, 3),
        "b is measured against the room a left"
    );
    assert_eq!(results.next().unwrap().expect("c fits after a").len(), 1);
    assert_eq!(engine.allocator_high_water(), size);
    assert!(
        (0..3).all(|i| item_of_key(&engine, &format!("b-{i}")).is_none()),
        "the refused batch created nothing"
    );

    engine.declare_shards(size + 3).expect("the size is raised");
    let again = engine
        .ingest_rows(rows("b", 3), "b".into(), [1; 32])
        .expect("the same batch goes in");
    assert_eq!(again.len(), 3);
    assert!((0..3).all(|i| item_of_key(&engine, &format!("b-{i}")).is_some()));
    assert_eq!(engine.allocator_high_water(), size + 3);
}

#[test]
fn a_build_at_its_size_refuses_an_insert_and_an_edit_past_it() {
    let tmp = built(Some(N));
    let (engine, _faults) = engine_over(tmp.path());
    assert_eq!(engine.shard_size(), N);
    assert_eq!(shards_of(&tmp.path().join("bundle")).0, N);
    assert_eq!(engine.allocator_high_water(), N);

    assert_eq!(
        refused_for_room(engine.ingest_rows(rows("new", 1), "new".into(), [1; 32])),
        full(N, N, 1)
    );
    let item = item_of_id(&engine, 1)
        .unwrap()
        .expect("the fixture holds item 1");
    let moved = UnallocatedRow {
        join: Some(item),
        x: 7.0,
        y: 7.0,
        scalars: vec![mosaica_lifecycle::wal::WalScalar::U64(1)],
        ..keyed_row("unused")
    };
    assert_eq!(
        refused_for_room(engine.ingest_rows(vec![moved.clone()], "edit".into(), [2; 32])),
        full(N, N, 1),
        "an edit takes an id from the high water"
    );
    assert_eq!(engine.allocator_high_water(), N, "neither took an id");

    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), N);
    engine.declare_shards(N + 2).expect("the size is raised");
    engine
        .ingest_rows(rows("new", 1), "new".into(), [1; 32])
        .expect("the insert goes in");
    engine
        .ingest_rows(vec![moved], "edit".into(), [2; 32])
        .expect("the edit goes in");
    assert_eq!(engine.allocator_high_water(), N + 2);
}

/// A build past its size writes a bundle of several shards, and the engine does not open one.
#[test]
fn a_bundle_of_several_shards_is_refused_at_open() {
    let tmp = built(Some(N / 2 - 1));
    let opened = Engine::open(
        &tmp.path().join("bundle"),
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        config(),
    );
    assert!(
        matches!(opened, Err(mosaica_engine::EngineError::SeveralShards { shards: 3 })),
        "{:?}",
        opened.err()
    );
}
