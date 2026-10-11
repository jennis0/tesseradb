//! **The point shard's size**, declared at a build or at a running service: kept across a restart,
//! a fold and both, and the high water an insert may not take the shard past. A batch that would is
//! refused alone, the other batches of its commit window go in, and once the size is raised it goes
//! in too. An edit takes an id as a new item does.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use common::*;
use mosaica_engine::{AcceptError, Engine};
use mosaica_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};
use mosaica_lifecycle::{ExecError, UnallocatedRow};
use mosaica_store::read::open_bundle;
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

/// An engine over the fixture with its executor running and the switchboard on it.
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

/// The same bundle and log reopened. The old engine is dropped first so its executor releases the
/// log.
fn restart(tmp: &Path, engine: Engine) -> Engine {
    drop(engine);
    engine_over(tmp).0
}

/// The size the served side-manifest records.
fn on_disc(tmp: &Path) -> Vec<u64> {
    let bundle = open_bundle(&tmp.join("bundle")).expect("the bundle opens");
    bundle.partitions.values().map(|p| p.shard_size).collect()
}

/// A new item at the origin holding the `id` of `key`.
fn row(key: &str) -> UnallocatedRow {
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 0.0,
        y: 0.0,
        scalars: keyed(key),
        terms: Vec::new(),
        scoped: Vec::new(),
    }
}

/// `n` new items, holding the `id`s `{prefix}-0` onwards.
fn rows(prefix: &str, n: usize) -> Vec<UnallocatedRow> {
    (0..n).map(|i| row(&format!("{prefix}-{i}"))).collect()
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

/// **A size declared at a running service is kept**: from the log alone, from the side-manifest a
/// publication writes, across a fold, and across a restart after the fold.
#[test]
fn a_size_declared_at_a_running_service_survives_a_restart_a_fold_and_both() {
    let tmp = built(None);
    let (engine, _faults) = engine_over(tmp.path());
    assert_eq!(engine.shard_size(), SHARD_SIZE_DEFAULT);
    assert_eq!(on_disc(tmp.path()), [SHARD_SIZE_DEFAULT]);

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
    assert_eq!(on_disc(tmp.path()), [size], "the publication restates it");
    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), size, "read from the side-manifest");

    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");
    assert_eq!(
        on_disc(tmp.path()),
        [size],
        "the fold's side-manifest carries it"
    );
    assert_eq!(engine.shard_size(), size);
    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), size, "after a fold and a restart");
}

/// **A size of zero and one below the high water are refused**, and leave the size as it was.
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
        "a full shard is a size"
    );
    let engine = restart(tmp.path(), engine);
    assert_eq!(engine.shard_size(), N);
}

/// **A batch past the size is refused alone.** Three batches are queued behind a parked window:
/// the first fits, the second does not fit beside it nor after it, and the third fits after the
/// first. The second changes nothing, and once the size is raised the same batch goes in as a new
/// one.
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
            engine.write_executor_stats().work_submitted >= i as u64 + 2
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
        "b is measured against the shard a's window left"
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

/// **A build at a size refuses an insert past it**, as a running service does, and an edit takes
/// an id as a new item does. A restart keeps the build's size.
#[test]
fn a_build_at_its_size_refuses_an_insert_and_an_edit_past_it() {
    let tmp = built(Some(N));
    let (engine, _faults) = engine_over(tmp.path());
    assert_eq!(engine.shard_size(), N);
    assert_eq!(on_disc(tmp.path()), [N]);
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
        ..row("unused")
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
