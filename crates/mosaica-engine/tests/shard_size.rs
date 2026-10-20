//! `[shards] size`, declared at a build or at a running service, and where the inserts and edits
//! past it go.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use common::*;
use mosaica_engine::{AcceptError, Engine};
use mosaica_lifecycle::faults::{FaultSwitchboard, PauseAction, PauseSite};
use mosaica_lifecycle::wal::WalRecord;
use mosaica_lifecycle::{ExecError, UnallocatedRow};
use mosaica_types::{
    EntityId, ItemAt, ShardId, ShardSizeError, ShardState, ShardsExhausted, SHARD_SIZE_DEFAULT,
};

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

/// The item the test calls `key` holds, in its shard.
fn item_at(engine: &Engine, key: &str) -> Option<ItemAt> {
    items_by_id(engine, [key_id(key)])[0]
}

fn shard(id: u32) -> ShardId {
    ShardId::new(id).unwrap()
}

/// Queues the batches `batches`, each `(name, items)`, behind a window parked after its fsync
/// that takes one id, and answers each batch in the order given.
fn queued_behind_a_parked_window(
    engine: &Arc<Engine>,
    faults: &FaultSwitchboard,
    batches: &[(&'static str, usize)],
) -> Vec<Result<Vec<EntityId>, AcceptError>> {
    faults.arm_pause(PauseSite::AfterFsync, PauseAction::Stall);
    let primer = Arc::clone(engine);
    let prime =
        std::thread::spawn(move || primer.ingest_rows(rows("prime", 1), "prime".into(), [9; 32]));
    faults.await_arrivals(PauseSite::AfterFsync, 1, WAIT);
    // Already counts every write before and the priming batch.
    let queued = engine.write_executor_stats().work_submitted;

    let results = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for (i, &(batch, n)) in batches.iter().enumerate() {
        let e = Arc::clone(engine);
        let out = Arc::clone(&results);
        handles.push(std::thread::spawn(move || {
            let answer = e.ingest_rows(rows(batch, n), batch.to_string(), [i as u8; 32]);
            out.lock().unwrap().push((i, answer));
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
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, answer)| answer).collect()
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

/// The size declarations, openings and seals the log at `tmp` keeps, in order.
fn shard_records(tmp: &Path) -> Vec<WalRecord> {
    mosaica_lifecycle::wal::Wal::open(tmp.join("wal.log"))
        .unwrap()
        .records()
        .map(|record| record.unwrap().1)
        .filter(|record| {
            matches!(
                record,
                WalRecord::ShardsDeclare { .. }
                    | WalRecord::ShardOpen { .. }
                    | WalRecord::ShardSeal { .. }
            )
        })
        .collect()
}

/// A size at or below the open shard's high water seals it in the declaration's own append. The
/// next insert opens shard 1, a restart keeps shard 0 sealed, and so does a size raised past its
/// high water.
#[test]
fn a_lowered_size_seals_the_open_shard() {
    for lowered in [N - 1, N] {
        let tmp = built(None);
        let (engine, _faults) = engine_over(tmp.path());
        assert_eq!(size_refused(engine.declare_shards(0)), ShardSizeError::Zero);
        assert!(
            !engine
                .declare_shards(lowered)
                .expect("the size is declared"),
            "a size of {lowered} is new"
        );
        let seal = WalRecord::ShardSeal {
            shard: ShardId::ZERO,
        };
        let declared = vec![WalRecord::ShardsDeclare { size: lowered }, seal.clone()];
        assert_eq!(shard_records(tmp.path()), declared);
        assert!(engine.declare_shards(lowered).expect("the same size again"));

        let engine = restart(tmp.path(), engine);
        assert_eq!(engine.shard_size(), lowered);
        engine
            .ingest_rows(rows("new", 1), "new".into(), [1; 32])
            .expect("the insert goes in");
        assert_eq!(
            item_at(&engine, "new-0"),
            Some(ItemAt::new(shard(1), EntityId::new(0)))
        );

        let raised = N + 10;
        assert!(!engine.declare_shards(raised).expect("the size is raised"));
        engine
            .ingest_rows(rows("later", 1), "later".into(), [2; 32])
            .expect("the insert goes in");
        assert_eq!(
            item_at(&engine, "later-0"),
            Some(ItemAt::new(shard(1), EntityId::new(1)))
        );
        let mut logged = declared;
        logged.extend([
            WalRecord::ShardOpen { shard: shard(1) },
            WalRecord::ShardsDeclare { size: raised },
        ]);
        assert_eq!(shard_records(tmp.path()), logged, "shard 0 is sealed once");

        let engine = restart(tmp.path(), engine);
        publish_buffered(&engine);
        let (shards, next_id) = recorded(&side_manifest(&tmp.path().join("bundle")));
        let states: Vec<_> = shards
            .iter()
            .map(|(id, state, high_water, ..)| (*id, *state, *high_water))
            .collect();
        assert_eq!(
            (states, next_id),
            (
                vec![(0, ShardState::Sealed, N), (1, ShardState::Open, 2)],
                2
            ),
            "after a restart and a raised size, at {lowered}"
        );
    }
}

/// Three batches queue behind a parked window and go in together: `a` fits the open shard, `b`
/// takes it to the size and opens shard 1 for the rest, and `c` follows `b` there.
#[test]
fn a_batch_past_the_size_straddles_into_a_new_shard() {
    let tmp = built(None);
    let (engine, faults) = engine_over(tmp.path());
    let size = N + 4;
    engine.declare_shards(size).expect("the size is declared");
    let engine = Arc::new(engine);

    let answers = queued_behind_a_parked_window(&engine, &faults, &[("a", 2), ("b", 3), ("c", 1)]);
    let created: Vec<usize> = answers
        .into_iter()
        .map(|answer| answer.expect("nothing is refused").len())
        .collect();
    assert_eq!(created, [2, 3, 1]);
    let placed = |engine: &Engine| -> Vec<(u32, u64)> {
        ["a-0", "a-1", "b-0", "b-1", "b-2", "c-0"]
            .iter()
            .map(|key| {
                let at = item_at(engine, key).expect("the item is held");
                (at.shard.raw(), at.entity.raw())
            })
            .collect()
    };
    let expected = vec![(0, N + 1), (0, N + 2), (0, N + 3), (1, 0), (1, 1), (1, 2)];
    assert_eq!(placed(&engine), expected);
    assert_eq!(
        engine.allocator_high_water(),
        size + 3,
        "the shards' high waters, summed"
    );

    let engine = restart(tmp.path(), Arc::try_unwrap(engine).ok().unwrap());
    assert_eq!(placed(&engine), expected);
    assert_eq!(engine.allocator_high_water(), size + 3);
}

/// Three batches queue behind a parked window when one shard number is left, the log having opened
/// and sealed the one before it: `a` fits, `b` needs a shard past the last number and is refused
/// alone, creating nothing, and `c` fits after `a`.
#[test]
fn a_batch_needing_a_shard_past_the_last_number_is_refused_alone() {
    let tmp = built(None);
    let before_last = ShardId::new(ShardId::LIMIT - 2).unwrap();
    let opened = [
        WalRecord::ShardOpen { shard: before_last },
        WalRecord::ShardSeal { shard: before_last },
    ];
    let mut wal = mosaica_lifecycle::wal::Wal::open(tmp.path().join("wal.log")).unwrap();
    for record in &opened {
        wal.append(record).unwrap();
    }
    wal.fsync().unwrap();
    drop(wal);
    let (engine, faults) = engine_over(tmp.path());
    let size = N + 4;
    engine.declare_shards(size).expect("the size is declared");
    let engine = Arc::new(engine);

    // `b` takes the last id of shard 0, all of the last shard, and needs one more.
    let past = 1 + size as usize + 1;
    let mut answers =
        queued_behind_a_parked_window(&engine, &faults, &[("a", 2), ("b", past), ("c", 1)])
            .into_iter();
    assert_eq!(answers.next().unwrap().expect("a fits").len(), 2);
    let refused = answers.next().unwrap();
    assert!(
        matches!(
            refused,
            Err(AcceptError::Exec(ExecError::ShardsExhausted(
                ShardsExhausted { needed: 1 }
            )))
        ),
        "{refused:?}"
    );
    assert_eq!(answers.next().unwrap().expect("c fits after a").len(), 1);
    assert!(
        (0..past).all(|i| item_at(&engine, &format!("b-{i}")).is_none()),
        "the refused batch created nothing"
    );
    assert_eq!(
        item_at(&engine, "c-0").map(|at| (at.shard, at.entity.raw())),
        Some((shard(0), N + 3))
    );
    assert_eq!(engine.allocator_high_water(), size);
    assert_eq!(
        shard_records(tmp.path()),
        [
            opened[0].clone(),
            opened[1].clone(),
            WalRecord::ShardsDeclare { size },
            WalRecord::ShardSeal {
                shard: ShardId::ZERO
            }
        ],
        "no shard opened, and a and c, filling shard 0, sealed it"
    );
}

/// A build exactly at its size writes its shard sealed. It takes an insert, which opens shard 1,
/// and an edit of a built item, which stays in shard 0 and takes the first id past the size; a
/// restart keeps both.
#[test]
fn a_build_at_its_size_writes_it_sealed_and_takes_an_insert_and_an_edit_past_it() {
    let tmp = built(Some(N));
    assert_eq!(
        recorded(&side_manifest(&tmp.path().join("bundle"))),
        (vec![(0, ShardState::Sealed, N, vec![], 0)], 1)
    );
    let (engine, _faults) = engine_over(tmp.path());
    assert_eq!(engine.shard_size(), N);
    assert_eq!(shards_of(&tmp.path().join("bundle")).0, N);
    assert_eq!(engine.allocator_high_water(), N);

    engine
        .ingest_rows(rows("new", 1), "new".into(), [1; 32])
        .expect("the insert goes in");
    edit_by_id(&engine, "edit", 1).expect("the edit goes in");
    let placed = |engine: &Engine| {
        (
            item_at(engine, "new-0").map(|at| (at.shard, at.entity.raw())),
            items_by_id(engine, [1])[0].map(|at| (at.shard, at.entity.raw())),
        )
    };
    let expected = (Some((shard(1), 0)), Some((ShardId::ZERO, N)));
    assert_eq!(placed(&engine), expected);
    assert_eq!(engine.allocator_high_water(), N + 2);
    assert_eq!(
        shard_records(tmp.path()),
        [WalRecord::ShardOpen { shard: shard(1) }],
        "the insert opens shard 1 and seals nothing"
    );

    let engine = restart(tmp.path(), engine);
    assert_eq!(placed(&engine), expected);
    assert_eq!(engine.allocator_high_water(), N + 2);
    assert_eq!(engine.shard_size(), N);
}
